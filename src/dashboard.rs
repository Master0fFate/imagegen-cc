use crate::backend::Gen;
use crate::catalog::CATALOG;
use crate::config::{Backend, Config, Kind};
use crate::download::{Report, client};
use crate::{engine, models, secrets};
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::OnceCell;

const PAGE: &str = include_str!("../web/index.html");

#[derive(Default)]
struct Job {
    lines: Vec<String>,
    done: bool,
    error: Option<String>,
    result: Option<Value>,
}

struct App {
    token: String,
    port: u16,
    jobs: Mutex<HashMap<u64, Arc<Mutex<Job>>>>,
    next: AtomicU64,
    seen: Mutex<Instant>,
}

type S = State<Arc<App>>;
type ApiResult = Result<Json<Value>, (StatusCode, String)>;

static URL: OnceCell<String> = OnceCell::const_new();

fn bad(e: impl Into<String>) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, e.into())
}

pub async fn start() -> Result<String, String> {
    URL.get_or_try_init(|| async {
        let mut raw = [0u8; 24];
        getrandom::fill(&mut raw).map_err(|e| e.to_string())?;
        let token = crate::download::hex(&raw);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.map_err(|e| e.to_string())?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let app = Arc::new(App { token: token.clone(), port, jobs: Mutex::new(HashMap::new()), next: AtomicU64::new(1), seen: Mutex::new(Instant::now()) });
        let router = Router::new()
            .route("/", get(page))
            .route("/api/state", get(state))
            .route("/api/job/{id}", get(job))
            .route("/api/engine", post(engine_install))
            .route("/api/local", post(local_add))
            .route("/api/api", post(api_add))
            .route("/api/comfyui/models", get(comfy_models))
            .route("/api/comfyui", post(comfy_add))
            .route("/api/test", post(test))
            .route("/api/default", post(set_default))
            .route("/api/remove", post(remove))
            .route("/api/settings", post(settings))
            .route("/api/hf-token", post(hf_token))
            .route("/api/uninstall", post(uninstall))
            .route("/api/recent", get(recent))
            .route("/api/img", get(img))
            .layer(middleware::from_fn_with_state(app.clone(), guard))
            .with_state(app.clone());
        let idle = app.clone();
        tokio::spawn(async move {
            let server = axum::serve(listener, router);
            let watchdog = async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    if idle.seen.lock().map(|t| t.elapsed() > Duration::from_secs(3600)).unwrap_or(false) {
                        return;
                    }
                }
            };
            tokio::select! { _ = server => {}, () = watchdog => {} }
        });
        Ok::<String, String>(format!("http://127.0.0.1:{port}/?t={token}"))
    })
    .await
    .cloned()
}

async fn guard(State(app): S, req: Request, next: Next) -> Response {
    let h = req.headers();
    let host_ok = h.get(header::HOST).and_then(|v| v.to_str().ok()).is_some_and(|v| v == format!("127.0.0.1:{}", app.port) || v == format!("localhost:{}", app.port));
    if !host_ok {
        return (StatusCode::FORBIDDEN, "bad host").into_response();
    }
    if req.uri().path().starts_with("/api/") {
        let query_token = (req.uri().path() == "/api/img")
            .then(|| req.uri().query().and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("t="))))
            .flatten();
        let token_ok = h.get("x-token").and_then(|v| v.to_str().ok()).or(query_token).is_some_and(|v| v == app.token);
        let origin_ok = h.get(header::ORIGIN).and_then(|v| v.to_str().ok()).is_none_or(|o| o == format!("http://127.0.0.1:{}", app.port) || o == format!("http://localhost:{}", app.port));
        if !token_ok || !origin_ok {
            return (StatusCode::FORBIDDEN, "bad token").into_response();
        }
    }
    if let Ok(mut t) = app.seen.lock() {
        *t = Instant::now();
    }
    let mut res = next.run(req).await;
    let hd = res.headers_mut();
    hd.insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    hd.insert("x-frame-options", header::HeaderValue::from_static("DENY"));
    hd.insert("referrer-policy", header::HeaderValue::from_static("no-referrer"));
    hd.insert(header::CONTENT_SECURITY_POLICY, header::HeaderValue::from_static("default-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src 'self' data:; connect-src 'self'"));
    res
}

async fn page() -> Html<&'static str> {
    Html(PAGE)
}

fn spawn_job<F, Fut>(app: &Arc<App>, f: F) -> Json<Value>
where
    F: FnOnce(Report) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<Value, String>> + Send + 'static,
{
    let id = app.next.fetch_add(1, Ordering::Relaxed);
    let j = Arc::new(Mutex::new(Job::default()));
    if let Ok(mut m) = app.jobs.lock() {
        m.insert(id, j.clone());
    }
    let jr = j.clone();
    let report: Report = Arc::new(move |line: &str| {
        if let Ok(mut g) = jr.lock() {
            let same = |a: &str, b: &str| a.split_once(':').map(|x| x.0) == b.split_once(':').map(|x| x.0) && b.contains(" MB/s");
            if g.lines.last().is_some_and(|l| same(l, line)) {
                g.lines.pop();
            }
            g.lines.push(line.to_owned());
            let n = g.lines.len();
            if n > 300 {
                g.lines.drain(..n - 300);
            }
        }
    });
    tokio::spawn(async move {
        let r = f(report).await;
        if let Ok(mut g) = j.lock() {
            g.done = true;
            match r {
                Ok(v) => g.result = Some(v),
                Err(e) => g.error = Some(e),
            }
        }
    });
    Json(json!({ "job": id }))
}

async fn job(State(app): S, Path(id): Path<u64>) -> ApiResult {
    let j = app.jobs.lock().ok().and_then(|m| m.get(&id).cloned()).ok_or_else(|| bad("no such job"))?;
    let g = j.lock().map_err(|e| bad(e.to_string()))?;
    Ok(Json(json!({ "lines": g.lines, "done": g.done, "error": g.error, "result": g.result })))
}

fn backend_view(name: &str, b: &Backend) -> Value {
    let key = match secrets::has(name) {
        Some(secrets::Store::Keychain) => "keychain",
        Some(secrets::Store::File) => "file",
        None => "none",
    };
    let recipe = b.recipe.as_deref().and_then(|r| CATALOG.recipe(r));
    json!({
        "name": name,
        "kind": b.kind,
        "base_url": b.base_url,
        "model": b.model,
        "recipe": recipe.map(|r| &r.name),
        "variant": b.variant,
        "engine": b.engine,
        "files": b.files,
        "workflow": b.workflow,
        "api_key": key,
        "edit": recipe.is_some_and(|r| r.edit) || matches!(b.kind, Kind::Openai | Kind::Comfyui),
    })
}

async fn state() -> ApiResult {
    let cfg = Config::load().map_err(bad)?;
    let data = crate::paths::data_dir();
    let hw = tokio::task::spawn_blocking(move || crate::hw::detect(&data)).await.map_err(|e| bad(e.to_string()))?;
    let (vram, ram) = (hw.usable_vram_gb(), hw.ram_gb());
    let recipes: Vec<Value> = CATALOG
        .recipes
        .iter()
        .map(|r| {
            let best = r.best_fit(vram, ram).map(|v| v.id.clone());
            json!({
                "id": r.id, "name": r.name, "about": r.about, "license": r.license, "edit": r.edit, "best": best,
                "variants": r.variants.iter().map(|v| json!({ "id": v.id, "label": v.label, "min_vram_gb": v.min_vram_gb, "bytes": v.download_bytes, "fits": v.min_vram_gb <= vram && v.min_ram_gb <= ram })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let engine = engine::installed_any();
    let prefs: Vec<&str> = engine::preference(&hw).into_iter().map(engine::EngineKind::name).collect();
    Ok(Json(json!({
        "hw": hw, "vram_gb": vram, "ram_gb": ram, "engine": engine, "engine_order": prefs,
        "recipes": recipes,
        "backends": cfg.backends.iter().map(|(n, b)| backend_view(n, b)).collect::<Vec<_>>(),
        "default_backend": cfg.default_backend,
        "output_dir": cfg.output_dir(),
        "data_dir": crate::paths::data_dir(),
        "hf_token": secrets::has("huggingface").is_some(),
        "version": env!("CARGO_PKG_VERSION"),
    })))
}

#[derive(Deserialize)]
struct EngineReq {
    kind: Option<String>,
}

async fn engine_install(State(app): S, Json(r): Json<EngineReq>) -> ApiResult {
    let kind = match r.kind.as_deref() {
        None | Some("auto" | "") => None,
        Some(k) => Some(engine::EngineKind::parse(k).ok_or_else(|| bad("unknown engine"))?),
    };
    Ok(spawn_job(&app, move |rep| async move {
        let i = match kind {
            Some(k) => engine::install(k, &rep).await?,
            None => engine::install_auto(&crate::hw::detect(&crate::paths::data_dir()), &rep).await?,
        };
        Ok(json!(i))
    }))
}

#[derive(Deserialize)]
struct LocalReq {
    name: String,
    recipe: String,
    variant: Option<String>,
    #[serde(default)]
    search: Vec<String>,
    #[serde(default)]
    links: BTreeMap<String, String>,
}

fn clean_name(n: &str) -> Result<String, (StatusCode, String)> {
    let n = n.trim();
    if n.is_empty() || n.len() > 40 || !n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(bad("Name: 1-40 letters, digits, - or _"));
    }
    Ok(n.to_owned())
}

async fn local_add(State(app): S, Json(r): Json<LocalReq>) -> ApiResult {
    let name = clean_name(&r.name)?;
    let o = models::AddLocal {
        name,
        recipe: r.recipe,
        variant: r.variant.filter(|v| !v.is_empty()),
        links: r.links.into_iter().filter(|(_, v)| !v.trim().is_empty()).map(|(k, v)| (k, PathBuf::from(v.trim()))).collect(),
        search: r.search.into_iter().filter(|s| !s.trim().is_empty()).map(|s| PathBuf::from(s.trim())).collect(),
        engine: None,
    };
    Ok(spawn_job(&app, move |rep| async move { models::add_local(o, &rep).await.map(|m| json!({ "message": m })) }))
}

#[derive(Deserialize)]
struct ApiReq {
    name: String,
    kind: Option<String>,
    base_url: String,
    model: Option<String>,
    auth_header: Option<String>,
    auth_prefix: Option<String>,
    extra: Option<String>,
    key: Option<String>,
}

async fn api_add(Json(r): Json<ApiReq>) -> ApiResult {
    let name = clean_name(&r.name)?;
    let url = r.base_url.trim();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(bad("Base URL must start with http:// or https://"));
    }
    let extra: Map<String, Value> = match r.extra.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => serde_json::from_str(s).map_err(|e| bad(format!("Extra parameters must be a JSON object: {e}")))?,
        None => Map::new(),
    };
    let mut cfg = Config::load().map_err(bad)?;
    let b = Backend {
        kind: if r.kind.as_deref() == Some("sdcpp") { Kind::Sdcpp } else { Kind::Openai },
        base_url: url.to_owned(),
        model: r.model.map(|m| m.trim().to_owned()).filter(|m| !m.is_empty()),
        auth_header: r.auth_header.map(|s| s.trim().to_owned()).unwrap_or_else(|| "Authorization".into()),
        auth_prefix: r.auth_prefix.unwrap_or_else(|| "Bearer ".into()),
        extra,
        ..Backend::default()
    };
    cfg.backends.insert(name.clone(), b);
    if cfg.default_backend.is_none() {
        cfg.default_backend = Some(name.clone());
    }
    cfg.save().map_err(bad)?;
    let store = match r.key.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
        Some(k) => Some(secrets::set(&name, k).map_err(bad)?),
        None => None,
    };
    Ok(Json(json!({ "ok": true, "key_store": store.map(|s| if s == secrets::Store::Keychain { "keychain" } else { "file" }) })))
}

#[derive(Deserialize)]
struct UrlQ {
    url: String,
}

async fn comfy_models(Query(q): Query<UrlQ>) -> ApiResult {
    let base = q.url.trim().trim_end_matches('/');
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        return Err(bad("URL must start with http://"));
    }
    let v: Value = client()
        .get(format!("{base}/object_info"))
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| bad(format!("ComfyUI is not reachable at {base}: {e}")))?
        .json()
        .await
        .map_err(|e| bad(format!("Not a ComfyUI server: {e}")))?;
    let list = |node: &str, input: &str| v.pointer(&format!("/{node}/input/required/{input}/0")).cloned().unwrap_or(json!([]));
    Ok(Json(json!({
        "unet": list("UNETLoader", "unet_name"),
        "clip": list("CLIPLoader", "clip_name"),
        "clip_type": list("CLIPLoader", "type"),
        "vae": list("VAELoader", "vae_name"),
        "qwen21": v.get("TextEncodeQwenImage21").is_some() && v.get("QwenImage21Cache").is_some(),
    })))
}

#[derive(Deserialize)]
struct ComfyReq {
    name: String,
    base_url: String,
    workflow_path: Option<String>,
    unet: Option<String>,
    clip: Option<String>,
    vae: Option<String>,
    steps: Option<f64>,
    guidance: Option<f64>,
}

fn qwen_template(unet: &str, clip: &str, vae: &str) -> Value {
    json!({
        "1": { "class_type": "UNETLoader", "inputs": { "unet_name": unet, "weight_dtype": "default" } },
        "2": { "class_type": "CLIPLoader", "inputs": { "clip_name": clip, "type": "qwen_image", "device": "default" } },
        "3": { "class_type": "VAELoader", "inputs": { "vae_name": vae } },
        "4": { "class_type": "ModelSamplingFlux", "inputs": { "model": ["1", 0], "max_shift": 0.69, "base_shift": 0.5, "width": "{{width}}", "height": "{{height}}" } },
        "5": { "class_type": "QwenImage21Cache", "inputs": { "model": ["4", 0], "device": "auto", "dtype": "default" } },
        "6": { "class_type": "TextEncodeQwenImage21", "inputs": { "clip": ["2", 0], "prompt": "{{prompt}}", "negative_prompt": "{{negative_prompt}}", "resolution": 0, "vae": ["3", 0] } },
        "7": { "class_type": "EmptyLatentImage", "inputs": { "width": "{{width}}", "height": "{{height}}", "batch_size": "{{count}}" } },
        "8": { "class_type": "KSampler", "inputs": { "model": ["5", 0], "seed": "{{seed}}", "steps": "{{steps}}", "cfg": "{{guidance}}", "sampler_name": "er_sde", "scheduler": "beta", "positive": ["6", 0], "negative": ["6", 1], "latent_image": ["7", 0], "denoise": 1 } },
        "9": { "class_type": "VAEDecode", "inputs": { "samples": ["8", 0], "vae": ["3", 0] } },
        "10": { "class_type": "SaveImage", "inputs": { "images": ["9", 0], "filename_prefix": "imagegen-cc" } }
    })
}

async fn comfy_add(Json(r): Json<ComfyReq>) -> ApiResult {
    let name = clean_name(&r.name)?;
    let wf = match r.workflow_path.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(p) => {
            let p = PathBuf::from(p);
            let text = std::fs::read_to_string(&p).map_err(|e| bad(format!("{}: {e}", p.display())))?;
            let v: Value = serde_json::from_str(&text).map_err(|e| bad(format!("Workflow is not JSON: {e}")))?;
            if v.get("nodes").is_some() {
                return Err(bad("This workflow is in UI format. In ComfyUI use Workflow > Export (API)"));
            }
            if !text.contains("{{prompt}}") {
                return Err(bad("The workflow has no {{prompt}} placeholder. Put {{prompt}} in the positive prompt text"));
            }
            p
        }
        None => {
            let (Some(u), Some(c), Some(v)) = (r.unet.as_deref(), r.clip.as_deref(), r.vae.as_deref()) else {
                return Err(bad("Pick the model, text encoder and VAE, or give an API workflow file"));
            };
            let p = crate::paths::data_dir().join("comfyui").join(format!("{name}.api.json"));
            let bytes = serde_json::to_vec_pretty(&qwen_template(u, c, v)).map_err(|e| bad(e.to_string()))?;
            crate::paths::write_atomic(&p, &bytes).map_err(|e| bad(e.to_string()))?;
            p
        }
    };
    let mut extra = Map::new();
    extra.insert("steps".into(), json!(r.steps.unwrap_or(30.0)));
    extra.insert("guidance".into(), json!(r.guidance.unwrap_or(3.5)));
    let mut cfg = Config::load().map_err(bad)?;
    cfg.backends.insert(name.clone(), Backend { kind: Kind::Comfyui, base_url: r.base_url.trim().trim_end_matches('/').to_owned(), workflow: Some(wf), extra, ..Backend::default() });
    if cfg.default_backend.is_none() {
        cfg.default_backend = Some(name);
    }
    cfg.save().map_err(bad)?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct NameReq {
    name: String,
}

async fn test(State(app): S, Json(r): Json<NameReq>) -> ApiResult {
    let cfg = Config::load().map_err(bad)?;
    let (name, b) = cfg.resolve(Some(&r.name)).map_err(bad)?;
    let b = b.clone();
    Ok(spawn_job(&app, move |rep| async move {
        let fast = matches!(b.kind, Kind::Local | Kind::Sdcpp);
        let g = Gen {
            prompt: "A red apple on a white table, soft window light, studio photo".into(),
            negative_prompt: None,
            width: Some(if b.kind == Kind::Openai { 1024 } else { 512 }),
            height: Some(if b.kind == Kind::Openai { 1024 } else { 512 }),
            count: 1,
            seed: Some(7),
            steps: fast.then_some(12),
            guidance: None,
            params: Map::new(),
        };
        let t = Instant::now();
        rep("Sending a test request");
        let out = crate::mcp::run_backend(&name, &b, &g, &[], None, &rep).await?;
        let img = out.images.first().ok_or("no image")?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(img);
        let mime = match crate::output::ext_of(img) {
            "jpg" => "image/jpeg",
            "webp" => "image/webp",
            _ => "image/png",
        };
        Ok(json!({ "ms": t.elapsed().as_millis(), "image": format!("data:{mime};base64,{b64}") }))
    }))
}

async fn set_default(Json(r): Json<NameReq>) -> ApiResult {
    let mut cfg = Config::load().map_err(bad)?;
    if !cfg.backends.contains_key(&r.name) {
        return Err(bad("unknown backend"));
    }
    cfg.default_backend = Some(r.name);
    cfg.save().map_err(bad)?;
    Ok(Json(json!({ "ok": true })))
}

async fn remove(Json(r): Json<NameReq>) -> ApiResult {
    let mut cfg = Config::load().map_err(bad)?;
    if cfg.backends.get(&r.name).is_some_and(|b| b.kind == Kind::Local) {
        engine::MANAGER.stop().await;
    }
    cfg.backends.remove(&r.name);
    if cfg.default_backend.as_deref() == Some(r.name.as_str()) {
        cfg.default_backend = cfg.backends.keys().next().cloned();
    }
    cfg.save().map_err(bad)?;
    secrets::delete(&r.name).map_err(bad)?;
    crate::prune_models(&cfg);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct SettingsReq {
    output_dir: Option<String>,
}

async fn settings(Json(r): Json<SettingsReq>) -> ApiResult {
    let mut cfg = Config::load().map_err(bad)?;
    cfg.output_dir = r.output_dir.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).map(PathBuf::from);
    cfg.save().map_err(bad)?;
    Ok(Json(json!({ "ok": true, "output_dir": cfg.output_dir() })))
}

#[derive(Deserialize)]
struct TokenReq {
    token: String,
}

async fn hf_token(Json(r): Json<TokenReq>) -> ApiResult {
    let t = r.token.trim();
    if t.is_empty() {
        secrets::delete("huggingface").map_err(bad)?;
    } else {
        secrets::set("huggingface", t).map_err(bad)?;
    }
    Ok(Json(json!({ "ok": true })))
}

async fn uninstall(headers: HeaderMap) -> ApiResult {
    if headers.get("x-confirm").and_then(|v| v.to_str().ok()) != Some("uninstall") {
        return Err(bad("missing confirmation"));
    }
    engine::MANAGER.stop().await;
    secrets::purge();
    for d in ["engines", "models", "comfyui", "logs"] {
        let _ = tokio::fs::remove_dir_all(crate::paths::data_dir().join(d)).await;
    }
    let _ = tokio::fs::remove_file(Config::path()).await;
    let mut cmd = tokio::process::Command::new("claude");
    cmd.args(["plugin", "uninstall", "imagegen-cc"]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    let ran = cmd.spawn().is_ok();
    Ok(Json(json!({ "ok": true, "plugin_uninstall_started": ran })))
}

fn is_image(p: &std::path::Path) -> bool {
    matches!(p.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref(), Some("png" | "jpg" | "jpeg" | "webp"))
}

async fn recent() -> ApiResult {
    let root = Config::load().map_err(bad)?.output_dir();
    let list = tokio::task::spawn_blocking(move || {
        let mut found: Vec<(std::time::SystemTime, String)> = Vec::new();
        let mut stack = vec![(root.clone(), 0u8)];
        while let Some((d, depth)) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    if depth < 1 {
                        stack.push((p, depth + 1));
                    }
                } else if is_image(&p)
                    && let (Ok(m), Ok(rel)) = (e.metadata().and_then(|m| m.modified()), p.strip_prefix(&root)) {
                        found.push((m, rel.to_string_lossy().replace('\\', "/")));
                    }
            }
        }
        found.sort_unstable_by_key(|x| std::cmp::Reverse(x.0));
        found.into_iter().take(8).map(|(_, r)| r).collect::<Vec<_>>()
    })
    .await
    .map_err(|e| bad(e.to_string()))?;
    Ok(Json(json!({ "files": list })))
}

#[derive(Deserialize)]
struct ImgQ {
    f: String,
}

async fn img(Query(q): Query<ImgQ>) -> Result<Response, (StatusCode, String)> {
    let root = Config::load().map_err(bad)?.output_dir();
    let rel = std::path::Path::new(&q.f);
    if rel.is_absolute() || rel.components().any(|c| !matches!(c, std::path::Component::Normal(_))) || !is_image(rel) {
        return Err((StatusCode::FORBIDDEN, "bad path".into()));
    }
    let root_c = tokio::fs::canonicalize(&root).await.map_err(|_| (StatusCode::NOT_FOUND, "no output folder".into()))?;
    let full = tokio::fs::canonicalize(root.join(rel)).await.map_err(|_| (StatusCode::NOT_FOUND, "not found".into()))?;
    if !full.starts_with(&root_c) {
        return Err((StatusCode::FORBIDDEN, "bad path".into()));
    }
    let bytes = tokio::fs::read(&full).await.map_err(|_| (StatusCode::NOT_FOUND, "not found".into()))?;
    let mime = match crate::output::ext_of(&bytes) {
        "jpg" => "image/jpeg",
        "webp" => "image/webp",
        "png" => "image/png",
        _ => return Err((StatusCode::FORBIDDEN, "not an image".into())),
    };
    Ok(([(header::CONTENT_TYPE, mime)], bytes).into_response())
}

pub fn open_browser(url: &str) -> bool {
    let mut c = if cfg!(windows) {
        let mut c = std::process::Command::new("rundll32");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    } else if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    } else {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    c.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().is_ok()
}
