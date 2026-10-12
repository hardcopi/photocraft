//! AI Horde image inpainting (native only).
//!
//! POST `/api/v2/generate/async` with `source_processing: inpainting`, poll `/check`, then
//! `/status`. Tests inject a [`Transport`] so they never touch the network.

use std::io::Read;
use std::thread;
use std::time::{Duration, Instant};

use photocraft_engine::jobs::JobCtx;
use serde_json::{Value, json};

pub const BASE: &str = "https://aihorde.net/api/v2";
pub const ANON_KEY: &str = "0000000000";
pub const MAX_SIDE: u32 = 1024;
pub const MIN_SIDE: u32 = 64;

const POLL: Duration = Duration::from_secs(2);
const TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// One HTTP round-trip. Status and raw body; the caller parses JSON.
pub trait Transport {
    fn send(&self, method: &str, url: &str, api_key: &str, json_body: Option<&Value>) -> Result<(u16, Vec<u8>), String>;
}

/// Blocking ureq client with the Client-Agent Horde requires.
pub struct UreqTransport {
    pub agent: String,
}

impl Default for UreqTransport {
    fn default() -> Self {
        Self { agent: format!("PhotoCraft:{}", env!("CARGO_PKG_VERSION")) }
    }
}

impl Transport for UreqTransport {
    fn send(&self, method: &str, url: &str, api_key: &str, json_body: Option<&Value>) -> Result<(u16, Vec<u8>), String> {
        let req = ureq::request(method, url).set("apikey", api_key).set("Client-Agent", &self.agent).timeout(Duration::from_secs(45));
        let resp = match json_body {
            Some(body) => req.set("Content-Type", "application/json").send_string(&body.to_string()),
            None => req.call(),
        };
        match resp {
            Ok(r) => {
                let status = r.status();
                let mut bytes = Vec::new();
                r.into_reader().read_to_end(&mut bytes).map_err(|e| format!("AI Horde: failed to read response: {e}"))?;
                Ok((status, bytes))
            }
            Err(ureq::Error::Status(code, r)) => {
                let msg = r.into_string().unwrap_or_default();
                let detail = serde_json::from_str::<Value>(&msg).ok().and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_string)).unwrap_or(msg);
                Err(format!("AI Horde ({code}): {detail}"))
            }
            Err(e) => Err(format!("AI Horde: {e}")),
        }
    }
}

#[derive(Debug)]
pub struct InpaintRequest {
    pub prompt: String,
    pub width: u32,
    pub height: u32,
    pub image_webp: Vec<u8>,
    pub mask_webp: Vec<u8>,
    pub api_key: String,
}

#[derive(Debug)]
pub struct InpaintResult {
    pub bytes: Vec<u8>,
}

pub fn resolve_key(explicit: &str) -> String {
    let t = explicit.trim();
    if !t.is_empty() {
        return t.to_string();
    }
    std::env::var("AIHORDE_API_KEY").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| ANON_KEY.into())
}

/// Pad a side length to Horde's 64-pixel grid, clamped to 64..=1024.
pub fn snap_side(n: u32) -> u32 {
    let n = n.clamp(MIN_SIDE, MAX_SIDE);
    let snapped = n.div_ceil(64).saturating_mul(64);
    snapped.clamp(MIN_SIDE, MAX_SIDE)
}

pub fn inpaint(t: &dyn Transport, req: &InpaintRequest, ctx: &JobCtx) -> Result<InpaintResult, String> {
    if req.prompt.trim().is_empty() {
        return Err("enter a prompt describing what should appear".into());
    }
    if !(MIN_SIDE..=MAX_SIDE).contains(&req.width)
        || !(MIN_SIDE..=MAX_SIDE).contains(&req.height)
        || !req.width.is_multiple_of(64)
        || !req.height.is_multiple_of(64)
    {
        return Err(format!("AI Horde needs width and height as multiples of 64 between {MIN_SIDE} and {MAX_SIDE}"));
    }
    ctx.progress(0.02, "Queued on AI Horde…");
    ctx.check().map_err(|_| "cancelled".to_string())?;
    let key = resolve_key(&req.api_key);
    let image_b64 = b64(&req.image_webp);
    let mask_b64 = b64(&req.mask_webp);
    let body = json!({
        "prompt": req.prompt,
        "params": {
            "sampler_name": "k_euler_a",
            "cfg_scale": 7.5,
            "denoising_strength": 1.0,
            "height": req.height,
            "width": req.width,
            "steps": 20,
            "n": 1,
            "karras": true,
        },
        "nsfw": false,
        "censor_nsfw": true,
        "trusted_workers": false,
        "slow_workers": true,
        "extra_slow_workers": true,
        "r2": true,
        "shared": false,
        "replacement_filter": true,
        "source_processing": "inpainting",
        "source_image": image_b64,
        "source_mask": mask_b64,
    });
    let (status, bytes) = t.send("POST", &format!("{BASE}/generate/async"), &key, Some(&body))?;
    if status != 202 && status != 200 {
        return Err(horde_error(status, &bytes));
    }
    let v: Value = serde_json::from_slice(&bytes).map_err(|e| format!("AI Horde: bad queue response: {e}"))?;
    let id = v.get("id").and_then(Value::as_str).ok_or_else(|| "AI Horde: queue response had no id".to_string())?;
    let started = Instant::now();
    loop {
        ctx.check().map_err(|_| {
            let _ = t.send("DELETE", &format!("{BASE}/generate/status/{id}"), &key, None);
            "cancelled".to_string()
        })?;
        if started.elapsed() > TIMEOUT {
            let _ = t.send("DELETE", &format!("{BASE}/generate/status/{id}"), &key, None);
            return Err("AI Horde: timed out waiting for a worker".into());
        }
        if cfg!(not(test)) {
            thread::sleep(POLL);
        }
        ctx.check().map_err(|_| {
            let _ = t.send("DELETE", &format!("{BASE}/generate/status/{id}"), &key, None);
            "cancelled".to_string()
        })?;
        let (st, raw) = t.send("GET", &format!("{BASE}/generate/check/{id}"), &key, None)?;
        if st != 200 {
            return Err(horde_error(st, &raw));
        }
        let check: Value = serde_json::from_slice(&raw).map_err(|e| format!("AI Horde: bad check response: {e}"))?;
        if check.get("faulted").and_then(Value::as_bool) == Some(true) {
            return Err("AI Horde: the request failed on the server".into());
        }
        if check.get("is_possible").and_then(Value::as_bool) == Some(false) {
            let _ = t.send("DELETE", &format!("{BASE}/generate/status/{id}"), &key, None);
            return Err("AI Horde: no workers can inpaint this request right now".into());
        }
        let wait = check.get("wait_time").and_then(Value::as_u64).unwrap_or(0);
        let processing = check.get("processing").and_then(Value::as_u64).unwrap_or(0);
        let msg = if processing > 0 { "Waiting for an AI Horde worker…" } else { "Queued on AI Horde…" };
        let frac = if wait == 0 { 0.7 } else { (1.0 / (wait as f32 + 8.0)).clamp(0.05, 0.7) };
        ctx.progress(frac, msg);
        if check.get("done").and_then(Value::as_bool) == Some(true) {
            break;
        }
    }
    ctx.progress(0.85, "Downloading result…");
    let (st, raw) = t.send("GET", &format!("{BASE}/generate/status/{id}"), &key, None)?;
    if st != 200 {
        return Err(horde_error(st, &raw));
    }
    let status_v: Value = serde_json::from_slice(&raw).map_err(|e| format!("AI Horde: bad status response: {e}"))?;
    let img = status_v
        .get("generations")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|g| g.get("img"))
        .and_then(Value::as_str)
        .ok_or_else(|| "AI Horde: no image in the result".to_string())?;
    if status_v.pointer("/generations/0/censored").and_then(Value::as_bool) == Some(true) {
        return Err("AI Horde: the result was censored".into());
    }
    let bytes = if img.starts_with("http://") || img.starts_with("https://") {
        let (st, raw) = t.send("GET", img, &key, None)?;
        if st != 200 {
            return Err(format!("AI Horde: download failed ({st})"));
        }
        raw
    } else {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.decode(img.trim()).map_err(|_| "AI Horde: result was not valid base64".to_string())?
    };
    if bytes.is_empty() {
        return Err("AI Horde: empty image".into());
    }
    ctx.progress(1.0, "Downloading result…");
    Ok(InpaintResult { bytes })
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn horde_error(status: u16, bytes: &[u8]) -> String {
    let msg = serde_json::from_slice::<Value>(bytes)
        .ok()
        .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| String::from_utf8_lossy(bytes).chars().take(200).collect());
    format!("AI Horde ({status}): {msg}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Fake {
        responses: Mutex<Vec<(u16, Vec<u8>)>>,
        calls: Mutex<Vec<(String, String)>>,
    }

    impl Transport for Fake {
        fn send(&self, method: &str, url: &str, _api_key: &str, _json_body: Option<&Value>) -> Result<(u16, Vec<u8>), String> {
            self.calls.lock().unwrap().push((method.into(), url.into()));
            let mut q = self.responses.lock().unwrap();
            if q.is_empty() {
                return Err("no stubbed response".into());
            }
            Ok(q.remove(0))
        }
    }

    fn png_1x1() -> Vec<u8> {
        photocraft_codecs::encode(
            &photocraft_codecs::Image::from_u8(64, 64, photocraft_codecs::ChannelLayout::Rgba, vec![10; 64 * 64 * 4]).unwrap(),
            photocraft_codecs::Format::Png,
            &photocraft_codecs::EncodeOptions::default(),
        )
        .unwrap()
    }

    #[test]
    fn snap_side_is_a_multiple_of_64() {
        assert_eq!(snap_side(1), 64);
        assert_eq!(snap_side(64), 64);
        assert_eq!(snap_side(65), 128);
        assert_eq!(snap_side(10000), 1024);
    }

    #[test]
    fn inpaint_polls_then_downloads() {
        let png = png_1x1();
        let url = "https://example.test/img.webp";
        let fake = Fake {
            responses: Mutex::new(vec![
                (202, json!({"id": "abc"}).to_string().into_bytes()),
                (200, json!({"done": false, "is_possible": true, "processing": 0, "wait_time": 12, "queue_position": 3}).to_string().into_bytes()),
                (200, json!({"done": true, "is_possible": true, "processing": 0, "wait_time": 0, "queue_position": 0}).to_string().into_bytes()),
                (200, json!({"generations": [{"img": url, "censored": false}]}).to_string().into_bytes()),
                (200, png.clone()),
            ]),
            calls: Mutex::new(Vec::new()),
        };
        let req =
            InpaintRequest { prompt: "a tree".into(), width: 64, height: 64, image_webp: vec![1, 2, 3], mask_webp: vec![4, 5, 6], api_key: String::new() };
        let got = inpaint(&fake, &req, &JobCtx::new()).unwrap();
        assert_eq!(got.bytes, png);
        let calls = fake.calls.lock().unwrap().clone();
        assert_eq!(calls[0].0, "POST");
        assert!(calls[0].1.ends_with("/generate/async"));
        assert_eq!(calls[1].0, "GET");
        assert!(calls[1].1.contains("/generate/check/abc"));
        assert!(calls.last().unwrap().1.contains("example.test"));
    }

    #[test]
    fn inpaint_rejects_an_empty_prompt() {
        let fake = Fake { responses: Mutex::new(Vec::new()), calls: Mutex::new(Vec::new()) };
        let req = InpaintRequest { prompt: "  ".into(), width: 64, height: 64, image_webp: vec![1], mask_webp: vec![1], api_key: String::new() };
        assert!(inpaint(&fake, &req, &JobCtx::new()).unwrap_err().contains("prompt"));
        assert!(fake.calls.lock().unwrap().is_empty());
    }
}
