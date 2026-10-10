//! The rest of Photoshop's Edit menu: Fade, Purge, Content-Aware Fill and Scale, Define Brush
//! Preset / Custom Shape, Find and Replace Text, the Preset Manager and preset export/import.
//! Auto-Align and Auto-Blend Layers live in [`crate::align_cmds`].
//!
//! Check Spelling is not implemented: it needs a word list per language, and no permissively
//! licensed dictionary small enough to bundle (or a pure-Rust checker with one) was available.

use photocraft_color::{BlendMode, PixelFormat};
use photocraft_doc::vector::Path;
use photocraft_doc::{DocId, Document, Layer, LayerContent, LayerId};
use photocraft_geom::Rect;
use photocraft_raster::{Surface, from_rgba_into, to_rgba};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::commands::{CommandSpec, blend_from_str, int, layer_param};
use crate::presets::{Group, Named};
use crate::{EngineError, Result, Session};

// ------------------------------------------------------------------ state

/// What Edit › Fade would fade: the result of the last filter, adjustment or paint command.
#[derive(Clone, Debug, PartialEq)]
pub struct FadeSource {
    pub doc: DocId,
    /// Document revision right after the command; any later change disables Fade.
    pub revision: u64,
    pub layer: LayerId,
    pub mask: bool,
    pub label: String,
}

/// A custom shape (Edit › Define Custom Shape), usable by the Custom Shape tool.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CustomShape {
    pub name: String,
    pub path: Path,
}

/// Edit-menu state kept by the session.
#[derive(Clone, Debug, Default)]
pub struct EditState {
    pub fade: Option<FadeSource>,
    /// Custom shape library (the vector tools read it).
    pub custom_shapes: Vec<CustomShape>,
    /// Find and Replace Text position: (layer, byte offset after the current match).
    pub find_cursor: Option<(LayerId, usize, usize)>,
}

/// Commands whose result Edit › Fade can blend back against the previous state.
fn fadeable(id: &str) -> bool {
    id.starts_with("filter.")
        || id.starts_with("image.adjustments.")
        || id.starts_with("paint.")
        || matches!(id, "edit.fill" | "edit.stroke" | "edit.contentAwareFill" | "image.autoTone" | "image.autoContrast" | "image.autoColor")
}

/// Called after every successful command: remember a fadeable result.
pub(crate) fn after_command(s: &mut Session, id: &str) {
    if !fadeable(id) {
        return;
    }
    let Some(st) = s.active() else { return };
    let Some(layer) = st.active_layer else { return };
    let label = st.history.undo_label().unwrap_or(id).to_string();
    let mask = st.doc.layer(layer).is_some_and(|l| l.mask.is_some())
        && st.history.state(st.history.past_len().wrapping_sub(1)).is_some_and(|prev| {
            // The mask changed and the pixels didn't: the command painted the mask.
            let now = st.doc.layer(layer);
            let was = prev.layer(layer);
            let surf_eq = |a: Option<&Surface>, b: Option<&Surface>| match (a, b) {
                (Some(a), Some(b)) => crate_fingerprint(a) == crate_fingerprint(b),
                (None, None) => true,
                _ => false,
            };
            surf_eq(now.and_then(|l| l.surface()), was.and_then(|l| l.surface()))
                && !surf_eq(now.and_then(|l| l.mask.as_ref().map(|m| &m.surface)), was.and_then(|l| l.mask.as_ref().map(|m| &m.surface)))
        });
    s.edit_state.fade = Some(FadeSource { doc: st.doc.id, revision: st.revision, layer, mask, label });
}

/// Tile identity of a surface (pointer equality of its COW tiles).
fn crate_fingerprint(s: &Surface) -> u64 {
    s.tiles().fold(s.tile_count() as u64, |h, (c, t)| {
        (h ^ std::sync::Arc::as_ptr(t) as usize as u64 ^ ((c.tx as u64) << 32 | c.ty as u32 as u64)).wrapping_mul(0x100_0000_01b3)
    })
}

// ------------------------------------------------------------------ helpers

fn bad(cmd: &str, msg: impl Into<String>) -> EngineError {
    EngineError::BadParams { cmd: cmd.into(), msg: msg.into() }
}

fn f32_or(p: &Value, k: &str, d: f32) -> f32 {
    p.get(k).and_then(Value::as_f64).map_or(d, |v| v as f32)
}

fn str_or<'a>(p: &'a Value, k: &str, d: &'a str) -> &'a str {
    p.get(k).and_then(Value::as_str).unwrap_or(d)
}

fn bool_or(p: &Value, k: &str, d: bool) -> bool {
    p.get(k).and_then(Value::as_bool).unwrap_or(d)
}

fn always(_: &Session) -> std::result::Result<(), String> {
    Ok(())
}

fn pixel_layer(s: &Session) -> std::result::Result<LayerId, String> {
    let d = s.active().ok_or("no document open")?;
    let id = d.active_layer.ok_or("no active layer")?;
    let l = d.doc.layer(id).ok_or("no active layer")?;
    if !matches!(l.content, LayerContent::Raster(_)) {
        return Err(format!("active layer is {} {} layer, not a pixel layer", l.content.article(), l.content.kind_name()));
    }
    let locks = d.doc.effective_locks(id);
    if locks.all || locks.pixels {
        return Err(format!("the layer \"{}\" is locked", l.name));
    }
    Ok(id)
}

fn writable_surface(doc: &mut Document, id: LayerId) -> Result<&mut Surface> {
    let locks = doc.effective_locks(id);
    let l = doc.layer_mut(id).ok_or(EngineError::NoLayer(id))?;
    if locks.all || locks.pixels {
        return Err(EngineError::Other(format!("Could not complete your request because the layer \"{}\" is locked", l.name)));
    }
    l.surface_mut().ok_or_else(|| EngineError::Other("not a pixel layer".into()))
}

// ------------------------------------------------------------------ Fade

fn can_fade(s: &Session) -> std::result::Result<(), String> {
    let st = s.active().ok_or("no document open")?;
    let f = s.edit_state.fade.as_ref().ok_or("nothing to fade")?;
    if f.doc != st.doc.id || f.revision != st.revision || !st.history.can_undo() {
        return Err("Fade is only available right after a filter, adjustment or painting step".into());
    }
    if st.doc.layer(f.layer).is_none() {
        return Err("the faded layer no longer exists".into());
    }
    Ok(())
}

fn surface_of(doc: &Document, id: LayerId, mask: bool) -> Option<&Surface> {
    let l = doc.layer(id)?;
    if mask { l.mask.as_ref().map(|m| &m.surface) } else { l.surface() }
}

/// Blend `after` back towards `before` over `area`: `before + opacity × (B(before, after) − before)`
/// with blend mode `mode` (premultiplied, so alpha changes fade too).
pub fn fade_surface(before: &Surface, after: &mut Surface, area: Rect, mode: BlendMode, opacity: f32) {
    let fmt = after.format();
    let n = fmt.channels();
    if area.is_empty() || n == 0 {
        return;
    }
    let b = before.read_region(area);
    let mut a = after.read_region(area);
    let k = opacity.clamp(0.0, 1.0);
    let lerp_premul = |pb: &[f32], pa: &[f32], out: &mut [f32], alpha: bool| {
        let m = out.len();
        if !alpha {
            for c in 0..m {
                out[c] = pb[c] + (pa[c] - pb[c]) * k;
            }
            return;
        }
        let (ab, aa) = (pb[m - 1], pa[m - 1]);
        let ao = ab + (aa - ab) * k;
        for c in 0..m - 1 {
            let pm = pb[c] * ab + (pa[c] * aa - pb[c] * ab) * k;
            out[c] = if ao > 1e-9 { pm / ao } else { 0.0 };
        }
        out[m - 1] = ao;
    };
    let mut tmp = vec![0.0f32; n];
    for (pa, pb) in a.chunks_exact_mut(n).zip(b.chunks_exact(n)) {
        if mode == BlendMode::Normal || n == 1 {
            lerp_premul(pb, pa, &mut tmp, fmt.alpha);
            pa.copy_from_slice(&tmp);
        } else {
            let rb = to_rgba(&fmt, pb);
            let ra = to_rgba(&fmt, pa);
            let blended = photocraft_color::blend::composite(mode, rb, ra, 1.0);
            let mut o = [0.0f32; 4];
            lerp_premul(&rb, &blended, &mut o, true);
            let mut enc = [0.0f32; 8];
            from_rgba_into(&fmt, o, &mut enc);
            pa.copy_from_slice(&enc[..n]);
        }
    }
    after.write_region(area, &a);
}

fn fade(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "edit.fade";
    let src = s.edit_state.fade.clone().ok_or_else(|| bad(cmd, "nothing to fade"))?;
    let opacity = f32_or(p, "opacity", 100.0).clamp(0.0, 100.0) / 100.0;
    let mode = blend_from_str(str_or(p, "mode", "normal")).ok_or_else(|| bad(cmd, "unknown blend mode"))?;
    let st = s.active().ok_or(EngineError::NoDocument)?;
    let prev = st.history.state(st.history.past_len() - 1).ok_or_else(|| bad(cmd, "no previous state"))?;
    let before = surface_of(&prev, src.layer, src.mask).cloned().ok_or_else(|| EngineError::Other("the layer did not exist before this step".into()))?;
    let now = surface_of(&st.doc, src.layer, src.mask).ok_or(EngineError::NoLayer(src.layer))?;
    if now.format() != before.format() {
        return Err(EngineError::Other("can't fade a step that changed the layer's pixel format".into()));
    }
    let area = before.content_bounds().union(&now.content_bounds());
    let label = format!("Fade {}", src.label);
    s.edit(&label, |doc, _| {
        let l = doc.layer_mut(src.layer).ok_or(EngineError::NoLayer(src.layer))?;
        let surf = if src.mask { l.mask.as_mut().map(|m| &mut m.surface) } else { l.surface_mut() }.ok_or(EngineError::NoLayer(src.layer))?;
        fade_surface(&before, surf, area, mode, opacity);
        surf.prune();
        Ok(())
    })?;
    Ok(json!({"faded": src.label, "opacity": opacity * 100.0, "mode": mode.label()}))
}

// ------------------------------------------------------------------ Purge

fn history_bytes(s: &Session, all: bool) -> usize {
    let docs: Vec<&crate::DocState> = if all { s.documents().iter().collect() } else { s.active().into_iter().collect() };
    docs.iter().map(|d| d.history.unique_bytes(&d.doc)).sum()
}

fn clip_bytes(s: &Session) -> usize {
    let pixels = s.clipboard.as_ref().map_or(0, |c| c.surface.tiles().map(|(_, t)| t.bytes().len()).sum());
    let layers = s.layer_clipboard.as_ref().map_or(0, |c| {
        c.copies.layers.iter().filter_map(photocraft_doc::Layer::surface).map(|surf| surf.tiles().map(|(_, t)| t.bytes().len()).sum::<usize>()).sum()
    });
    pixels + layers
}

fn purge(s: &mut Session, what: &str) -> Result<Value> {
    let mut freed = 0usize;
    let mut items: Vec<&str> = Vec::new();
    if matches!(what, "undo" | "all") && s.active().is_some_and(|d| d.history.can_undo()) {
        let before = history_bytes(s, false);
        if let Some(st) = s.active_mut() {
            st.history.purge_last();
        }
        freed += before.saturating_sub(history_bytes(s, false));
        items.push("undo");
    }
    if matches!(what, "clipboard" | "all") && (s.clipboard.is_some() || s.layer_clipboard.is_some() || s.style_clipboard.is_some()) {
        freed += clip_bytes(s);
        s.clipboard = None;
        s.layer_clipboard = None;
        s.style_clipboard = None;
        items.push("clipboard");
    }
    if matches!(what, "histories" | "all") {
        let b = history_bytes(s, true);
        if s.documents().iter().any(|d| d.history.can_undo() || d.history.can_redo()) {
            for st in &mut s.docs {
                st.history.clear();
                st.coalesce = None;
            }
            freed += b;
            items.push("histories");
        }
    }
    if what == "all" {
        let fx = photocraft_compose::purge_effect_cache();
        if fx > 0 {
            freed += fx;
            items.push("effect cache");
        }
        let masks = photocraft_compose::masks::purge_cache();
        if masks > 0 {
            freed = freed.saturating_add(masks);
            items.push("mask cache");
        }
    }
    s.edit_state.fade = None;
    let msg = if items.is_empty() { "nothing to purge".to_string() } else { format!("purged {}", items.join(", ")) };
    Ok(json!({"purged": items, "bytes": freed, "message": msg}))
}

fn can_purge_undo(s: &Session) -> std::result::Result<(), String> {
    s.active().filter(|d| d.history.can_undo()).map(|_| ()).ok_or_else(|| "nothing to purge".into())
}
fn can_purge_clipboard(s: &Session) -> std::result::Result<(), String> {
    if s.clipboard.is_some() || s.layer_clipboard.is_some() || s.style_clipboard.is_some() { Ok(()) } else { Err("the clipboard is empty".into()) }
}
fn can_purge_histories(s: &Session) -> std::result::Result<(), String> {
    if s.documents().iter().any(|d| d.history.can_undo() || d.history.can_redo()) { Ok(()) } else { Err("no history to purge".into()) }
}
fn can_purge_all(s: &Session) -> std::result::Result<(), String> {
    can_purge_histories(s).or_else(|_| can_purge_clipboard(s)).or_else(|_| {
        if photocraft_compose::effect_cache_bytes() > 0 || photocraft_compose::masks::cache_bytes() > 0 { Ok(()) } else { Err("nothing to purge".into()) }
    })
}

// ------------------------------------------------------------------ Content-Aware Fill

fn can_caf(s: &Session) -> std::result::Result<(), String> {
    pixel_layer(s)?;
    let d = s.active().ok_or("no document open")?;
    if d.doc.selection.as_ref().is_none_or(|m| m.content_bounds().is_empty()) {
        return Err("make a selection around what to remove".into());
    }
    Ok(())
}

/// Mask surface (gray) of an alpha channel by index or name.
fn channel_mask<'a>(doc: &'a Document, v: &Value) -> Option<&'a Surface> {
    match v {
        Value::Number(n) => doc.channels.get(n.as_u64()? as usize).map(|c| &c.surface),
        Value::String(name) => doc.channels.iter().find(|c| c.name == *name).map(|c| &c.surface),
        _ => None,
    }
}

/// `key` as an `[x, y, w, h]` rectangle. The saturating float→int casts bound each component;
/// the additions saturate too — `area: [1e30, 0, 1e30, 10]` is a whole-canvas window, not an
/// overflow (it panicked in debug builds before the `saturating_add`).
fn rect_param(p: &Value, key: &str, cmd: &str) -> Result<Rect> {
    let a = p.get(key).and_then(Value::as_array).ok_or_else(|| bad(cmd, format!("`{key}` must be [x, y, w, h]")))?;
    let n = |i: usize| {
        a.get(i)
            .and_then(Value::as_f64)
            .filter(|f| f.is_finite())
            .map(|v| v.round() as i32)
            .ok_or_else(|| bad(cmd, format!("`{key}` must be four finite numbers")))
    };
    let (x, y, w, h) = (n(0)?, n(1)?, n(2)?, n(3)?);
    Ok(Rect::new(x, y, x.saturating_add(w.max(0)), y.saturating_add(h.max(0))))
}

fn content_aware_fill(s: &mut Session, p: &Value) -> Result<Value> {
    content_aware_fill_as(s, p, "edit.contentAwareFill", "Content-Aware Fill")
}

/// Delete and Fill Selection (#1286): Photoshop's one-click removal from the selection-tool
/// context menu. Content-Aware Fill with its default settings into the layer, no dialog.
fn delete_and_fill(s: &mut Session, _: &Value) -> Result<Value> {
    content_aware_fill_as(s, &json!({}), "edit.deleteAndFillSelection", "Delete and Fill Selection")
}

fn content_aware_fill_as(s: &mut Session, p: &Value, cmd: &'static str, label: &'static str) -> Result<Value> {
    use photocraft_algo::content_aware::{FillOptions, color_level, fill_with, rotation_level};
    let id = pixel_layer(s).map_err(EngineError::Other)?;
    let st = s.active().ok_or(EngineError::NoDocument)?;
    let doc = st.doc.clone();
    let sel = doc.selection.clone().ok_or_else(|| bad(cmd, "no selection"))?;
    let hb = sel.content_bounds().intersect(&doc.bounds());
    if hb.is_empty() {
        return Err(bad(cmd, "the selection is outside the canvas"));
    }
    // The canvas is at most i32 wide, so the extent fits; saturate anyway rather than wrap (#963).
    let ext = i32::try_from(hb.width().max(hb.height())).unwrap_or(i32::MAX);
    let sampling = str_or(p, "sampling", "auto").to_string();
    let custom_mask: Option<Surface> = p.get("channel").and_then(|v| channel_mask(&doc, v)).cloned();
    let custom_rect = if p.get("area").is_some() { Some(rect_param(p, "area", cmd)?) } else { None };
    let window = match sampling.as_str() {
        "auto" => hb.inflate(crate::fill_cmds::sampling_margin(hb)),
        "rectangular" => hb.inflate(int(p, "margin").map_or(ext.max(16), |m| m.clamp(0, 100_000) as i32)),
        "custom" => {
            let r = match (custom_rect, custom_mask.as_ref()) {
                (Some(r), _) => r,
                (None, Some(m)) => m.content_bounds(),
                (None, None) => return Err(bad(cmd, "custom sampling needs `area` [x,y,w,h] or `channel` (alpha channel index or name)")),
            };
            r.union(&hb).inflate(4)
        }
        other => return Err(bad(cmd, format!("unknown sampling `{other}` (auto|rectangular|custom)"))),
    }
    .intersect(&doc.bounds());
    let opts = FillOptions {
        color_adaptation: color_level(str_or(p, "colorAdaptation", "default")),
        rotations: rotation_level(str_or(p, "rotationAdaptation", "none")),
        scale: bool_or(p, "scale", false),
        mirror: bool_or(p, "mirror", false),
        seed: p.get("seed").and_then(Value::as_u64).unwrap_or(1),
    };
    let output = str_or(p, "output", "current").to_string();
    if !matches!(output.as_str(), "current" | "new" | "duplicate") {
        return Err(bad(cmd, "`output` must be current|new|duplicate"));
    }
    let surf = doc.layer(id).and_then(|l| l.surface()).ok_or(EngineError::NoLayer(id))?;
    let fmt = surf.format();
    let n = fmt.channels();
    // Refuse a window too large to read before allocating for it, whatever the sampling (#963).
    let (w, h) = crate::fill_cmds::window_size(window)?;
    // A background job when started with `Session::start` (#210): reading the window and the
    // PatchMatch fill run on a worker against the document snapshot, cancellable per row band.
    crate::jobs::run(
        s,
        label,
        true,
        move |ctx| {
            ctx.progress(0.0, label);
            let surf = doc.layer(id).and_then(|l| l.surface()).ok_or(EngineError::NoLayer(id))?;
            let img = surf.read_region(window);
            let mut cover = vec![0.0f32; w * h];
            let mut hole = vec![false; w * h];
            let mut source = vec![true; w * h];
            for y in 0..h {
                if y % 64 == 0 {
                    ctx.check()?;
                }
                for x in 0..w {
                    let (dx, dy) = (window.x0 + x as i32, window.y0 + y as i32);
                    let i = y * w + x;
                    cover[i] = sel.sample_channel(dx, dy, 0);
                    hole[i] = cover[i] > 0.0;
                    if sampling == "custom" {
                        source[i] = match (custom_rect, custom_mask.as_ref()) {
                            (Some(r), _) => r.contains(dx, dy),
                            (None, Some(m)) => m.sample_channel(dx, dy, 0) > 0.5,
                            _ => true,
                        };
                    }
                }
            }
            ctx.check()?;
            let filled = ctx.stage(0.05, 1.0, label, |ctl| fill_with(w, h, n, &img, &hole, &source, &opts, ctl)).map_err(|_| EngineError::Cancelled)?;
            Ok((img, cover, hole, filled))
        },
        move |s, (img, cover, hole, filled)| apply_content_aware_fill(s, label, id, &output, fmt, window, &img, &cover, &hole, &filled),
    )
}

/// Write a Content-Aware Fill result (the job's apply step): into the layer, a new layer or a
/// duplicate, as one undo step.
#[allow(clippy::too_many_arguments)]
fn apply_content_aware_fill(
    s: &mut Session,
    label: &str,
    id: LayerId,
    output: &str,
    fmt: PixelFormat,
    window: Rect,
    img: &[f32],
    cover: &[f32],
    hole: &[bool],
    filled: &[f32],
) -> Result<Value> {
    let n = fmt.channels();
    let (w, h) = (window.width() as usize, window.height() as usize);
    let n_hole = hole.iter().filter(|h| **h).count();
    let layer = s.edit(label, |doc, active| {
        let target = match output {
            "new" => {
                let mut l = Layer::raster(doc.next_layer_name("Content-Aware Fill"), PixelFormat { alpha: true, ..fmt });
                let mut px = vec![0.0f32; w * h * n.max(1)];
                let lf = l.surface().map(|s| s.format()).unwrap_or(fmt);
                let ln = lf.channels();
                px.resize(w * h * ln, 0.0);
                for i in 0..w * h {
                    if hole[i] {
                        let rgba = to_rgba(&fmt, &filled[i * n..(i + 1) * n]);
                        let mut enc = [0.0f32; 8];
                        from_rgba_into(&lf, [rgba[0], rgba[1], rgba[2], rgba[3] * cover[i]], &mut enc);
                        px[i * ln..(i + 1) * ln].copy_from_slice(&enc[..ln]);
                    }
                }
                if let Some(s) = l.surface_mut() {
                    s.write_region(window, &px);
                    s.prune();
                }
                let nid = doc.insert_above(Some(id), l);
                *active = Some(nid);
                return Ok(nid);
            }
            "duplicate" => {
                let mut dup = doc.layer(id).ok_or(EngineError::NoLayer(id))?.duplicate();
                dup.name = doc.copy_name(&dup.name);
                let nid = doc.insert_above(Some(id), dup);
                *active = Some(nid);
                nid
            }
            _ => id,
        };
        let surf = writable_surface(doc, target)?;
        let mut out = img.to_vec();
        for (i, &k) in cover.iter().enumerate() {
            if k > 0.0 {
                for c in 0..n {
                    let j = i * n + c;
                    out[j] = img[j] + (filled[j] - img[j]) * k;
                }
            }
        }
        surf.write_region(window, &out);
        Ok(target)
    })?;
    Ok(json!({"layer": layer.0, "filled": n_hole, "window": [window.x0, window.y0, window.width(), window.height()]}))
}

// ------------------------------------------------------------------ Content-Aware Scale

fn content_aware_scale(s: &mut Session, p: &Value) -> Result<Value> {
    use photocraft_algo::seam::{carve_with, resize_bilinear, skin_mask};
    let cmd = "edit.contentAwareScale";
    let id = pixel_layer(s).map_err(EngineError::Other)?;
    let st = s.active().ok_or(EngineError::NoDocument)?;
    let doc = st.doc.clone();
    let surf = doc.layer(id).and_then(|l| l.surface()).ok_or(EngineError::NoLayer(id))?;
    let region = match &doc.selection {
        Some(sel) => sel.content_bounds().intersect(&doc.bounds()),
        None => surf.content_bounds(),
    };
    if region.is_empty() {
        return Err(EngineError::Other("nothing to scale: the layer is empty".into()));
    }
    let (w, h) = (region.width() as usize, region.height() as usize);
    let dim = |k: &str, pct: &str, cur: usize| -> Result<usize> {
        let v = match (p.get(k).and_then(Value::as_f64), p.get(pct).and_then(Value::as_f64)) {
            (Some(px), _) => px,
            (None, Some(pc)) => cur as f64 * pc / 100.0,
            (None, None) => cur as f64,
        };
        if !(1.0..=60_000.0).contains(&v) {
            return Err(bad(cmd, format!("`{k}` must be 1..60000 px")));
        }
        Ok(v.round() as usize)
    };
    let (nw, nh) = (dim("width", "scaleX", w)?, dim("height", "scaleY", h)?);
    let amount = f32_or(p, "amount", 100.0).clamp(0.0, 100.0) / 100.0;
    let fmt = surf.format();
    let n = fmt.channels();
    // Protection: an alpha channel and/or skin tones.
    let protect_mask: Option<Surface> = match p.get("protect").filter(|v| !v.is_null() && v.as_str() != Some("none")) {
        Some(v) => Some(channel_mask(&doc, v).cloned().ok_or_else(|| bad(cmd, "`protect` must name an alpha channel (index or name) or \"none\""))?),
        None => None,
    };
    let skin_tones = bool_or(p, "protectSkinTones", false);
    let dst = Rect::from_xywh(region.x0, region.y0, nw as u32, nh as u32);
    let label = "Content-Aware Scale";
    // A background job when started with `Session::start` (#210): seam carving runs on a worker
    // against the document snapshot and checks for cancellation before every seam.
    crate::jobs::run(
        s,
        label,
        true,
        move |ctx| {
            ctx.progress(0.0, label);
            let surf = doc.layer(id).and_then(|l| l.surface()).ok_or(EngineError::NoLayer(id))?;
            let img = surf.read_region(region);
            let mut protect: Option<Vec<f32>> =
                protect_mask.map(|m| (0..w * h).map(|i| m.sample_channel(region.x0 + (i % w) as i32, region.y0 + (i / w) as i32, 0)).collect());
            if skin_tones {
                let rgba: Vec<f32> = img.chunks_exact(n).flat_map(|px| to_rgba(&fmt, px)).collect();
                let skin = skin_mask(w, h, 4, &rgba);
                protect = Some(match protect {
                    Some(pm) => pm.iter().zip(&skin).map(|(a, b)| a.max(*b)).collect(),
                    None => skin,
                });
            }
            ctx.check()?;
            let carved = if amount > 0.0 {
                ctx.stage(0.05, 0.95, label, |ctl| carve_with(w, h, n, &img, protect.as_deref(), nw, nh, ctl)).map_err(|_| EngineError::Cancelled)?
            } else {
                Vec::new()
            };
            let plain = if amount < 1.0 { resize_bilinear(w, h, n, &img, nw, nh) } else { Vec::new() };
            Ok(match (carved.is_empty(), plain.is_empty()) {
                (false, true) => carved,
                (true, false) => plain,
                _ => carved.iter().zip(&plain).map(|(c, l)| l + (c - l) * amount).collect::<Vec<f32>>(),
            })
        },
        move |s, out| {
            s.edit(label, |doc, _| {
                let surf = writable_surface(doc, id)?;
                crate::pixels::clear_surface(surf, region, None);
                surf.write_region(dst, &out);
                surf.prune();
                Ok(())
            })?;
            Ok(json!({"from": [w, h], "to": [nw, nh], "bounds": [dst.x0, dst.y0, nw, nh]}))
        },
    )
}

// ------------------------------------------------------------------ Define Brush / Custom Shape

fn define_brush_preset(s: &mut Session, p: &Value) -> Result<Value> {
    let name = match p.get("name").and_then(Value::as_str).map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => n.to_string(),
        None => {
            let n = s.tools.presets.iter().filter(|b| b.name.starts_with("Sampled Brush")).count() + 1;
            format!("Sampled Brush {n}")
        }
    };
    s.execute("brush.defineFromSelection", json!({"name": name}))
}

/// The path Define Custom Shape uses: `"work"`, a saved path name, else the work path, the
/// active shape layer, the active layer's vector mask, or the last saved path.
pub(crate) fn current_path(doc: &Document, active: Option<LayerId>, spec: Option<&str>) -> Option<Path> {
    let ok = |p: &Path| !p.subpaths.is_empty();
    match spec {
        Some("work") => return doc.work_path.clone().filter(ok),
        Some(name) => return doc.paths.iter().find(|n| n.name == name).map(|n| n.path.clone()).filter(ok),
        None => {}
    }
    if let Some(p) = doc.work_path.clone().filter(ok) {
        return Some(p);
    }
    if let Some(l) = active.and_then(|id| doc.layer(id)) {
        if let LayerContent::Shape(sh) = &l.content
            && ok(&sh.path)
        {
            return Some(sh.path.clone());
        }
        if let Some(vm) = l.vector_mask.as_ref().filter(|v| ok(&v.path)) {
            return Some(vm.path.clone());
        }
    }
    doc.paths.last().map(|n| n.path.clone()).filter(ok)
}

fn can_define_shape(s: &Session) -> std::result::Result<(), String> {
    let d = s.active().ok_or("no document open")?;
    current_path(&d.doc, d.active_layer, None).map(|_| ()).ok_or_else(|| "select a path or shape layer first".into())
}

fn define_custom_shape(s: &mut Session, p: &Value) -> Result<Value> {
    let st = s.active().ok_or(EngineError::NoDocument)?;
    let path = current_path(&st.doc, st.active_layer, p.get("path").and_then(Value::as_str)).ok_or_else(|| bad("edit.defineCustomShape", "no such path"))?;
    let name = p
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("Shape {}", s.edit_state.custom_shapes.len() + 1));
    let shapes = &mut s.edit_state.custom_shapes;
    match shapes.iter_mut().find(|c| c.name == name) {
        Some(c) => c.path = path,
        None => shapes.push(CustomShape { name: name.clone(), path }),
    }
    Ok(json!({"name": name, "count": shapes.len()}))
}

// ------------------------------------------------------------------ Find and Replace Text

/// Byte ranges of `find` in `text` (case-insensitive unless `case`; whole words only with `whole`).
pub fn find_matches(text: &str, find: &str, case: bool, whole: bool) -> Vec<(usize, usize)> {
    if find.is_empty() {
        return Vec::new();
    }
    let norm = |c: char| -> String { if case { c.to_string() } else { c.to_lowercase().collect() } };
    let needle: String = find.chars().map(norm).collect();
    let mut out = Vec::new();
    let mut at = 0;
    while at < text.len() {
        // Compare char by char so case folding that changes byte lengths stays aligned.
        let mut hay = String::new();
        let mut end = at;
        for (i, c) in text[at..].char_indices() {
            if hay.len() >= needle.len() {
                break;
            }
            hay.push_str(&norm(c));
            end = at + i + c.len_utf8();
        }
        if hay == needle {
            let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
            let before = text[..at].chars().next_back();
            let after = text[end..].chars().next();
            if !whole || (!word(before) && !word(after)) {
                out.push((at, end));
                at = end;
                continue;
            }
        }
        at += text[at..].chars().next().map_or(1, char::len_utf8);
    }
    out
}

fn type_layers(doc: &Document, forward: bool) -> Vec<LayerId> {
    // Layers panel order: top first.
    let mut ids: Vec<LayerId> =
        doc.walk().into_iter().rev().filter(|(p, _, l)| matches!(l.content, LayerContent::Text(_)) && !doc.locks_at(p).all).map(|(_, _, l)| l.id).collect();
    if !forward {
        ids.reverse();
    }
    ids
}

fn has_type_layer(s: &Session) -> std::result::Result<(), String> {
    let d = s.active().ok_or("no document open")?;
    if type_layers(&d.doc, true).is_empty() { Err("the document has no type layers".into()) } else { Ok(()) }
}

fn find_replace(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "edit.findAndReplaceText";
    let find = p.get("find").and_then(Value::as_str).filter(|f| !f.is_empty()).ok_or_else(|| bad(cmd, "missing `find`"))?.to_string();
    let replace = str_or(p, "replace", "").to_string();
    let (case, whole, forward, all_layers) =
        (bool_or(p, "caseSensitive", false), bool_or(p, "wholeWord", false), bool_or(p, "forward", true), bool_or(p, "allLayers", true));
    let action = str_or(p, "action", "changeAll").to_string();
    let st = s.active().ok_or(EngineError::NoDocument)?;
    let mut layers = type_layers(&st.doc, forward);
    if !all_layers {
        let active = layer_param(s, p).ok();
        layers.retain(|id| Some(*id) == active);
    }
    let text_of = |doc: &Document, id: LayerId| -> Option<String> {
        match &doc.layer(id)?.content {
            LayerContent::Text(t) => Some(t.text.clone()),
            _ => None,
        }
    };
    match action.as_str() {
        "changeAll" => {
            let doc = st.doc.clone();
            let plan: Vec<(LayerId, Vec<(usize, usize)>)> =
                layers.iter().filter_map(|id| Some((*id, find_matches(&text_of(&doc, *id)?, &find, case, whole)))).filter(|(_, m)| !m.is_empty()).collect();
            let count: usize = plan.iter().map(|(_, m)| m.len()).sum();
            if count == 0 {
                return Ok(json!({"count": 0, "layers": 0}));
            }
            s.edit("Replace All Text", |doc, _| {
                let snapshot = doc.clone();
                for (id, matches) in &plan {
                    if let Some(LayerContent::Text(t)) = doc.layer_mut(*id).map(|l| &mut l.content) {
                        for (a, b) in matches.iter().rev() {
                            crate::type_cmds::replace_text(t, *a, *b, &replace);
                        }
                        crate::type_cmds::refresh(&snapshot, t);
                    }
                }
                Ok(())
            })?;
            s.edit_state.find_cursor = None;
            Ok(json!({"count": count, "layers": plan.len()}))
        }
        "find" | "change" | "changeFind" => {
            let doc = st.doc.clone();
            let cursor = s.edit_state.find_cursor;
            let mut changed = None;
            if action != "find"
                && let Some((id, a, b)) = cursor
                && text_of(&doc, id).is_some_and(|t| find_matches(&t, &find, case, whole).contains(&(a, b)))
            {
                s.edit("Replace Text", |doc, _| {
                    let snapshot = doc.clone();
                    if let Some(LayerContent::Text(t)) = doc.layer_mut(id).map(|l| &mut l.content) {
                        crate::type_cmds::replace_text(t, a, b, &replace);
                        crate::type_cmds::refresh(&snapshot, t);
                    }
                    Ok(())
                })?;
                let end = a + replace.len();
                s.edit_state.find_cursor = Some((id, end, end));
                changed = Some((id, a, end));
                if action == "change" {
                    return Ok(json!({"changed": {"layer": id.0, "start": a, "end": end}}));
                }
            }
            // Find the next match after the cursor (wrapping through the layers).
            let doc = s.active().ok_or(EngineError::NoDocument)?.doc.clone();
            let cursor = s.edit_state.find_cursor;
            let start = cursor.and_then(|(id, _, _)| layers.iter().position(|l| *l == id)).unwrap_or(0);
            for k in 0..=layers.len() {
                // `allLayers: false` with a non-type layer active leaves no layers to search (#703).
                let Some(&id) = layers.get((start + k) % layers.len().max(1)) else { break };
                let Some(text) = text_of(&doc, id) else { continue };
                let ms = find_matches(&text, &find, case, whole);
                let hit = match (k, cursor) {
                    (0, Some((cid, a, b))) if cid == id => {
                        if forward {
                            ms.into_iter().find(|m| m.0 >= b)
                        } else {
                            ms.into_iter().rev().find(|m| m.1 <= a)
                        }
                    }
                    _ => {
                        if forward {
                            ms.into_iter().next()
                        } else {
                            ms.into_iter().next_back()
                        }
                    }
                };
                if let Some((a, b)) = hit {
                    s.edit_state.find_cursor = Some((id, a, b));
                    let _ = s.select_layer(id);
                    return Ok(
                        json!({"found": {"layer": id.0, "start": a, "end": b, "text": &text[a..b]}, "changed": changed.map(|(l, a, b)| json!({"layer": l.0, "start": a, "end": b}))}),
                    );
                }
            }
            s.edit_state.find_cursor = None;
            Ok(json!({"found": null, "changed": changed.map(|(l, a, b)| json!({"layer": l.0, "start": a, "end": b}))}))
        }
        other => Err(bad(cmd, format!("unknown action `{other}` (find|change|changeFind|changeAll)"))),
    }
}

// ------------------------------------------------------------------ presets

/// Libraries the Preset Manager edits (patterns export as `.pat` with `pattern.export`).
const PRESET_KINDS: [&str; 6] = ["brushes", "customShapes", "patterns", "gradients", "swatches", "styles"];

fn canonical_kind(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "brushes" => "brushes",
        "customShapes" | "shapes" => "customShapes",
        "patterns" => "patterns",
        "gradients" => "gradients",
        "swatches" | "colors" => "swatches",
        "styles" => "styles",
        _ => return None,
    })
}

#[derive(Clone, Debug)]
struct PresetEntry {
    name: String,
    group: String,
}

impl PresetEntry {
    fn json(&self) -> Value {
        if self.group.is_empty() {
            json!({"name": self.name})
        } else {
            json!({"name": self.name, "group": self.group})
        }
    }
}

fn grouped_entries<T: Named>(groups: &[Group<T>]) -> Vec<PresetEntry> {
    groups
        .iter()
        .flat_map(|g| g.items.iter().map(|i| PresetEntry { name: i.name().to_string(), group: g.name.clone() }))
        .collect()
}

fn preset_entries(s: &Session, kind: &str) -> Option<Vec<PresetEntry>> {
    Some(match canonical_kind(kind)? {
        "brushes" => s
            .tools
            .presets
            .iter()
            .map(|b| PresetEntry { name: b.name.clone(), group: b.group.clone() })
            .collect(),
        "customShapes" => s.edit_state.custom_shapes.iter().map(|c| PresetEntry { name: c.name.clone(), group: String::new() }).collect(),
        "patterns" => s.patterns.items.iter().map(|p| PresetEntry { name: p.name.clone(), group: String::new() }).collect(),
        "gradients" => grouped_entries(&s.presets.gradients),
        "swatches" => grouped_entries(&s.presets.swatches),
        "styles" => grouped_entries(&s.presets.styles),
        _ => return None,
    })
}

fn preset_names(s: &Session, kind: &str) -> Option<Vec<String>> {
    Some(preset_entries(s, kind)?.into_iter().map(|e| e.name).collect())
}

fn filter_query(entries: Vec<PresetEntry>, query: &str) -> Vec<PresetEntry> {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return entries;
    }
    entries.into_iter().filter(|e| e.name.to_ascii_lowercase().contains(&q) || e.group.to_ascii_lowercase().contains(&q)).collect()
}

fn preset_index(s: &Session, kind: &str, p: &Value) -> Result<usize> {
    let names = preset_names(s, kind).ok_or_else(|| bad("edit.presets.presetManager", format!("unknown preset kind `{kind}` ({})", PRESET_KINDS.join("|"))))?;
    if let Some(i) = p.get("index").and_then(Value::as_u64) {
        return if (i as usize) < names.len() { Ok(i as usize) } else { Err(bad("edit.presets.presetManager", format!("no preset {i}"))) };
    }
    let name = p.get("name").and_then(Value::as_str).ok_or_else(|| bad("edit.presets.presetManager", "give `index` or `name`"))?;
    names.iter().position(|n| n == name).ok_or_else(|| bad("edit.presets.presetManager", format!("no preset named `{name}`")))
}

fn rename_grouped<T: Named>(groups: &mut [Group<T>], i: usize, new: String) -> bool {
    let mut n = 0;
    for g in groups {
        for item in &mut g.items {
            if n == i {
                item.set_name(new);
                return true;
            }
            n += 1;
        }
    }
    false
}

fn delete_grouped<T>(groups: &mut [Group<T>], i: usize) -> bool {
    let mut n = 0;
    for g in groups.iter_mut() {
        for j in 0..g.items.len() {
            if n == i {
                g.items.remove(j);
                return true;
            }
            n += 1;
        }
    }
    false
}

fn move_grouped<T: Clone>(groups: &mut Vec<Group<T>>, i: usize, to: usize) -> bool {
    let mut items: Vec<(String, T)> = groups.iter().flat_map(|g| g.items.iter().cloned().map(|item| (g.name.clone(), item))).collect();
    if crate::move_item(&mut items, i, to).is_none() {
        return false;
    }
    let mut rebuilt: Vec<Group<T>> = Vec::new();
    for (name, item) in items {
        match rebuilt.iter_mut().find(|g| g.name == name) {
            Some(g) => g.items.push(item),
            None => rebuilt.push(Group::new(&name, vec![item])),
        }
    }
    *groups = rebuilt;
    true
}

fn list_json(s: &Session, kind: &str, query: &str) -> Result<Value> {
    let entries = preset_entries(s, kind).ok_or_else(|| bad("edit.presets.presetManager", format!("unknown preset kind `{kind}`")))?;
    Ok(Value::Array(filter_query(entries, query).into_iter().map(|e| e.json()).collect()))
}

const PRESET_SET_FORMAT: &str = "photocraft-preset-set";

fn dump_set(s: &Session, kind: &str) -> Result<Value> {
    let cmd = "edit.presets.presetManager";
    let items = match kind {
        "brushes" => serde_json::to_value(&s.tools.presets).map_err(|e| bad(cmd, e.to_string()))?,
        "customShapes" => serde_json::to_value(&s.edit_state.custom_shapes).map_err(|e| bad(cmd, e.to_string()))?,
        "gradients" => dump_grouped(&s.presets.gradients),
        "swatches" => dump_grouped(&s.presets.swatches),
        "styles" => dump_grouped(&s.presets.styles),
        "patterns" => {
            let bytes = photocraft_io::pattern_map::write_pat(&s.patterns.items).map_err(EngineError::Other)?;
            return Ok(json!({
                "format": PRESET_SET_FORMAT,
                "version": 1,
                "kind": kind,
                "pat": photocraft_paint::tile::b64_encode(&bytes),
                "items": list_json(s, kind, "")?,
            }));
        }
        _ => Value::Null,
    };
    Ok(json!({"format": PRESET_SET_FORMAT, "version": 1, "kind": kind, "items": items}))
}

fn dump_grouped<T: Serialize>(groups: &[Group<T>]) -> Value {
    let mut items = Vec::new();
    for g in groups {
        for item in &g.items {
            items.push(json!({"group": g.name, "preset": item}));
        }
    }
    Value::Array(items)
}

fn load_set(s: &mut Session, kind: &str, p: &Value) -> Result<Value> {
    let cmd = "edit.presets.presetManager";
    if let Some(b64) = p.get("bytes").and_then(Value::as_str) {
        return load_native_bytes(s, kind, b64, p.get("fileName").and_then(Value::as_str).unwrap_or(""));
    }
    let data = p.get("data").cloned().ok_or_else(|| bad(cmd, "missing `data` (a dumped preset set) or `bytes`"))?;
    let data = match data {
        Value::String(text) => serde_json::from_str(&text).map_err(|e| bad(cmd, format!("not a preset set: {e}")))?,
        v => v,
    };
    if data.get("format").and_then(Value::as_str) != Some(PRESET_SET_FORMAT) {
        return Err(bad(cmd, "not a PhotoCraft preset set (Save Set… from the Preset Manager)"));
    }
    let file_kind = data.get("kind").and_then(Value::as_str).unwrap_or("");
    if canonical_kind(file_kind) != Some(kind) {
        return Err(bad(cmd, format!("set is `{file_kind}`, expected `{kind}`")));
    }
    let replace = p.get("replace").and_then(Value::as_bool).unwrap_or(false);
    let n = match kind {
        "brushes" => {
            let items: Vec<photocraft_paint::BrushPreset> =
                serde_json::from_value(data.get("items").cloned().unwrap_or(Value::Null)).map_err(|e| bad(cmd, format!("brushes: {e}")))?;
            if replace {
                s.tools.presets.clear();
            }
            merge_named(&mut s.tools.presets, items, |b| b.name.clone())
        }
        "customShapes" => {
            let items: Vec<CustomShape> =
                serde_json::from_value(data.get("items").cloned().unwrap_or(Value::Null)).map_err(|e| bad(cmd, format!("customShapes: {e}")))?;
            if replace {
                s.edit_state.custom_shapes.clear();
            }
            merge_named(&mut s.edit_state.custom_shapes, items, |c| c.name.clone())
        }
        "gradients" => merge_grouped_presets(&mut s.presets.gradients, &data, replace, cmd)?,
        "swatches" => merge_grouped_presets(&mut s.presets.swatches, &data, replace, cmd)?,
        "styles" => merge_grouped_presets(&mut s.presets.styles, &data, replace, cmd)?,
        "patterns" => load_patterns(s, &data, replace, cmd)?,
        _ => 0,
    };
    match kind {
        "brushes" => s.brush_presets_changed(),
        "gradients" | "swatches" | "styles" => s.presets_changed(),
        _ => {}
    }
    Ok(json!({"kind": kind, "loaded": n, kind: list_json(s, kind, "")?}))
}

fn merge_named<T>(dst: &mut Vec<T>, incoming: Vec<T>, name: impl Fn(&T) -> String) -> usize {
    let n = incoming.len();
    for item in incoming {
        let nme = name(&item);
        match dst.iter_mut().position(|x| name(x) == nme) {
            Some(i) => dst[i] = item,
            None => dst.push(item),
        }
    }
    n
}

fn grouped_from_items<T: serde::de::DeserializeOwned>(data: &Value, cmd: &str) -> Result<Vec<(String, T)>> {
    let arr = data.get("items").and_then(Value::as_array).ok_or_else(|| bad(cmd, "set has no items"))?;
    let mut out = Vec::new();
    for it in arr {
        let group = it.get("group").and_then(Value::as_str).unwrap_or("").to_string();
        let preset = it.get("preset").cloned().ok_or_else(|| bad(cmd, "each item needs `preset` (re-save the set from the Preset Manager)"))?;
        let item: T = serde_json::from_value(preset).map_err(|e| bad(cmd, e.to_string()))?;
        out.push((group, item));
    }
    Ok(out)
}

fn merge_grouped_presets<T: Named + serde::de::DeserializeOwned>(groups: &mut Vec<Group<T>>, data: &Value, replace: bool, cmd: &str) -> Result<usize> {
    let incoming = grouped_from_items::<T>(data, cmd)?;
    if replace {
        groups.clear();
    }
    let n = incoming.len();
    for (group, item) in incoming {
        let name = item.name().to_string();
        if let Some(x) = groups.iter_mut().flat_map(|g| g.items.iter_mut()).find(|i| i.name() == name) {
            *x = item;
            continue;
        }
        match groups.iter_mut().find(|g| g.name == group) {
            Some(g) => g.items.push(item),
            None => groups.push(Group::new(&group, vec![item])),
        }
    }
    Ok(n)
}

fn load_patterns(s: &mut Session, data: &Value, replace: bool, cmd: &str) -> Result<usize> {
    let b64 = data.get("pat").and_then(Value::as_str).ok_or_else(|| bad(cmd, "pattern set is missing `pat` (re-save from the Preset Manager)"))?;
    let bytes = photocraft_paint::tile::b64_decode(b64).ok_or_else(|| bad(cmd, "`pat` is not valid base64"))?;
    let pats = photocraft_io::pattern_map::read_pat(&bytes).map_err(|e| bad(cmd, e))?;
    if replace {
        s.patterns.items.clear();
    }
    let n = pats.len();
    for pat in pats {
        match s.patterns.items.iter_mut().find(|q| q.id == pat.id || q.name == pat.name) {
            Some(q) => *q = pat,
            None => s.patterns.items.push(pat),
        }
    }
    Ok(n)
}

fn load_native_bytes(s: &mut Session, kind: &str, b64: &str, file_name: &str) -> Result<Value> {
    let cmd = "edit.presets.presetManager";
    let bytes = photocraft_paint::tile::b64_decode(b64).ok_or_else(|| bad(cmd, "`bytes` is not valid base64"))?;
    let data = photocraft_paint::tile::b64_encode(&bytes);
    match kind {
        "brushes" => s.execute("brush.presets.importAbr", json!({"data": data, "group": stem(file_name)})),
        "gradients" => s.execute("gradient.presets.importGrd", json!({"data": data, "group": stem(file_name)})),
        "swatches" => s.execute("swatches.import", json!({"data": data, "group": stem(file_name)})),
        "patterns" => {
            let pats = photocraft_io::pattern_map::read_pat(&bytes).map_err(|e| bad(cmd, e))?;
            let n = pats.len();
            for pat in pats {
                match s.patterns.items.iter_mut().find(|q| q.id == pat.id || q.name == pat.name) {
                    Some(q) => *q = pat,
                    None => s.patterns.items.push(pat),
                }
            }
            Ok(json!({"kind": kind, "loaded": n, kind: list_json(s, kind, "")?}))
        }
        _ => Err(bad(cmd, format!("`{kind}` loads a PhotoCraft preset set (Save Set…), not this file"))),
    }
}

fn stem(name: &str) -> String {
    std::path::Path::new(name).file_stem().map(|s| s.to_string_lossy().into_owned()).filter(|s| !s.is_empty()).unwrap_or_else(|| "Imported".into())
}

fn preset_manager(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "edit.presets.presetManager";
    let action = str_or(p, "action", "list");
    let query = p.get("query").and_then(Value::as_str).unwrap_or("");
    if action == "list" {
        let kinds: Vec<String> = match p.get("kind").and_then(Value::as_str) {
            Some(k) => vec![canonical_kind(k).unwrap_or(k).to_string()],
            None => PRESET_KINDS.iter().map(|k| (*k).to_string()).collect(),
        };
        let mut out = serde_json::Map::new();
        for k in kinds {
            out.insert(k.clone(), list_json(s, &k, query)?);
        }
        return Ok(Value::Object(out));
    }
    let raw_kind = p.get("kind").and_then(Value::as_str).ok_or_else(|| bad(cmd, "missing `kind`"))?;
    let kind = canonical_kind(raw_kind).ok_or_else(|| bad(cmd, format!("unknown preset kind `{raw_kind}` ({})", PRESET_KINDS.join("|"))))?;
    if action == "save" {
        let set = dump_set(s, kind)?;
        if let Some(path) = p.get("path").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            let bytes = serde_json::to_vec_pretty(&set).map_err(|e| bad(cmd, e.to_string()))?;
            crate::file_cmds::write_file(path, &bytes)?;
            return Ok(json!({"kind": kind, "path": path, "items": set.get("items").cloned().unwrap_or(Value::Null)}));
        }
        return Ok(set);
    }
    if action == "load" {
        return load_set(s, kind, p);
    }
    let i = preset_index(s, kind, p)?;
    match action {
        "rename" => {
            let new =
                p.get("newName").and_then(Value::as_str).map(str::trim).filter(|n| !n.is_empty()).ok_or_else(|| bad(cmd, "missing `newName`"))?.to_string();
            match kind {
                "brushes" => {
                    let b = &mut s.tools.presets[i];
                    b.name = new;
                    b.builtin = false;
                }
                "patterns" => s.patterns.items[i].name = new,
                "customShapes" => s.edit_state.custom_shapes[i].name = new,
                "gradients" => drop(rename_grouped(&mut s.presets.gradients, i, new)),
                "swatches" => drop(rename_grouped(&mut s.presets.swatches, i, new)),
                "styles" => drop(rename_grouped(&mut s.presets.styles, i, new)),
                _ => {}
            }
        }
        "delete" => match kind {
            "brushes" => drop(s.tools.presets.remove(i)),
            "patterns" => drop(s.patterns.items.remove(i)),
            "customShapes" => drop(s.edit_state.custom_shapes.remove(i)),
            "gradients" => drop(delete_grouped(&mut s.presets.gradients, i)),
            "swatches" => drop(delete_grouped(&mut s.presets.swatches, i)),
            "styles" => drop(delete_grouped(&mut s.presets.styles, i)),
            _ => {}
        },
        "move" => {
            let to = p.get("to").and_then(Value::as_u64).ok_or_else(|| bad(cmd, "missing `to`"))? as usize;
            match kind {
                "brushes" => drop(crate::move_item(&mut s.tools.presets, i, to)),
                "patterns" => drop(crate::move_item(&mut s.patterns.items, i, to)),
                "customShapes" => drop(crate::move_item(&mut s.edit_state.custom_shapes, i, to)),
                "gradients" => drop(move_grouped(&mut s.presets.gradients, i, to)),
                "swatches" => drop(move_grouped(&mut s.presets.swatches, i, to)),
                "styles" => drop(move_grouped(&mut s.presets.styles, i, to)),
                _ => {}
            };
        }
        other => return Err(bad(cmd, format!("unknown action `{other}` (list|rename|delete|move|save|load)"))),
    }
    match kind {
        "brushes" => s.brush_presets_changed(),
        "gradients" | "swatches" | "styles" => s.presets_changed(),
        _ => {}
    }
    Ok(json!({kind: list_json(s, kind, "")?}))
}

/// Exported preset file (`.pcpresets`, JSON).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PresetFile {
    pub format: String,
    pub version: u32,
    pub brushes: Vec<photocraft_paint::BrushPreset>,
    pub custom_shapes: Vec<CustomShape>,
}

pub const PRESET_FORMAT: &str = "photocraft-presets";

fn export_import(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "edit.presets.exportImportPresets";
    let kinds: Vec<String> = p
        .get("kinds")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_else(|| ["brushes", "customShapes"].iter().map(|k| k.to_string()).collect());
    let want = |k: &str| kinds.iter().any(|x| x == k);
    match str_or(p, "action", "export") {
        "export" => {
            let file = PresetFile {
                format: PRESET_FORMAT.into(),
                version: 1,
                brushes: if want("brushes") {
                    let mut out: Vec<photocraft_paint::BrushPreset> =
                        s.tools.presets.iter().filter(|b| !b.builtin || bool_or(p, "includeBuiltins", false)).cloned().collect();
                    // The file embeds every bitmap: load the tips the preset store keeps (#1843).
                    for b in &mut out {
                        s.load_brush_tips(&mut b.brush).map_err(|e| bad(cmd, format!("brush preset `{}`: {e}", b.name)))?;
                    }
                    out
                } else {
                    Vec::new()
                },
                custom_shapes: if want("customShapes") { s.edit_state.custom_shapes.clone() } else { Vec::new() },
            };
            Ok(json!({"data": file, "brushes": file.brushes.len(), "customShapes": file.custom_shapes.len()}))
        }
        "import" => {
            let data = p.get("data").cloned().ok_or_else(|| bad(cmd, "missing `data` (an exported preset file)"))?;
            let data = match data {
                Value::String(text) => serde_json::from_str(&text).map_err(|e| bad(cmd, format!("not a preset file: {e}")))?,
                v => v,
            };
            let file: PresetFile = serde_json::from_value(data).map_err(|e| bad(cmd, format!("not a preset file: {e}")))?;
            if file.format != PRESET_FORMAT {
                return Err(bad(cmd, format!("not a preset file (format `{}`)", file.format)));
            }
            let (mut nb, mut ns) = (0, 0);
            if want("brushes") {
                for mut b in file.brushes {
                    // Nested folders as the commands bound them (a hand-edited file can't build an
                    // absurd tree).
                    b.folder.retain(|f| !f.trim().is_empty());
                    b.folder.truncate(crate::brush_preset_cmds::MAX_FOLDER_DEPTH);
                    match s.tools.presets.iter_mut().find(|x| x.name == b.name) {
                        Some(x) => *x = b,
                        None => s.tools.presets.push(b),
                    }
                    nb += 1;
                }
                s.brush_presets_changed();
            }
            if want("customShapes") {
                for c in file.custom_shapes {
                    match s.edit_state.custom_shapes.iter_mut().find(|x| x.name == c.name) {
                        Some(x) => *x = c,
                        None => s.edit_state.custom_shapes.push(c),
                    }
                    ns += 1;
                }
            }
            Ok(json!({"brushes": nb, "customShapes": ns}))
        }
        other => Err(bad(cmd, format!("unknown action `{other}` (export|import)"))),
    }
}

// ------------------------------------------------------------------ specs

macro_rules! spec {
    ($id:literal, $label:literal, [$($m:literal),*], $sc:expr, $params:literal, $en:expr, $run:expr) => {
        CommandSpec { id: $id, label: $label, menu: &[$($m),*], shortcut: $sc, params: $params, enabled: $en, run: $run, journal: true }
    };
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        spec!(
            "edit.fade",
            "Fade…",
            ["Edit"],
            Some("Cmd+Shift+F"),
            r##"{"opacity":0..100=100,"mode":"normal|multiply|screen|overlay|softLight|hardLight|darken|lighten|difference|color|luminosity"}"##,
            can_fade,
            fade
        ),
        spec!("edit.purge.undo", "Undo", ["Edit", "Purge"], None, "{}", can_purge_undo, |s, _| purge(s, "undo")),
        spec!("edit.purge.clipboard", "Clipboard", ["Edit", "Purge"], None, "{}", can_purge_clipboard, |s, _| purge(s, "clipboard")),
        spec!("edit.purge.histories", "Histories", ["Edit", "Purge"], None, "{}", can_purge_histories, |s, _| purge(s, "histories")),
        spec!("edit.purge.videoCache", "Video Cache", ["Edit", "Purge"], None, "{}", always, |_, _| Ok(
            json!({"purged": [], "bytes": 0, "message": "nothing to purge (no video layers)"})
        )),
        spec!("edit.purge.all", "All", ["Edit", "Purge"], None, "{}", can_purge_all, |s, _| purge(s, "all")),
        spec!(
            "edit.contentAwareFill",
            "Content-Aware Fill…",
            ["Edit"],
            None,
            r##"{"sampling":"auto|rectangular|custom","margin":px?,"area":[x,y,w,h]?,"channel":index|name?,"colorAdaptation":"default|none|high|veryHigh","rotationAdaptation":"none|low|medium|high|full","scale":bool=false,"mirror":bool=false,"output":"current|new|duplicate","seed":u64=1}"##,
            can_caf,
            content_aware_fill
        ),
        spec!("edit.deleteAndFillSelection", "Delete and Fill Selection", [], None, "{}", can_caf, delete_and_fill),
        spec!(
            "edit.contentAwareScale",
            "Content-Aware Scale",
            ["Edit"],
            Some("Cmd+Alt+Shift+C"),
            r##"{"width":px?,"height":px?,"scaleX":1..400=100,"scaleY":1..400=100,"amount":0..100=100,"protect":"none"|channel index|name,"protectSkinTones":bool=false}"##,
            |s| pixel_layer(s).map(|_| ()),
            content_aware_scale
        ),
        spec!(
            "edit.defineBrushPreset",
            "Define Brush Preset…",
            ["Edit"],
            None,
            r##"{"name":text}"##,
            |s| if s.is_enabled("brush.defineFromSelection") { Ok(()) } else { Err("select pixels to define a brush from".into()) },
            define_brush_preset
        ),
        spec!(
            "edit.defineCustomShape",
            "Define Custom Shape…",
            ["Edit"],
            None,
            r##"{"name":text,"path":"work|<saved path name>"?}"##,
            can_define_shape,
            define_custom_shape
        ),
        spec!(
            "edit.findAndReplaceText",
            "Find and Replace Text…",
            ["Edit"],
            None,
            r##"{"find":text,"replace":text,"action":"changeAll|find|change|changeFind","caseSensitive":bool=false,"wholeWord":bool=false,"forward":bool=true,"allLayers":bool=true}"##,
            has_type_layer,
            find_replace
        ),
        spec!(
            "edit.presets.presetManager",
            "Preset Manager…",
            ["Edit", "Presets"],
            None,
            r##"{"action":"list|rename|delete|move|save|load","kind":"brushes|customShapes|patterns|gradients|swatches|styles","index":n?,"name":str?,"newName":str?,"to":n?,"query":str?,"data":json (load),"bytes":base64 (load native file),"fileName":str?,"path":file (save),"replace":bool=false}"##,
            always,
            preset_manager
        ),
        spec!(
            "edit.presets.exportImportPresets",
            "Export/Import Presets…",
            ["Edit", "Presets"],
            None,
            r##"{"action":"export|import","kinds":["brushes","customShapes"]?,"data":json (import),"includeBuiltins":bool=false}"##,
            always,
            export_import
        ),
    ]
}

/// The custom shape library (for the Custom Shape tool).
pub fn custom_shapes(s: &Session) -> &[CustomShape] {
    &s.edit_state.custom_shapes
}

#[cfg(test)]
#[path = "edit_menu_cmds/tests.rs"]
mod tests;
