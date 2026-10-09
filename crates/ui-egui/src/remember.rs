//! Last-used tool options, Export As / Save for Web settings and font recents
//! survive a quit. They live in [`photocraft_engine::prefs::Preferences::dialogs`]
//! like Fill and Liquify, so they round-trip with the rest of Preferences.

use photocraft_engine::BrushSettings;
use serde_json::{Map, Value, json};

use crate::PhotocraftApp;
use crate::paint_mouse;
use crate::state::{Tool, ToolOptions};

pub const TOOL_OPTIONS: &str = "toolOptions";
pub const EXPORT_AS: &str = "file.export.exportAs";
pub const SAVE_FOR_WEB: &str = "file.export.saveForWebLegacy";
pub const RECENT_FONTS: &str = "type.recentFontList";

const EXPORT_KEYS: &[&str] = &["format", "quality", "lossless", "transparency", "scale", "metadata"];
const EXPORT_FORMATS: &[&str] = &["png", "jpg", "webp", "tif", "tga"];

/// Restore remembered tool options, per-tool brushes and Save for Web settings after
/// preferences load.
pub fn restore(app: &mut PhotocraftApp) {
    restore_tools(app);
    restore_last_web(app);
}

/// Write changed tool options once the pointer is up (same cadence as the dock layout).
pub fn persist(app: &mut PhotocraftApp, ctx: &egui::Context) {
    if ctx.input(|i| i.pointer.any_down()) {
        return;
    }
    persist_tools(app);
}

pub(crate) fn persist_tools(app: &mut PhotocraftApp) {
    let mut brushes = app.ui.tool_brushes.clone();
    // `brush_tool` owns the live session brush, even when the current tool has no picker
    // (Move, Magic Wand, Type…): switching away from a painting tool does not save first.
    if paint_mouse::has_brush_picker(app.ui.brush_tool) {
        let brush = app.session.tools.brush.clone();
        match brushes.iter_mut().find(|(t, _)| *t == app.ui.brush_tool) {
            Some(e) => e.1 = brush,
            None => brushes.push((app.ui.brush_tool, brush)),
        }
    }
    let Ok(options) = serde_json::to_value(&app.ui.tool_options) else { return };
    let Ok(brushes_v) = serde_json::to_value(&brushes) else { return };
    let Ok(brush_tool) = serde_json::to_value(app.ui.brush_tool) else { return };
    let value = json!({"options": options, "brushes": brushes_v, "brushTool": brush_tool});
    if app.session.prefs().dialogs.get(TOOL_OPTIONS) != Some(&value) {
        app.session.prefs.edit(|p| p.dialogs.insert(TOOL_OPTIONS.into(), value));
    }
}

fn restore_tools(app: &mut PhotocraftApp) {
    let Some(v) = app.session.prefs().dialogs.get(TOOL_OPTIONS).cloned() else { return };
    if let Some(o) = v.get("options").cloned().and_then(|o| serde_json::from_value::<ToolOptions>(o).ok()) {
        app.ui.tool_options = o;
    }
    if let Some(b) = v.get("brushes").cloned().and_then(|b| serde_json::from_value::<Vec<(Tool, BrushSettings)>>(b).ok()) {
        app.ui.tool_brushes = b;
    }
    paint_mouse::load_tool_brush(app);
}

fn restore_last_web(app: &mut PhotocraftApp) {
    if app.session.file_menu.last_web.is_some() {
        return;
    }
    if let Some(v) = app.session.prefs().dialogs.get(SAVE_FOR_WEB).cloned() {
        app.session.file_menu.last_web = Some(v);
    }
}

/// Overlay last Export As choices onto a freshly opened dialog.
pub fn apply_export_as(app: &PhotocraftApp, f: &mut Map<String, Value>) {
    let Some(Value::Object(saved)) = app.session.prefs().dialogs.get(EXPORT_AS) else { return };
    for k in EXPORT_KEYS {
        if let Some(v) = saved.get(*k).filter(|v| export_value_ok(k, v)) {
            f.insert((*k).into(), v.clone());
        }
    }
}

/// Remember Export As fields (not the destination path).
pub fn remember_export_as(app: &mut PhotocraftApp, f: &Map<String, Value>) {
    let mut remembered = Map::new();
    for k in EXPORT_KEYS {
        if let Some(v) = f.get(*k).filter(|v| export_value_ok(k, v)) {
            remembered.insert((*k).into(), v.clone());
        }
    }
    app.session.prefs.edit(|p| p.dialogs.insert(EXPORT_AS.into(), Value::Object(remembered)));
}

fn export_value_ok(k: &str, v: &Value) -> bool {
    match k {
        "format" => v.as_str().is_some_and(|s| EXPORT_FORMATS.contains(&s)),
        "quality" => v.as_f64().is_some_and(|n| (1.0..=100.0).contains(&n)),
        "lossless" | "transparency" => v.is_boolean(),
        "scale" => v.as_f64().is_some_and(|n| (1.0..=1000.0).contains(&n)),
        "metadata" => v.as_str().is_some_and(|s| matches!(s, "none" | "all")),
        _ => false,
    }
}

/// Remember Save for Web settings, dropping the destination so a later launch can reuse them.
pub fn remember_save_for_web(app: &mut PhotocraftApp, p: &Value) {
    let mut v = p.clone();
    if let Value::Object(m) = &mut v {
        m.remove("path");
        m.remove("dir");
        m.remove("numbers");
    }
    app.session.prefs.edit(|p| p.dialogs.insert(SAVE_FOR_WEB.into(), v));
}

/// Recently used type families, newest first, capped by Preferences › Type › Recent fonts.
pub fn recent_fonts(app: &PhotocraftApp) -> Vec<String> {
    let cap = app.session.prefs().type_.recent_fonts as usize;
    app.session
        .prefs()
        .dialogs
        .get(RECENT_FONTS)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).filter(|s| !s.is_empty()).take(cap).map(str::to_string).collect())
        .unwrap_or_default()
}

/// Record a family the user just picked. An empty cap (0) clears the list.
pub fn remember_font(app: &mut PhotocraftApp, family: &str) {
    if family.is_empty() {
        return;
    }
    let cap = app.session.prefs().type_.recent_fonts as usize;
    let mut list = recent_fonts(app);
    list.retain(|f| f != family);
    if cap == 0 {
        list.clear();
    } else {
        list.insert(0, family.to_string());
        list.truncate(cap);
    }
    app.session.prefs.edit(|p| p.dialogs.insert(RECENT_FONTS.into(), json!(list)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_engine::Session;
    use serde_json::json;

    fn app() -> PhotocraftApp {
        PhotocraftApp::new(Session::new(), Default::default())
    }

    fn round_trip(src: &PhotocraftApp) -> PhotocraftApp {
        let saved = src.session.prefs_to_json();
        let mut s = Session::new();
        s.load_prefs_json(&saved).unwrap();
        let mut again = PhotocraftApp::new(s, Default::default());
        restore(&mut again);
        again
    }

    #[test]
    fn tool_options_and_brushes_survive_a_restart() {
        let mut app = app();
        app.ui.tool_options.tolerance = 12.0;
        app.ui.tool_options.contiguous = false;
        app.ui.tool_options.type_font = "Source Serif".into();
        app.ui.tool_options.move_auto_select = false;
        app.session.tools.brush.size = 37.0;
        app.session.tools.brush.hardness = 0.25;
        persist_tools(&mut app);

        let again = round_trip(&app);
        assert_eq!(again.ui.tool_options.tolerance, 12.0);
        assert!(!again.ui.tool_options.contiguous);
        assert_eq!(again.ui.tool_options.type_font, "Source Serif");
        assert!(!again.ui.tool_options.move_auto_select);
        assert_eq!(again.session.tools.brush.size, 37.0);
        assert!((again.session.tools.brush.hardness - 0.25).abs() < f32::EPSILON);
    }

    #[test]
    fn the_live_brush_saves_while_a_non_painting_tool_is_active() {
        let mut src = app();
        src.session.tools.brush.size = 19.0;
        src.ui.tool = Tool::Move;
        persist_tools(&mut src);
        let again = round_trip(&src);
        assert_eq!(again.session.tools.brush.size, 19.0);
    }

    #[test]
    fn corrupt_tool_options_keep_the_defaults() {
        let mut app = app();
        let defaults = app.ui.tool_options.clone();
        app.session.prefs.edit(|p| p.dialogs.insert(TOOL_OPTIONS.into(), json!("nope")));
        restore(&mut app);
        assert_eq!(app.ui.tool_options, defaults);
        app.session.prefs.edit(|p| p.dialogs.insert(TOOL_OPTIONS.into(), json!({"options": {"tolerance": "lots"}})));
        restore(&mut app);
        assert_eq!(app.ui.tool_options, defaults);
    }

    #[test]
    fn export_as_and_save_for_web_remember_their_fields() {
        let mut app = app();
        let mut f = Map::new();
        f.insert("format".into(), json!("jpg"));
        f.insert("quality".into(), json!(72.0));
        f.insert("transparency".into(), json!(false));
        f.insert("scale".into(), json!(50.0));
        f.insert("metadata".into(), json!("all"));
        remember_export_as(&mut app, &f);

        let mut opened = Map::new();
        opened.insert("format".into(), json!("png"));
        opened.insert("quality".into(), json!(85.0));
        opened.insert("scale".into(), json!(100.0));
        opened.insert("metadata".into(), json!("none"));
        apply_export_as(&app, &mut opened);
        assert_eq!(opened["format"], json!("jpg"));
        assert_eq!(opened["quality"], json!(72.0));
        assert_eq!(opened["scale"], json!(50.0));
        assert_eq!(opened["metadata"], json!("all"));

        remember_save_for_web(&mut app, &json!({"format": "gif", "colors": 32, "path": "/tmp/out.gif"}));
        let again = round_trip(&app);
        let last = again.session.file_menu.last_web.expect("restored");
        assert_eq!(last["format"], json!("gif"));
        assert_eq!(last["colors"], json!(32));
        assert!(last.get("path").is_none());
    }

    #[test]
    fn export_as_ignores_corrupt_remembered_values() {
        let mut app = app();
        app.session.prefs.edit(|p| {
            p.dialogs.insert(EXPORT_AS.into(), json!({"format": "bmp", "quality": 900, "scale": 0, "metadata": "exif"}));
        });
        let mut f = Map::new();
        f.insert("format".into(), json!("png"));
        f.insert("quality".into(), json!(85.0));
        f.insert("scale".into(), json!(100.0));
        f.insert("metadata".into(), json!("none"));
        apply_export_as(&app, &mut f);
        assert_eq!(f["format"], json!("png"));
        assert_eq!(f["quality"], json!(85.0));
        assert_eq!(f["scale"], json!(100.0));
        assert_eq!(f["metadata"], json!("none"));
    }

    #[test]
    fn font_recents_are_capped_by_the_type_preference() {
        let mut app = app();
        app.session.prefs.edit(|p| p.type_.recent_fonts = 2);
        remember_font(&mut app, "Inter");
        remember_font(&mut app, "Source Serif");
        remember_font(&mut app, "IBM Plex Sans");
        remember_font(&mut app, "Source Serif");
        assert_eq!(recent_fonts(&app), vec!["Source Serif".to_string(), "IBM Plex Sans".to_string()]);
        let mut again = round_trip(&app);
        assert_eq!(recent_fonts(&again), vec!["Source Serif".to_string(), "IBM Plex Sans".to_string()]);
        again.session.prefs.edit(|p| p.type_.recent_fonts = 1);
        // Display honours a smaller cap without rewriting the store.
        assert_eq!(recent_fonts(&again), vec!["Source Serif".to_string()]);
    }
}
