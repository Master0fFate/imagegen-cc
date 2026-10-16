use crate::config::{Backend, Kind};
use base64::Engine;
use serde_json::{Map, Value, json};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

pub struct Gen {
    pub prompt: String,
    pub negative_prompt: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub count: u32,
    pub seed: Option<i64>,
    pub steps: Option<u32>,
    pub guidance: Option<f32>,
    pub params: Map<String, Value>,
}

pub struct Output {
    pub images: Vec<Vec<u8>>,
    pub notes: Vec<String>,
}

const MAX_IMAGE: usize = 128 << 20;

fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::custom(|a| {
                let same = a.previous().last().is_some_and(|p| p.host_str() == a.url().host_str() && p.port_or_known_default() == a.url().port_or_known_default());
                if same && a.previous().len() < 5 { a.follow() } else { a.stop() }
            }))
            .user_agent(concat!("imagegen-cc/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default()
    })
}

fn is_image_bytes(b: &[u8]) -> bool {
    matches!(crate::output::ext_of(b), "png" | "jpg" | "webp" | "gif")
}

async fn read_capped(mut r: reqwest::Response, what: &str) -> Result<Vec<u8>, String> {
    if r.content_length().is_some_and(|n| n > MAX_IMAGE as u64) {
        return Err(format!("{what}: response larger than {} MB", MAX_IMAGE >> 20));
    }
    let mut out = Vec::new();
    while let Some(c) = r.chunk().await.map_err(|e| format!("{what}: {e}"))? {
        if out.len() + c.len() > MAX_IMAGE {
            return Err(format!("{what}: response larger than {} MB", MAX_IMAGE >> 20));
        }
        out.extend_from_slice(&c);
    }
    Ok(out)
}

fn endpoint(b: &Backend, path: &str) -> String {
    format!("{}/{}", b.base_url.trim_end_matches('/'), path)
}

fn auth(req: reqwest::RequestBuilder, name: &str, b: &Backend) -> reqwest::RequestBuilder {
    match crate::secrets::get(name) {
        Some(key) if !b.auth_header.is_empty() => req.header(b.auth_header.as_str(), format!("{}{key}", b.auth_prefix)),
        _ => req,
    }
}

fn prepare(b: &Backend, g: &Gen, edit: bool) -> (String, Map<String, Value>, Vec<String>) {
    let mut body = b.extra.clone();
    let mut notes = Vec::new();
    let mut prompt = g.prompt.replace("<sd_cpp_extra_args>", "").replace("</sd_cpp_extra_args>", "");
    match b.kind {
        Kind::Openai | Kind::Local | Kind::Comfyui => {
            body.extend(g.params.clone());
            let ignored: Vec<&str> = [
                ("negative_prompt", g.negative_prompt.is_some()),
                ("seed", g.seed.is_some()),
                ("steps", g.steps.is_some()),
                ("guidance", g.guidance.is_some()),
            ]
            .into_iter()
            .filter_map(|(k, set)| set.then_some(k))
            .collect();
            if !ignored.is_empty() {
                notes.push(format!(
                    "Ignored by this OpenAI-compatible backend: {}. Pass provider-specific fields in `params`.",
                    ignored.join(", ")
                ));
            }
        }
        Kind::Sdcpp => {
            let mut extra = g.params.clone();
            if edit {
                extra.entry("strength").or_insert_with(|| json!(1.0));
            }
            if let Some(v) = &g.negative_prompt {
                extra.insert("negative_prompt".into(), json!(v));
            }
            if let Some(v) = g.seed {
                extra.insert("seed".into(), json!(v));
            }
            let guidance = g.guidance.or(if edit { Some(1.0) } else { None });
            if g.steps.is_some() || guidance.is_some() {
                let sp = extra.entry("sample_params").or_insert_with(|| json!({}));
                if let Some(o) = sp.as_object_mut() {
                    if let Some(v) = g.steps {
                        o.insert("sample_steps".into(), json!(v));
                    }
                    if let Some(v) = guidance.filter(|_| !o.contains_key("guidance")) {
                        o.insert("guidance".into(), json!({ "txt_cfg": v }));
                    }
                }
            }
            if !extra.is_empty() {
                prompt.push_str("<sd_cpp_extra_args>");
                prompt.push_str(&Value::Object(extra).to_string());
                prompt.push_str("</sd_cpp_extra_args>");
            }
        }
    }
    if let Some(m) = &b.model {
        body.entry("model").or_insert_with(|| json!(m));
    }
    body.insert("n".into(), json!(g.count));
    if let (Some(w), Some(h)) = (g.width, g.height) {
        body.insert("size".into(), json!(format!("{w}x{h}")));
    }
    (prompt, body, notes)
}

async fn read_images(name: &str, resp: reqwest::Response) -> Result<Vec<Vec<u8>>, String> {
    let status = resp.status();
    if status.is_redirection() {
        let to = resp.headers().get(reqwest::header::LOCATION).and_then(|v| v.to_str().ok()).unwrap_or("?").to_owned();
        return Err(format!("Backend '{name}' redirected to another host ({to}). Not followed, so the API key is not sent there. Use the final address as the base URL"));
    }
    let bytes = read_capped(resp, &format!("Backend '{name}'")).await?;
    if !status.is_success() {
        let text = String::from_utf8_lossy(&bytes);
        let msg = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|v| {
                v.pointer("/error/message")
                    .or_else(|| v.get("error"))
                    .or_else(|| v.get("message"))
                    .or_else(|| v.get("detail"))
                    .map(|m| m.as_str().map_or_else(|| m.to_string(), str::to_owned))
            })
            .unwrap_or_else(|| text.chars().take(500).collect());
        return Err(format!("Backend '{name}' returned HTTP {}: {msg}", status.as_u16()));
    }
    let v: Value = serde_json::from_slice(&bytes).map_err(|e| format!("Backend '{name}': response is not JSON: {e}"))?;
    let data = v
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("Backend '{name}': response has no `data` array"))?;
    let engine = base64::engine::general_purpose::STANDARD;
    let mut out = Vec::with_capacity(data.len());
    let mut urls = Vec::new();
    for (i, item) in data.iter().enumerate() {
        if let Some(b64) = item.get("b64_json").and_then(Value::as_str) {
            let b = engine.decode(b64).map_err(|e| format!("Backend '{name}': bad base64: {e}"))?;
            if !is_image_bytes(&b) {
                return Err(format!("Backend '{name}' returned data that is not an image"));
            }
            out.push((i, b));
        } else if let Some(url) = item.get("url").and_then(Value::as_str) {
            urls.push((i, url.to_owned()));
        }
    }
    let mut set = tokio::task::JoinSet::new();
    for (i, url) in urls {
        set.spawn(async move {
            let r = crate::download::client().get(&url).send().await.and_then(reqwest::Response::error_for_status).map_err(|e| e.to_string())?;
            let b = read_capped(r, "image download").await?;
            if is_image_bytes(&b) { Ok((i, b)) } else { Err("downloaded file is not an image".to_string()) }
        });
    }
    while let Some(r) = set.join_next().await {
        out.push(r.map_err(|e| e.to_string())?.map_err(|e| format!("Backend '{name}': image download failed: {e}"))?);
    }
    out.sort_unstable_by_key(|(i, _)| *i);
    if out.is_empty() {
        return Err(format!("Backend '{name}' returned no images"));
    }
    Ok(out.into_iter().map(|(_, b)| b).collect())
}

pub async fn generate(name: &str, b: &Backend, g: &Gen) -> Result<Output, String> {
    let (prompt, mut body, notes) = prepare(b, g, false);
    body.insert("prompt".into(), json!(prompt));
    let req = auth(client().post(endpoint(b, "images/generations")), name, b).json(&body);
    let resp = req.send().await.map_err(|e| format!("Backend '{name}' is unreachable at {}: {e}", b.base_url))?;
    Ok(Output { images: read_images(name, resp).await?, notes })
}

fn mime_of(p: &std::path::Path) -> &'static str {
    match p.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        _ => "image/png",
    }
}

pub async fn read_input_image(p: &PathBuf) -> Result<Vec<u8>, String> {
    let meta = tokio::fs::metadata(p).await.map_err(|e| format!("Cannot read {}: {e}", p.display()))?;
    if !meta.is_file() || meta.len() > MAX_IMAGE as u64 {
        return Err(format!("{} is not an image file under {} MB", p.display(), MAX_IMAGE >> 20));
    }
    let bytes = tokio::fs::read(p).await.map_err(|e| format!("Cannot read {}: {e}", p.display()))?;
    if !is_image_bytes(&bytes) {
        return Err(format!("{} is not a PNG, JPEG, WebP or GIF image", p.display()));
    }
    Ok(bytes)
}

async fn file_part(p: &PathBuf) -> Result<reqwest::multipart::Part, String> {
    let bytes = read_input_image(p).await?;
    let fname = p.file_name().and_then(|n| n.to_str()).unwrap_or("image.png").to_owned();
    reqwest::multipart::Part::bytes(bytes)
        .file_name(fname)
        .mime_str(mime_of(p))
        .map_err(|e| e.to_string())
}

pub async fn edit(name: &str, b: &Backend, g: &Gen, images: &[PathBuf], mask: Option<&PathBuf>) -> Result<Output, String> {
    let (prompt, body, notes) = prepare(b, g, true);
    let mut form = reqwest::multipart::Form::new().text("prompt", prompt);
    for (k, v) in body {
        form = form.text(k, v.as_str().map_or_else(|| v.to_string(), str::to_owned));
    }
    let field = if images.len() > 1 { "image[]" } else { "image" };
    for p in images {
        form = form.part(field, file_part(p).await?);
    }
    if let Some(m) = mask {
        form = form.part("mask", file_part(m).await?);
    }
    let req = auth(client().post(endpoint(b, "images/edits")), name, b).multipart(form);
    let resp = req.send().await.map_err(|e| format!("Backend '{name}' is unreachable at {}: {e}", b.base_url))?;
    Ok(Output { images: read_images(name, resp).await?, notes })
}
