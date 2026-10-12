//! Edit › Content-Aware Fill…: PatchMatch by default, optional AI Horde inpaint.

use serde_json::{Map, Value, json};

use crate::PhotocraftApp;
use crate::theme::Tokens;

pub const COMMAND: &str = "edit.contentAwareFill";

const CAF_PROMPT: &str = "photorealistic inpainting, match surrounding texture lighting and perspective, empty space, no extra objects";

pub fn owns(fields: &Map<String, Value>) -> bool {
    fields.get("__command").and_then(Value::as_str) == Some(COMMAND)
}

pub fn is_ai(fields: &Map<String, Value>) -> bool {
    owns(fields) && fields.get("method").and_then(Value::as_str) == Some("ai")
}

pub fn ok_label(fields: &Map<String, Value>) -> Option<&'static str> {
    is_ai(fields).then_some(tl!("Generate"))
}

/// Restore the last AI key/hint after the generic filter dialog opens.
pub fn hydrate_from(saved: Option<&Value>, fields: &mut Map<String, Value>) {
    if fields.get("method").and_then(Value::as_str).is_none() {
        fields.insert("method".into(), json!("contentAware"));
    }
    let Some(Value::Object(saved)) = saved else { return };
    if let Some(k) = saved.get("apiKey").and_then(Value::as_str) {
        fields.insert("apiKey".into(), json!(k));
    }
    if let Some(h) = saved.get("hint").and_then(Value::as_str) {
        fields.insert("hint".into(), json!(h));
    }
}

pub fn method_row(ui: &mut egui::Ui, f: &mut Map<String, Value>) {
    let t = Tokens::get(ui.ctx());
    let mut cur = f.get("method").and_then(Value::as_str).unwrap_or("contentAware").to_string();
    if cfg!(target_arch = "wasm32") && cur == "ai" {
        cur = "contentAware".into();
    }
    let mut opts: Vec<(String, &str)> = vec![("contentAware".into(), tl!("Content-Aware"))];
    if !cfg!(target_arch = "wasm32") {
        opts.push(("ai".into(), tl!("AI (AI Horde)")));
    }
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(tl!("Method")).color(t.text_dim));
        crate::widgets::dropdown(ui, "caf-method", &mut cur, &opts, 170.0);
    });
    f.insert("method".into(), json!(cur));
}

pub fn ai_body(ui: &mut egui::Ui, f: &mut Map<String, Value>) {
    ui.add_space(6.0);
    ui.label(tl!("AI reconstructs the selection to match the surroundings."));
    ui.add_space(6.0);
    let mut hint = f.get("hint").and_then(Value::as_str).unwrap_or("").to_string();
    ui.label(tl!("Hint (optional)"));
    let edit = egui::TextEdit::multiline(&mut hint).desired_rows(3).desired_width(f32::INFINITY).hint_text(tl!("Hint (optional)"));
    if ui.add(edit).changed() {
        f.insert("hint".into(), json!(hint));
    }
    ui.add_space(8.0);
    ui.label(tl!("AI Horde"));
    ui.add_space(4.0);
    let mut key = f.get("apiKey").and_then(Value::as_str).unwrap_or("").to_string();
    ui.horizontal(|ui| {
        ui.label(tl!("API key (optional)"));
        let key_edit = egui::TextEdit::singleline(&mut key).password(true).desired_width(220.0);
        if ui.add(key_edit).changed() {
            f.insert("apiKey".into(), json!(key));
        }
    });
    ui.add_space(4.0);
    ui.weak(tl!("Anonymous access is used when the key is empty. Large selections are sent at up to 1024 px."));
}

fn remember(app: &mut PhotocraftApp, f: &Map<String, Value>) {
    crate::filter_dialog::remember(app, COMMAND, f);
    let api_key = f.get("apiKey").and_then(Value::as_str).unwrap_or("");
    let hint = f.get("hint").and_then(Value::as_str).unwrap_or("");
    app.session.prefs.edit(|p| {
        let entry = p.dialogs.entry(COMMAND.into()).or_insert_with(|| json!({}));
        if let Value::Object(m) = entry {
            if api_key.trim().is_empty() {
                m.remove("apiKey");
            } else {
                m.insert("apiKey".into(), json!(api_key));
            }
            if hint.trim().is_empty() {
                m.remove("hint");
            } else {
                m.insert("hint".into(), json!(hint));
            }
        }
    });
}

fn patchmatch_params(f: &Map<String, Value>) -> Value {
    let mut p = crate::filter_dialog::params_of(f);
    if let Some(o) = p.as_object_mut() {
        o.remove("apiKey");
        o.remove("hint");
        if o.get("method").and_then(Value::as_str) == Some("contentAware") {
            o.remove("method");
        }
    }
    p
}

pub fn confirm(app: &mut PhotocraftApp, f: &Map<String, Value>) -> Result<Value, String> {
    remember(app, f);
    if is_ai(f) {
        return confirm_ai(app, f);
    }
    let params = patchmatch_params(f);
    let result = app.run(COMMAND, params.clone());
    let running = app
        .session
        .active()
        .is_some_and(|st| app.filter_preview.as_ref().is_some_and(|p| p.key.doc == st.doc.id) && app.session.job_on(st.doc.id).is_some());
    match app.filter_preview.as_mut() {
        Some(p) if result.is_ok() && running => p.committing = true,
        _ => app.filter_preview = None,
    }
    result
}

fn confirm_ai(app: &mut PhotocraftApp, f: &Map<String, Value>) -> Result<Value, String> {
    app.filter_preview = None;
    #[cfg(target_arch = "wasm32")]
    {
        let _ = app;
        let _ = f;
        return Err(tl!("AI Content-Aware Fill is only available in the desktop app.").into());
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        if !app.session.is_enabled(COMMAND) {
            return Err(tl!("Content-Aware Fill needs a selection on a pixel layer").into());
        }
        let hint = f.get("hint").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let prompt = if hint.is_empty() { CAF_PROMPT.to_string() } else { format!("{CAF_PROMPT}. {hint}") };
        let api_key = f.get("apiKey").and_then(Value::as_str).unwrap_or("").to_string();
        let output = f.get("output").and_then(Value::as_str).unwrap_or("current").to_string();
        let cap = crate::generative_fill_ui::capture(app, tl!("Content-Aware Fill needs a selection on a pixel layer"))?;
        let origin = cap.origin;
        let req = crate::horde::InpaintRequest {
            prompt,
            width: cap.width,
            height: cap.height,
            image_webp: cap.image_webp,
            mask_webp: cap.mask_webp,
            api_key,
        };
        use photocraft_engine::jobs::Started;
        let inpaint = crate::generative_fill_ui::take_stub().unwrap_or(crate::generative_fill_ui::horde_default);
        let started = app.session.start_job(
            COMMAND,
            json!({"method": "ai", "output": output}),
            "Content-Aware Fill",
            true,
            move |ctx| inpaint(&req, ctx).map(|r| r.bytes).map_err(photocraft_engine::EngineError::Other),
            move |s, bytes| {
                use base64::Engine as _;
                let png = base64::engine::general_purpose::STANDARD.encode(&bytes);
                s.execute(
                    COMMAND,
                    json!({
                        "method": "ai",
                        "png": png,
                        "origin": [origin[0], origin[1]],
                        "output": output,
                    }),
                )
            },
        );
        match started.map_err(|e| e.to_string())? {
            Started::Done(v) => Ok(v),
            Started::Job(job) => {
                app.jobs.last_started = Some(job);
                app.ui.status.clear();
                app.ui.status_error = false;
                Ok(json!({"job": job.0, "pending": true}))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_codecs::{ChannelLayout, EncodeOptions, Format, Image};
    use photocraft_engine::Session;
    use photocraft_engine::jobs::JobCtx;

    fn app_with_selection() -> PhotocraftApp {
        let mut app = PhotocraftApp::new(Session::new(), crate::Services::default());
        app.run("file.new", json!({"width": 64, "height": 48, "background": "white"})).unwrap();
        app.run("select.rect", json!({"x": 8, "y": 8, "width": 16, "height": 16})).unwrap();
        app
    }

    #[test]
    fn menu_opens_with_content_aware_method() {
        let mut app = app_with_selection();
        let r = crate::menus::invoke(&mut app, &egui::Context::default(), COMMAND, json!({})).unwrap();
        let id = r["dialog"].as_u64().unwrap();
        let d = app.ui.dialogs.iter().find(|d| d.id == id).unwrap();
        assert!(owns(&d.fields));
        assert_eq!(d.fields.get("method").and_then(Value::as_str), Some("contentAware"));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn ai_fill_paints_the_selection() {
        fn stub(req: &crate::horde::InpaintRequest, _ctx: &JobCtx) -> Result<crate::horde::InpaintResult, String> {
            assert!(req.prompt.contains("inpainting"), "{}", req.prompt);
            let img = Image::from_u8(req.width, req.height, ChannelLayout::Rgba, vec![20u8; req.width as usize * req.height as usize * 4])
                .map_err(|e| e.to_string())?;
            let bytes = photocraft_codecs::encode(&img, Format::Png, &EncodeOptions::default()).map_err(|e| e.to_string())?;
            Ok(crate::horde::InpaintResult { bytes })
        }
        crate::generative_fill_ui::set_inpaint_stub(Some(stub));
        let mut app = app_with_selection();
        app.background_jobs = false;
        let mut f = Map::new();
        f.insert("__command".into(), json!(COMMAND));
        f.insert("method".into(), json!("ai"));
        f.insert("output".into(), json!("current"));
        let r = confirm(&mut app, &f);
        crate::generative_fill_ui::set_inpaint_stub(None);
        let r = r.expect("ai fill");
        if let Some(id) = r.get("job").and_then(Value::as_u64) {
            app.session.wait_job(photocraft_engine::jobs::JobId(id)).unwrap();
        }
        let st = app.session.active().unwrap();
        let px = st.doc.layer(st.active_layer.unwrap()).unwrap().surface().unwrap().pixel(12, 12);
        assert!(px[0] < 0.2, "{px:?}");
        assert_eq!(st.history.undo_label(), Some("Content-Aware Fill"));
    }
}
