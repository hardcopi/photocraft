//! Copy layers into another open document (#589): what Photoshop does when layers are dragged from
//! the Layers panel, or with the Move tool from the canvas, onto another document's tab or window.
//! Photoshop has no menu item for it; `layer.copyToDocument` is the command the drag dispatches and
//! agents call.
//!
//! The copies keep their names, content, masks, styles and blending; they land above the
//! destination's active layer as one history step there, and the destination becomes the active
//! document. Pixels are converted to the destination's colour profile (Color Settings' intent and
//! black point compensation) and bit depth, as Photoshop converts dragged layers.

use photocraft_color::ColorMode;
use photocraft_doc::{Document, LayerId, Pattern};
use photocraft_geom::Rect;
use serde_json::{Value, json};

use crate::commands::CommandSpec;
use crate::{EngineError, Result, Session};

/// Whole layers on the session clipboard (Edit › Copy with no pixel selection).
#[derive(Clone, Debug)]
pub struct LayerClip {
    /// Scratch document holding the duplicated layers, in the source's colour.
    pub copies: Document,
    pub patterns: Vec<Pattern>,
    /// Indices into `copies.layers` that were the source Background (renamed on insert).
    pub from_background: Vec<usize>,
}

const CMD: &str = "layer.copyToDocument";

fn bad(msg: impl Into<String>) -> EngineError {
    EngineError::BadParams { cmd: CMD.into(), msg: msg.into() }
}

fn enabled(s: &Session) -> std::result::Result<(), String> {
    let d = s.active().ok_or("no document open")?;
    d.active_layer.ok_or("no active layer")?;
    if s.documents().len() < 2 { Err("no other document is open".into()) } else { Ok(()) }
}

/// A document index param (`None` when absent), checked against the open documents.
fn doc_index(s: &Session, p: &Value, key: &str) -> Result<Option<usize>> {
    let Some(v) = p.get(key) else { return Ok(None) };
    let i = v.as_u64().and_then(|i| usize::try_from(i).ok()).ok_or_else(|| bad(format!("`{key}` must be a document index")))?;
    if i >= s.documents().len() {
        return Err(bad(format!("no document {i} (there are {})", s.documents().len())));
    }
    Ok(Some(i))
}

/// An `[x, y]` param of finite numbers within ±10⁷ pixels.
fn point(p: &Value, key: &str) -> Result<Option<[f64; 2]>> {
    let Some(v) = p.get(key) else { return Ok(None) };
    let xy = v.as_array().filter(|a| a.len() == 2).and_then(|a| Some([a[0].as_f64()?, a[1].as_f64()?]));
    match xy {
        Some(xy) if xy.iter().all(|c| c.is_finite() && c.abs() <= 1e7) => Ok(Some(xy)),
        _ => Err(bad(format!("`{key}` must be [x, y] (pixels)"))),
    }
}

/// Modes whose layers convert through the colour engine.
fn layered(mode: ColorMode) -> bool {
    matches!(mode, ColorMode::Rgb | ColorMode::Grayscale | ColorMode::Cmyk | ColorMode::Lab)
}

fn center(r: Rect) -> [f64; 2] {
    [(f64::from(r.x0) + f64::from(r.x1)) / 2.0, (f64::from(r.y0) + f64::from(r.y1)) / 2.0]
}

/// Duplicate `ids` (top-level only) into a clipboard payload. A Background becomes an ordinary layer.
pub(crate) fn pack_layers(sdoc: &Document, ids: &[LayerId]) -> Result<LayerClip> {
    if !layered(sdoc.mode) {
        return Err(EngineError::Other(format!("layers can't be copied with a {:?} source document", sdoc.mode)));
    }
    if let Some(missing) = ids.iter().find(|id| sdoc.layer(**id).is_none()) {
        return Err(EngineError::NoLayer(*missing));
    }
    let ids = crate::layer_multi_cmds::top_level(sdoc, ids);
    if ids.is_empty() {
        return Err(EngineError::Other("no layers to copy".into()));
    }
    let mut copies = Document::new("", sdoc.size, sdoc.mode, sdoc.depth);
    copies.icc_profile = sdoc.icc_profile.clone();
    // Type re-lays out at the source's resolution, so it keeps its size in pixels.
    copies.resolution_dpi = sdoc.resolution_dpi;
    let mut from_background = Vec::new();
    for id in &ids {
        let src_layer = sdoc.layer(*id).ok_or(EngineError::NoLayer(*id))?;
        let mut l = src_layer.duplicate();
        if crate::extra_cmds::is_background(src_layer) && sdoc.layers.first().is_some_and(|b| b.id == *id) {
            l.locks = Default::default();
            from_background.push(copies.layers.len());
        }
        copies.layers.push(l);
    }
    Ok(LayerClip { copies, patterns: sdoc.patterns.clone(), from_background })
}

/// Insert `clip` above the destination's active layer. `restore_active` is the document to
/// reactivate when the edit fails (the source of a drag).
pub(crate) fn insert_layers(s: &mut Session, dest: usize, clip: &LayerClip, dx: i32, dy: i32, label: &str, restore_active: Option<usize>) -> Result<Value> {
    let ddoc = s.documents().get(dest).ok_or(EngineError::NoDocument)?.doc.clone();
    if !layered(ddoc.mode) {
        return Err(EngineError::Other(format!("layers can't be copied with a {:?} destination document", ddoc.mode)));
    }
    let mut copies = clip.copies.clone();
    if (copies.mode, &copies.icc_profile) != (ddoc.mode, &ddoc.icc_profile) {
        let profile = crate::color_cmds::document_profile(&ddoc);
        crate::color_cmds::convert_document(&mut copies, &profile, s.color.settings.intent(), s.color.settings.bpc)?;
    }
    if copies.depth != ddoc.depth {
        crate::image_cmds::for_each_surface(&mut copies.layers, true, &mut |surf, _| {
            let f = surf.format().with_sample(ddoc.depth);
            *surf = surf.convert(f);
        });
    }
    if dx != 0 || dy != 0 {
        let snapshot = copies.clone();
        for l in &mut copies.layers {
            crate::commands::translate_layer(&snapshot, l, dx, dy);
            crate::vector_cmds::translate_vectors(&snapshot, l, f64::from(dx), f64::from(dy));
        }
    }
    let patterns: Vec<_> = clip.patterns.iter().filter(|pat| !ddoc.patterns.iter().any(|d| d.id == pat.id)).cloned().collect();
    let from_background = clip.from_background.clone();
    s.set_active(dest);
    let edited = s.edit(label, |doc, active| {
        doc.patterns.extend(patterns);
        let mut above = *active;
        let mut new = Vec::with_capacity(copies.layers.len());
        for (i, mut l) in copies.layers.into_iter().enumerate() {
            if from_background.contains(&i) {
                l.name = doc.next_layer_name("Layer");
                l.locks = Default::default();
            }
            let id = doc.insert_above(above, l);
            above = Some(id);
            new.push(id);
        }
        *active = above;
        Ok(new)
    });
    if edited.is_err()
        && let Some(src) = restore_active
    {
        s.set_active(src);
    }
    let new = edited?;
    let last = new.last().copied();
    crate::layer_multi_cmds::reselect(s, new.clone(), last);
    Ok(json!({"document": dest, "layers": new.iter().map(|l| l.0).collect::<Vec<_>>(), "offset": [dx, dy]}))
}

fn content_bounds(clip: &LayerClip) -> Rect {
    let bounds = clip.copies.layers.iter().filter_map(crate::layer_multi_cmds::layer_bounds).fold(Rect::EMPTY, |a, b| a.union(&b));
    if bounds.is_empty() { clip.copies.bounds() } else { bounds }
}

/// Paste the layer clipboard into the active document, or make a document when none is open.
pub(crate) fn paste_clip(s: &mut Session, p: &Value, in_place: bool) -> Result<Value> {
    let clip = s.layer_clipboard.clone().ok_or(EngineError::Other("the clipboard is empty".into()))?;
    let Some(dest) = s.active_index() else {
        return new_from_clip(s);
    };
    let canvas = s.documents()[dest].doc.bounds();
    let content = content_bounds(&clip);
    let (dx, dy) = if in_place || (content.intersect(&canvas) == content && p.get("center").is_none()) {
        (0, 0)
    } else {
        let c = p.get("center").and_then(Value::as_array).filter(|a| a.len() >= 2).map(|a| (a[0].as_f64().unwrap_or(0.0), a[1].as_f64().unwrap_or(0.0)));
        let (cx, cy) = c.unwrap_or(((canvas.x0 + canvas.x1) as f64 / 2.0, (canvas.y0 + canvas.y1) as f64 / 2.0));
        let mid = center(content);
        ((cx - mid[0]).round() as i32, (cy - mid[1]).round() as i32)
    };
    insert_layers(s, dest, &clip, dx, dy, "Paste", None)
}

/// A new document holding the layer clipboard, at the source canvas size.
pub(crate) fn new_from_clip(s: &mut Session) -> Result<Value> {
    let clip = s.layer_clipboard.clone().ok_or(EngineError::Other("the clipboard is empty".into()))?;
    let src = &clip.copies;
    let (w, h) = (src.size.width, src.size.height);
    if w == 0 || h == 0 {
        return Err(EngineError::Other("the clipboard image has no size".into()));
    }
    let mut doc = Document::new("Untitled", src.size, src.mode, src.depth);
    doc.icc_profile = src.icc_profile.clone();
    doc.resolution_dpi = src.resolution_dpi;
    doc.layers = src.layers.clone();
    doc.patterns = clip.patterns.clone();
    for i in clip.from_background.clone() {
        let name = doc.next_layer_name("Layer");
        if let Some(l) = doc.layers.get_mut(i) {
            l.name = name;
            l.locks = Default::default();
        }
    }
    let i = s.add_document(doc, None);
    Ok(json!({"document": i, "width": w, "height": h}))
}

/// `layer.copyToDocument`.
fn copy_to_document(s: &mut Session, p: &Value) -> Result<Value> {
    let dest = doc_index(s, p, "document")?.ok_or_else(|| bad("missing `document` (the destination's index)"))?;
    let src = match doc_index(s, p, "source")? {
        Some(i) => i,
        None => s.active_index().ok_or(EngineError::NoDocument)?,
    };
    if src == dest {
        return Err(bad("the destination is the source document (use layer.duplicate)"));
    }
    let (from, to) = (&s.documents()[src], &s.documents()[dest]);
    let (sdoc, ddoc) = (from.doc.clone(), to.doc.clone());
    let ids: Vec<LayerId> = match p.get("layers") {
        None => from.selected_layers(),
        Some(v) => v
            .as_array()
            .map(|a| a.iter().map(|id| id.as_u64().map(LayerId)).collect::<Option<Vec<_>>>())
            .and_then(|ids| ids)
            .ok_or_else(|| bad("`layers` must be an array of layer ids"))?,
    };
    if let Some(missing) = ids.iter().find(|id| sdoc.layer(**id).is_none()) {
        return Err(EngineError::NoLayer(*missing));
    }
    let ids = crate::layer_multi_cmds::top_level(&sdoc, &ids);
    if ids.is_empty() {
        return Err(bad("no layers to copy"));
    }
    for (doc, role) in [(&sdoc, "source"), (&ddoc, "destination")] {
        if !layered(doc.mode) {
            return Err(EngineError::Other(format!("layers can't be copied with a {:?} {role} document", doc.mode)));
        }
    }
    let clip = pack_layers(&sdoc, &ids)?;
    // Placement: centred on the destination's canvas, centred on a point, or offset from where
    // the layers are in the source (default: the same canvas position).
    let content = content_bounds(&clip);
    let (dx, dy) = if p.get("center").and_then(Value::as_bool).unwrap_or(false) {
        let (c, d) = (center(content), center(ddoc.bounds()));
        (d[0] - c[0], d[1] - c[1])
    } else if let Some(at) = point(p, "at")? {
        let c = center(content);
        (at[0] - c[0], at[1] - c[1])
    } else {
        point(p, "offset")?.map_or((0.0, 0.0), |o| (o[0], o[1]))
    };
    let (dx, dy) = (dx.round() as i32, dy.round() as i32);
    let label = if ids.len() == 1 { "Duplicate Layer" } else { "Duplicate Layers" };
    insert_layers(s, dest, &clip, dx, dy, label, Some(src))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: CMD,
        label: "Copy Layers to Document",
        menu: &[],
        shortcut: None,
        params: r##"{"document":index (destination),"layers":[id,…]?=the source's selected layers,"source":index?=active,"center":bool=false (centre on the destination's canvas),"at":[x,y]? (centre the copies on this point),"offset":[dx,dy]?=[0,0] (from their place in the source)}"##,
        enabled,
        run: copy_to_document,
        journal: true,
    }]
}

#[cfg(test)]
mod tests;
