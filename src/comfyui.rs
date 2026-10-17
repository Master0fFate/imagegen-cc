use crate::backend::{Gen, Output};
use crate::config::Backend;
use crate::download::client;
use serde_json::{Map, Value, json};
use std::path::PathBuf;
use std::time::Duration;

fn num(v: f64) -> Value {
    if v.fract() == 0.0 && v.abs() < 9.0e15 { json!(v as i64) } else { json!(v) }
}

fn substitute(v: &mut Value, vars: &Map<String, Value>) {
    match v {
        Value::String(s) => {
            if let Some(key) = s.strip_prefix("{{").and_then(|r| r.strip_suffix("}}"))
                && let Some(x) = vars.get(key) {
                    *v = x.clone();
                    return;
                }
            if s.contains("{{") {
                let mut out = s.clone();
                for (k, x) in vars {
                    let token = format!("{{{{{k}}}}}");
                    if out.contains(&token) {
                        out = out.replace(&token, &x.as_str().map_or_else(|| x.to_string(), str::to_owned));
                    }
                }
                *s = out;
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| substitute(x, vars)),
        Value::Object(o) => o.values_mut().for_each(|x| substitute(x, vars)),
        _ => {}
    }
}

fn base(b: &Backend) -> &str {
    b.base_url.trim_end_matches('/')
}

async fn upload(name: &str, b: &Backend, p: &PathBuf) -> Result<String, String> {
    let bytes = crate::backend::read_input_image(p).await?;
    let fname = p.file_name().and_then(|n| n.to_str()).unwrap_or("image.png").to_owned();
    let form = reqwest::multipart::Form::new()
        .part("image", reqwest::multipart::Part::bytes(bytes).file_name(fname))
        .text("overwrite", "true");
    let r = client().post(format!("{}/upload/image", base(b))).multipart(form).send().await.map_err(|e| format!("Backend '{name}' is unreachable at {}: {e}", b.base_url))?;
    let v: Value = r.json().await.map_err(|e| format!("Backend '{name}': upload failed: {e}"))?;
    let n = v.get("name").and_then(Value::as_str).ok_or_else(|| format!("Backend '{name}': upload failed: {v}"))?;
    Ok(match v.get("subfolder").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        Some(sf) => format!("{sf}/{n}"),
        None => n.to_owned(),
    })
}

pub async fn run(name: &str, b: &Backend, g: &Gen, images: &[PathBuf]) -> Result<Output, String> {
    let wf = b.workflow.as_ref().ok_or_else(|| format!("Backend '{name}' has no workflow file"))?;
    let text = tokio::fs::read_to_string(wf).await.map_err(|e| format!("Cannot read workflow {}: {e}", wf.display()))?;
    let mut graph: Value = serde_json::from_str(&text).map_err(|e| format!("Workflow {} is not valid JSON: {e}", wf.display()))?;
    if graph.get("nodes").is_some() {
        return Err(format!("Workflow {} is in UI format. In ComfyUI use Workflow > Export (API) and point the backend at that file", wf.display()));
    }
    let defaults = &b.extra;
    let pick = |k: &str, given: Option<f64>, fallback: f64| given.or_else(|| defaults.get(k).and_then(Value::as_f64)).unwrap_or(fallback);
    let seed = g.seed.map_or_else(
        || {
            let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
            json!((t.as_nanos() as u64 ^ 0x9E37_79B9_7F4A_7C15) >> 14)
        },
        |s| json!(s),
    );
    let mut vars = Map::new();
    vars.insert("prompt".into(), json!(g.prompt));
    vars.insert("negative_prompt".into(), json!(g.negative_prompt.clone().unwrap_or_default()));
    vars.insert("seed".into(), seed);
    vars.insert("width".into(), num(pick("width", g.width.map(f64::from), 1024.0)));
    vars.insert("height".into(), num(pick("height", g.height.map(f64::from), 1024.0)));
    vars.insert("steps".into(), num(pick("steps", g.steps.map(f64::from), 30.0)));
    vars.insert("guidance".into(), num(pick("guidance", g.guidance.map(f64::from), 3.5)));
    vars.insert("count".into(), json!(g.count));
    for (k, v) in &g.params {
        vars.insert(k.clone(), v.clone());
    }
    for (i, p) in images.iter().enumerate() {
        let n = upload(name, b, p).await?;
        if i == 0 {
            vars.insert("image".into(), json!(n));
        }
        vars.insert(format!("image{}", i + 1), json!(n));
    }
    substitute(&mut graph, &vars);
    let r = client()
        .post(format!("{}/prompt", base(b)))
        .json(&json!({ "prompt": graph, "client_id": "imagegen-cc" }))
        .send()
        .await
        .map_err(|e| format!("Backend '{name}' is unreachable at {}: {e}", b.base_url))?;
    let status = r.status();
    let v: Value = r.json().await.map_err(|e| format!("Backend '{name}': bad /prompt reply: {e}"))?;
    if !status.is_success() {
        let msg = v.pointer("/error/message").and_then(Value::as_str).unwrap_or("rejected");
        let nodes = v.get("node_errors").map(|n| n.to_string()).unwrap_or_default();
        return Err(format!("Backend '{name}' rejected the workflow: {msg} {}", nodes.chars().take(600).collect::<String>()));
    }
    let id = v.get("prompt_id").and_then(Value::as_str).ok_or("no prompt_id")?.to_owned();
    let hist = loop {
        tokio::time::sleep(Duration::from_millis(400)).await;
        let h: Value = client().get(format!("{}/history/{id}", base(b))).send().await.map_err(|e| e.to_string())?.json().await.map_err(|e| e.to_string())?;
        if let Some(entry) = h.get(&id) {
            if entry.pointer("/status/status_str").and_then(Value::as_str) == Some("error") {
                let msgs = entry.pointer("/status/messages").map(|m| m.to_string()).unwrap_or_default();
                return Err(format!("Backend '{name}': ComfyUI error: {}", msgs.chars().take(800).collect::<String>()));
            }
            if entry.get("outputs").and_then(Value::as_object).is_some_and(|o| !o.is_empty()) || entry.pointer("/status/completed").and_then(Value::as_bool) == Some(true) {
                break entry.clone();
            }
        }
    };
    let mut images_out = Vec::new();
    if let Some(outs) = hist.get("outputs").and_then(Value::as_object) {
        for o in outs.values() {
            for im in o.get("images").and_then(Value::as_array).into_iter().flatten() {
                if im.get("type").and_then(Value::as_str) != Some("output") {
                    continue;
                }
                let q = [
                    ("filename", im.get("filename").and_then(Value::as_str).unwrap_or_default()),
                    ("subfolder", im.get("subfolder").and_then(Value::as_str).unwrap_or_default()),
                    ("type", "output"),
                ];
                let url = reqwest::Url::parse_with_params(&format!("{}/view", base(b)), &q).map_err(|e| e.to_string())?;
                let bytes = client().get(url).send().await.map_err(|e| e.to_string())?.bytes().await.map_err(|e| e.to_string())?;
                images_out.push(bytes.to_vec());
            }
        }
    }
    if images_out.is_empty() {
        return Err(format!("Backend '{name}': the workflow finished but saved no image. Add a SaveImage node"));
    }
    Ok(Output { images: images_out, notes: Vec::new() })
}
