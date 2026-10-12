//! Edit › Generative Fill…: prompt dialog, then AI Horde inpainting on a worker.

use photocraft_codecs::{ChannelLayout, EncodeOptions, Format, Image};
use photocraft_engine::jobs::{JobCtx, Started};
use photocraft_geom::Rect;
use serde_json::{Map, Value, json};

use crate::PhotocraftApp;
use crate::state::DialogKind;

pub const COMMAND: &str = "edit.generativeFill";
const MARK: &str = "__generativeFill";

pub fn owns(fields: &Map<String, Value>) -> bool {
    fields.contains_key(MARK)
}

pub fn fields(app: &PhotocraftApp) -> Map<String, Value> {
    let mut f = Map::new();
    f.insert(MARK.into(), json!(true));
    f.insert("__command".into(), json!(COMMAND));
    f.insert("__label".into(), json!("Generative Fill"));
    f.insert("prompt".into(), json!(""));
    f.insert("apiKey".into(), json!(""));
    if let Some(Value::Object(saved)) = app.session.prefs().dialogs.get(COMMAND) {
        if let Some(p) = saved.get("prompt").and_then(Value::as_str) {
            f.insert("prompt".into(), json!(p));
        }
        if let Some(k) = saved.get("apiKey").and_then(Value::as_str) {
            f.insert("apiKey".into(), json!(k));
        }
    }
    f
}

pub fn open(app: &mut PhotocraftApp) -> u64 {
    app.ui.open_dialog(DialogKind::Command, fields(app))
}

pub fn ok_label() -> &'static str {
    tl!("Generate")
}

pub fn body(_app: &PhotocraftApp, ui: &mut egui::Ui, f: &mut Map<String, Value>) {
    ui.label(tl!("Describe what should appear in the selection."));
    ui.add_space(6.0);
    let mut prompt = f.get("prompt").and_then(Value::as_str).unwrap_or("").to_string();
    let edit = egui::TextEdit::multiline(&mut prompt).desired_rows(5).desired_width(f32::INFINITY).hint_text(tl!("Prompt"));
    if ui.add(edit).changed() {
        f.insert("prompt".into(), json!(prompt));
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

/// Open the dialog when the menu item is invoked with no parameters.
pub fn invoke(app: &mut PhotocraftApp, params: &Value) -> Option<Result<Value, String>> {
    let empty = params.as_object().is_none_or(|o| o.is_empty());
    if !empty {
        return None;
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = app;
        return Some(Err(tl!("Generative Fill is only available in the desktop app.").into()));
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        if !app.session.is_enabled(COMMAND) {
            return Some(Err(tl!("Generative Fill needs a selection on a pixel layer").into()));
        }
        Some(Ok(json!({"dialog": open(app)})))
    }
}

pub fn confirm(app: &mut PhotocraftApp, f: &Map<String, Value>) -> Result<Value, String> {
    let prompt = f.get("prompt").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if prompt.is_empty() {
        return Err(tl!("enter a prompt describing what should appear").into());
    }
    let api_key = f.get("apiKey").and_then(Value::as_str).unwrap_or("").to_string();
    let mut remembered = Map::new();
    remembered.insert("prompt".into(), json!(prompt));
    if !api_key.trim().is_empty() {
        remembered.insert("apiKey".into(), json!(api_key));
    }
    app.session.prefs.edit(|p| p.dialogs.insert(COMMAND.into(), Value::Object(remembered)));

    #[cfg(target_arch = "wasm32")]
    {
        let _ = (app, api_key);
        return Err(tl!("Generative Fill is only available in the desktop app.").into());
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        let cap = capture(app, tl!("Generative Fill needs a selection on a pixel layer"))?;
        let origin = cap.origin;
        let req = crate::horde::InpaintRequest {
            prompt: prompt.clone(),
            width: cap.width,
            height: cap.height,
            image_webp: cap.image_webp,
            mask_webp: cap.mask_webp,
            api_key,
        };
        let inpaint = take_stub().unwrap_or(horde_default);
        let started = app.session.start_job(
            COMMAND,
            json!({"prompt": prompt}),
            "Generative Fill",
            true,
            move |ctx| inpaint(&req, ctx).map(|r| r.bytes).map_err(photocraft_engine::EngineError::Other),
            move |s, bytes| {
                use base64::Engine as _;
                let png = base64::engine::general_purpose::STANDARD.encode(&bytes);
                s.execute(
                    COMMAND,
                    json!({
                        "prompt": prompt,
                        "png": png,
                        "origin": [origin[0], origin[1]],
                        "output": "new",
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

#[cfg(not(target_arch = "wasm32"))]
pub(crate) type InpaintFn = fn(&crate::horde::InpaintRequest, &JobCtx) -> Result<crate::horde::InpaintResult, String>;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct Capture {
    pub origin: [i32; 2],
    pub width: u32,
    pub height: u32,
    pub image_webp: Vec<u8>,
    pub mask_webp: Vec<u8>,
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn horde_default(req: &crate::horde::InpaintRequest, ctx: &JobCtx) -> Result<crate::horde::InpaintResult, String> {
    crate::horde::inpaint(&crate::horde::UreqTransport::default(), req, ctx)
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn capture(app: &PhotocraftApp, need: &str) -> Result<Capture, String> {
    let st = app.session.active().ok_or_else(|| need.to_string())?;
    let sel = st.doc.selection.as_ref().ok_or_else(|| need.to_string())?;
    let hole = sel.content_bounds().intersect(&st.doc.bounds());
    if hole.is_empty() {
        return Err(need.into());
    }
    let window = horde_window(hole, st.doc.bounds());
    let w = window.width();
    let h = window.height();
    if w == 0 || h == 0 {
        return Err(need.into());
    }
    let buf = photocraft_compose::render(&st.doc, window).over_background([0.0, 0.0, 0.0]);
    let rgba = buf.to_rgba8();
    let mut mask = vec![0u8; (w as usize).saturating_mul(h as usize).saturating_mul(3)];
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let cover = sel.sample_channel(window.x0.saturating_add(x), window.y0.saturating_add(y), 0);
            if cover > 0.5 {
                let i = (y as usize).saturating_mul(w as usize).saturating_add(x as usize).saturating_mul(3);
                if let Some(p) = mask.get_mut(i..i + 3) {
                    p.fill(255);
                }
            }
        }
    }
    let image = Image::from_u8(w, h, ChannelLayout::Rgba, rgba.pixels).map_err(|e| e.to_string())?;
    let mask_img = Image::from_u8(w, h, ChannelLayout::Rgb, mask).map_err(|e| e.to_string())?;
    let opts = EncodeOptions::default();
    let image_webp = photocraft_codecs::encode(&image, Format::WebP, &opts).map_err(|e| e.to_string())?;
    let mask_webp = photocraft_codecs::encode(&mask_img, Format::WebP, &opts).map_err(|e| e.to_string())?;
    Ok(Capture { origin: [window.x0, window.y0], width: w, height: h, image_webp, mask_webp })
}

#[cfg(not(target_arch = "wasm32"))]
fn horde_window(hole: Rect, canvas: Rect) -> Rect {
    let long = hole.width().max(hole.height()) as i64;
    let margin = i32::try_from(long * 3 / 4).unwrap_or(i32::MAX).clamp(32, 256);
    let grown = hole.inflate(margin).intersect(&canvas);
    let w = crate::horde::snap_side(grown.width().max(1)) as i32;
    let h = crate::horde::snap_side(grown.height().max(1)) as i32;
    let cx = hole.x0.saturating_add(hole.width() as i32 / 2);
    let cy = hole.y0.saturating_add(hole.height() as i32 / 2);
    let mut x0 = cx.saturating_sub(w / 2);
    let mut y0 = cy.saturating_sub(h / 2);
    if x0 < canvas.x0 {
        x0 = canvas.x0;
    }
    if y0 < canvas.y0 {
        y0 = canvas.y0;
    }
    if x0.saturating_add(w) > canvas.x1 {
        x0 = canvas.x1.saturating_sub(w);
    }
    if y0.saturating_add(h) > canvas.y1 {
        y0 = canvas.y1.saturating_sub(h);
    }
    Rect::new(x0, y0, x0.saturating_add(w), y0.saturating_add(h))
}

#[cfg(all(test, not(target_arch = "wasm32")))]
use std::cell::RefCell;

#[cfg(all(test, not(target_arch = "wasm32")))]
thread_local! {
    static STUB: RefCell<Option<InpaintFn>> = const { RefCell::new(None) };
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn take_stub() -> Option<InpaintFn> {
    #[cfg(test)]
    {
        STUB.with(|s| *s.borrow())
    }
    #[cfg(not(test))]
    {
        None
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
pub fn set_inpaint_stub(f: Option<InpaintFn>) {
    STUB.with(|s| *s.borrow_mut() = f);
}

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_engine::Session;

    fn app_with_selection() -> PhotocraftApp {
        let mut app = PhotocraftApp::new(Session::new(), crate::Services::default());
        app.run("file.new", json!({"width": 64, "height": 48, "background": "white"})).unwrap();
        app.run("select.rect", json!({"x": 8, "y": 8, "width": 16, "height": 16})).unwrap();
        app
    }

    #[test]
    fn menu_opens_a_prompt_dialog() {
        let mut app = app_with_selection();
        let r = crate::menus::invoke(&mut app, &egui::Context::default(), COMMAND, json!({})).unwrap();
        let id = r["dialog"].as_u64().unwrap();
        let d = app.ui.dialogs.iter().find(|d| d.id == id).unwrap();
        assert!(owns(&d.fields));
        assert_eq!(d.fields.get("__command").and_then(Value::as_str), Some(COMMAND));
    }

    #[test]
    fn empty_prompt_is_an_error() {
        let mut app = app_with_selection();
        let f = fields(&app);
        assert!(confirm(&mut app, &f).unwrap_err().contains("prompt"));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn generate_places_a_new_layer() {
        fn stub(req: &crate::horde::InpaintRequest, _ctx: &JobCtx) -> Result<crate::horde::InpaintResult, String> {
            assert_eq!(req.prompt, "a red square");
            let img = Image::from_u8(req.width, req.height, ChannelLayout::Rgba, vec![220u8; req.width as usize * req.height as usize * 4])
                .map_err(|e| e.to_string())?;
            let bytes = photocraft_codecs::encode(&img, Format::Png, &EncodeOptions::default()).map_err(|e| e.to_string())?;
            Ok(crate::horde::InpaintResult { bytes })
        }
        set_inpaint_stub(Some(stub));
        let mut app = app_with_selection();
        app.background_jobs = false;
        let mut f = fields(&app);
        f.insert("prompt".into(), json!("a red square"));
        let r = confirm(&mut app, &f);
        set_inpaint_stub(None);
        let r = r.expect("generate");
        // Inline start_job still returns Done, or a job we wait on.
        if let Some(id) = r.get("job").and_then(Value::as_u64) {
            app.session.wait_job(photocraft_engine::jobs::JobId(id)).unwrap();
        }
        let st = app.session.active().unwrap();
        assert!(st.doc.walk().len() >= 2, "expected a new layer");
        assert_eq!(st.history.undo_label(), Some("Generative Fill"));
    }
}
