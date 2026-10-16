use crate::backend::{self, Gen, Output};
use crate::config::{Backend, Config, Kind};
use crate::download::Report;
use crate::output;
use std::sync::{Arc, Mutex};
use rmcp::{
    ErrorData, Peer, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, ProgressNotificationParam, RequestMetaObject, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::future::Future;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const INSTRUCTIONS: &str = "Generates and edits raster images with the user's configured image model (local or API). Use generate_image for new pictures and edit_image to change existing ones. Results are saved to disk and returned as a small preview. Use save_to when the image belongs in the current project.";

#[derive(Deserialize, schemars::JsonSchema)]
pub struct GenerateArgs {
    #[schemars(description = "Full natural-language description of the image: subject, setting, composition, lighting, style")]
    pub prompt: String,
    #[serde(flatten)]
    pub opts: Opts,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct Opts {
    #[schemars(description = "Things to avoid. Only some backends use it")]
    pub negative_prompt: Option<String>,
    #[schemars(description = "Width in pixels, multiple of 32 recommended. Omit for the backend default")]
    pub width: Option<u32>,
    #[schemars(description = "Height in pixels, multiple of 32 recommended. Omit for the backend default")]
    pub height: Option<u32>,
    #[schemars(description = "Number of images, 1 to 4. Default 1")]
    pub count: Option<u32>,
    #[schemars(description = "Seed for reproducible results. Only some backends use it")]
    pub seed: Option<i64>,
    #[schemars(description = "Sampling steps. Only local backends use it")]
    pub steps: Option<u32>,
    #[schemars(description = "Guidance / CFG scale. Only local backends use it")]
    pub guidance: Option<f32>,
    #[schemars(description = "Backend name. Omit to use the default backend")]
    pub backend: Option<String>,
    #[schemars(description = "Also copy the result into the project: a folder (ends with /) or a file path. Relative to the project root")]
    pub save_to: Option<String>,
    #[schemars(description = "Advanced: extra provider-specific request fields")]
    pub params: Option<Map<String, Value>>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct EditArgs {
    #[schemars(description = "Paths of the input image(s). The first is the main image; more images act as references")]
    pub images: Vec<String>,
    #[schemars(description = "What to change, and what to keep")]
    pub prompt: String,
    #[schemars(description = "Optional mask image path. Transparent areas are edited")]
    pub mask: Option<String>,
    #[serde(flatten)]
    pub opts: Opts,
}

#[derive(Clone)]
pub struct Server {
    #[expect(dead_code, reason = "tool_handler")]
    tool_router: ToolRouter<Self>,
}

impl Server {
    pub fn new() -> Self {
        Self { tool_router: Self::tool_router() }
    }
}

fn fail(msg: impl Into<String>) -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::error(vec![ContentBlock::text(msg.into())]))
}

type Status = Arc<Mutex<String>>;

fn reporter(status: &Status) -> Report {
    let s = status.clone();
    Arc::new(move |m: &str| {
        if let Ok(mut g) = s.lock() {
            m.clone_into(&mut g);
        }
    })
}

pub(crate) async fn run_backend(name: &str, b: &Backend, g: &Gen, images: &[PathBuf], mask: Option<&PathBuf>, report: &Report) -> Result<Output, String> {
    match b.kind {
        Kind::Openai | Kind::Sdcpp if images.is_empty() => backend::generate(name, b, g).await,
        Kind::Openai | Kind::Sdcpp => backend::edit(name, b, g, images, mask).await,
        Kind::Comfyui => crate::comfyui::run(name, b, g, images).await,
        Kind::Local => {
            let (url, lease) = crate::engine::MANAGER.ensure(b, report).await?;
            report("Generating image");
            let sb = Backend { kind: Kind::Sdcpp, base_url: url, auth_header: String::new(), ..Backend::default() };
            let r = if images.is_empty() { backend::generate(name, &sb, g).await } else { backend::edit(name, &sb, g, images, mask).await };
            drop(lease);
            r
        }
    }
}

async fn with_progress<T>(meta: &RequestMetaObject, peer: &Peer<RoleServer>, status: &Status, fut: impl Future<Output = T>) -> T {
    let Some(token) = meta.get_progress_token() else {
        return fut.await;
    };
    tokio::pin!(fut);
    let start = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    tick.tick().await;
    loop {
        tokio::select! {
            r = &mut fut => return r,
            _ = tick.tick() => {
                let s = start.elapsed().as_secs();
                let label = status.lock().map(|g| g.clone()).unwrap_or_default();
                let label = if label.is_empty() { "Generating image".to_owned() } else { label };
                let _ = peer
                    .notify_progress(ProgressNotificationParam::new(token.clone(), s as f64).with_message(format!("{label} ({s}s)")))
                    .await;
            }
        }
    }
}

fn gen_of(prompt: String, o: &mut Opts) -> Gen {
    Gen {
        prompt,
        negative_prompt: o.negative_prompt.take(),
        width: o.width,
        height: o.height,
        count: o.count.unwrap_or(1).clamp(1, 4),
        seed: o.seed,
        steps: o.steps,
        guidance: o.guidance,
        params: o.params.take().unwrap_or_default(),
    }
}

async fn finish(cfg: Config, name: String, prompt: String, save_to: Option<String>, out: Output, started: Instant) -> Result<CallToolResult, ErrorData> {
    let gen_ms = started.elapsed().as_millis();
    let root = cfg.output_dir();
    let Output { images, notes } = out;
    let job = tokio::task::spawn_blocking(move || {
        let files = output::save(&root, &prompt, &images).map_err(|e| format!("Cannot save to {}: {e}", root.display()))?;
        let copies = match &save_to {
            Some(t) => output::copy_to(t, &files).map_err(|e| format!("Saved to {}, but copy to '{t}' failed: {e}", root.display()))?,
            None => Vec::new(),
        };
        let previews: Vec<Option<String>> = std::thread::scope(|s| {
            let hs: Vec<_> = images.iter().map(|b| s.spawn(move || output::preview(b))).collect();
            hs.into_iter().map(|h| h.join().ok().flatten()).collect()
        });
        Ok::<_, String>((files, copies, previews))
    })
    .await;
    let (files, copies, previews) = match job {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return fail(e),
        Err(e) => return fail(format!("Internal error while saving: {e}")),
    };
    let summary = json!({
        "files": files,
        "copied_to": copies,
        "backend": name,
        "generation_ms": gen_ms,
        "notes": notes,
    });
    let mut content = vec![ContentBlock::text(summary.to_string())];
    content.extend(previews.into_iter().flatten().map(|b64| ContentBlock::image(b64, "image/jpeg")));
    Ok(CallToolResult::success(content))
}

#[tool_router]
impl Server {
    #[tool(description = "Generate new image(s) from a text prompt with the configured image model. Saves full-size files and returns their paths plus a small preview")]
    async fn generate_image(&self, meta: RequestMetaObject, peer: Peer<RoleServer>, Parameters(mut a): Parameters<GenerateArgs>) -> Result<CallToolResult, ErrorData> {
        let started = Instant::now();
        let cfg = match Config::load() {
            Ok(c) => c,
            Err(e) => return fail(e),
        };
        let (name, b) = match cfg.resolve(a.opts.backend.as_deref()) {
            Ok((n, b)) => (n, b.clone()),
            Err(e) => return fail(e),
        };
        let prompt = a.prompt.clone();
        let g = gen_of(a.prompt, &mut a.opts);
        let status = Status::default();
        let report = reporter(&status);
        match with_progress(&meta, &peer, &status, run_backend(&name, &b, &g, &[], None, &report)).await {
            Ok(out) => finish(cfg, name, prompt, a.opts.save_to, out, started).await,
            Err(e) => fail(e),
        }
    }

    #[tool(description = "Edit existing image(s) with a text instruction, or use them as references for a new image. Saves full-size files and returns their paths plus a small preview")]
    async fn edit_image(&self, meta: RequestMetaObject, peer: Peer<RoleServer>, Parameters(mut a): Parameters<EditArgs>) -> Result<CallToolResult, ErrorData> {
        let started = Instant::now();
        if a.images.is_empty() {
            return fail("`images` needs at least one image path");
        }
        let cfg = match Config::load() {
            Ok(c) => c,
            Err(e) => return fail(e),
        };
        let (name, b) = match cfg.resolve(a.opts.backend.as_deref()) {
            Ok((n, b)) => (n, b.clone()),
            Err(e) => return fail(e),
        };
        let images: Vec<PathBuf> = a.images.iter().map(PathBuf::from).collect();
        let mask = a.mask.as_ref().map(PathBuf::from);
        let prompt = a.prompt.clone();
        let g = gen_of(a.prompt, &mut a.opts);
        let status = Status::default();
        let report = reporter(&status);
        match with_progress(&meta, &peer, &status, run_backend(&name, &b, &g, &images, mask.as_ref(), &report)).await {
            Ok(out) => finish(cfg, name, prompt, a.opts.save_to, out, started).await,
            Err(e) => fail(e),
        }
    }

    #[tool(description = "Open the imagegen-cc setup dashboard in the user's browser to add a local model, an image API, or ComfyUI. Use when no backend is configured or the user asks to set up or change image generation")]
    async fn open_setup(&self) -> Result<CallToolResult, ErrorData> {
        match crate::dashboard::start().await {
            Ok(url) => {
                let opened = crate::dashboard::open_browser(&url);
                let msg = if opened {
                    format!("The setup dashboard is open in the browser. If it did not appear, open: {url}")
                } else {
                    format!("Open this address in a browser to set up image generation: {url}")
                };
                Ok(CallToolResult::success(vec![ContentBlock::text(msg)]))
            }
            Err(e) => fail(format!("Could not start the setup dashboard: {e}")),
        }
    }

    #[tool(description = "List configured image backends, the default backend, and where images are saved")]
    async fn list_backends(&self) -> Result<CallToolResult, ErrorData> {
        let cfg = match Config::load() {
            Ok(c) => c,
            Err(e) => return fail(e),
        };
        let env_key = std::env::var("IMAGEGEN_CC_API_KEY").is_ok_and(|v| !v.is_empty());
        let backends: Map<String, Value> = cfg
            .backends
            .iter()
            .map(|(n, b)| {
                let key = if env_key {
                    "env"
                } else {
                    match crate::secrets::has(n) {
                        Some(crate::secrets::Store::Keychain) => "keychain",
                        Some(crate::secrets::Store::File) => "file",
                        None => "none",
                    }
                };
                let v = match b.kind {
                    Kind::Local => json!({ "kind": b.kind, "recipe": b.recipe, "variant": b.variant, "engine": b.engine, "edit": b.recipe.as_deref().and_then(|r| crate::catalog::CATALOG.recipe(r)).is_some_and(|r| r.edit) }),
                    Kind::Comfyui => json!({ "kind": b.kind, "base_url": b.base_url, "workflow": b.workflow }),
                    _ => json!({ "kind": b.kind, "base_url": b.base_url, "model": b.model, "api_key": key }),
                };
                (n.clone(), v)
            })
            .collect();
        let v = json!({
            "default_backend": cfg.default_backend,
            "backends": backends,
            "output_dir": cfg.output_dir(),
            "config": Config::path(),
            "setup": if cfg.backends.is_empty() { Some(crate::config::NO_BACKEND) } else { None },
        });
        Ok(CallToolResult::success(vec![ContentBlock::text(v.to_string())]))
    }
}

#[tool_handler]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(INSTRUCTIONS)
    }
}
