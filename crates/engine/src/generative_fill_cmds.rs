//! Edit › Generative Fill: place generated pixels on a new layer, clipped to the selection.
//!
//! Network I/O lives in the UI (AI Horde). This command only composites pixels the caller
//! already has, so tests and the CLI never need an HTTP client.

use photocraft_color::PixelFormat;
use photocraft_doc::{Layer, LayerContent, LayerId};
use photocraft_geom::Rect;
use photocraft_raster::{Surface, from_rgba_into};
use serde_json::{Value, json};

use crate::commands::{CommandSpec, int, int_i32};
use crate::{EngineError, Result, Session};

const CMD: &str = "edit.generativeFill";
const MAX_SIDE: i64 = 8192;
const MAX_PIXELS: u64 = 16_000_000;

fn bad(msg: impl Into<String>) -> EngineError {
    EngineError::BadParams { cmd: CMD.into(), msg: msg.into() }
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

fn can_generative_fill(s: &Session) -> std::result::Result<(), String> {
    pixel_layer(s)?;
    let d = s.active().ok_or("no document open")?;
    if d.doc.selection.as_ref().is_none_or(|m| m.content_bounds().is_empty()) {
        return Err("make a selection around what to generate".into());
    }
    Ok(())
}

fn u32_dim(p: &Value, key: &str) -> Result<u32> {
    let v = int(p, key).ok_or_else(|| bad(format!("`{key}` must be a positive integer")))?;
    if !(1..=MAX_SIDE).contains(&v) {
        return Err(bad(format!("`{key}` must be 1..={MAX_SIDE}")));
    }
    u32::try_from(v).map_err(|_| bad(format!("`{key}` is too large")))
}

pub(crate) fn decode_pixels(p: &Value) -> Result<(u32, u32, Vec<[f32; 4]>)> {
    if p.get("rgba").is_some() {
        let w = u32_dim(p, "width")?;
        let h = u32_dim(p, "height")?;
        let n = u64::from(w).saturating_mul(u64::from(h));
        if n > MAX_PIXELS {
            return Err(bad("the generated image is too large"));
        }
        let arr = p.get("rgba").and_then(Value::as_array).ok_or_else(|| bad("`rgba` must be an array of 0..255 bytes"))?;
        let expect = n.saturating_mul(4);
        if arr.len() as u64 != expect {
            return Err(bad(format!("`rgba` must have {expect} values (width×height×4)")));
        }
        let mut px = Vec::new();
        if px.try_reserve(n as usize).is_err() {
            return Err(bad("the generated image is too large"));
        }
        for chunk in arr.chunks(4) {
            let b = |i: usize| {
                chunk
                    .get(i)
                    .and_then(Value::as_f64)
                    .filter(|f| f.is_finite())
                    .map(|v| (v.round() as i64).clamp(0, 255) as u8)
                    .ok_or_else(|| bad("`rgba` values must be finite numbers"))
            };
            px.push([b(0)? as f32 / 255.0, b(1)? as f32 / 255.0, b(2)? as f32 / 255.0, b(3)? as f32 / 255.0]);
        }
        return Ok((w, h, px));
    }
    let encoded = p
        .get("png")
        .and_then(Value::as_str)
        .or_else(|| p.get("webp").and_then(Value::as_str))
        .ok_or_else(|| bad("needs generated pixels: `rgba` with `width`/`height`, or `png`/`webp` as standard base64"))?;
    let bytes = decode_b64(encoded)?;
    if bytes.len() as u64 > 32 * 1024 * 1024 {
        return Err(bad("the generated image is too large"));
    }
    let img = photocraft_codecs::decode(&bytes).map_err(|e| bad(format!("could not decode generated image: {e}")))?;
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 || u64::from(w).saturating_mul(u64::from(h)) > MAX_PIXELS {
        return Err(bad("the generated image is too large"));
    }
    let rgba8 = img.to_rgba8();
    let mut px = Vec::new();
    if px.try_reserve(rgba8.len() / 4).is_err() {
        return Err(bad("the generated image is too large"));
    }
    for c in rgba8.as_chunks::<4>().0 {
        px.push([c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0, c[3] as f32 / 255.0]);
    }
    Ok((w, h, px))
}

fn decode_b64(s: &str) -> Result<Vec<u8>> {
    use base64::Engine as _;
    let trimmed = s.trim();
    if trimmed.is_empty() || trimmed.len() > 64 * 1024 * 1024 {
        return Err(bad("generated image base64 is empty or too large"));
    }
    base64::engine::general_purpose::STANDARD.decode(trimmed).map_err(|_| bad("generated image is not valid standard base64"))
}

fn layer_name(prompt: &str) -> String {
    let t = prompt.trim();
    if t.is_empty() {
        return "Generative Fill".into();
    }
    let mut name = String::from("Generative Fill: ");
    for ch in t.chars() {
        if name.len() >= 80 {
            name.push('…');
            break;
        }
        if ch.is_control() {
            name.push(' ');
        } else {
            name.push(ch);
        }
    }
    name
}

fn generative_fill(s: &mut Session, p: &Value) -> Result<Value> {
    let id = pixel_layer(s).map_err(EngineError::Other)?;
    let st = s.active().ok_or(EngineError::NoDocument)?;
    let sel = st.doc.selection.clone().ok_or_else(|| bad("no selection"))?;
    let hb = sel.content_bounds().intersect(&st.doc.bounds());
    if hb.is_empty() {
        return Err(bad("the selection is outside the canvas"));
    }
    let (w, h, mut px) = decode_pixels(p)?;
    let (ox, oy) = origin_xy(p, hb)?;
    let output = p.get("output").and_then(Value::as_str).unwrap_or("new");
    if !matches!(output, "new" | "current") {
        return Err(bad("`output` must be new|current"));
    }
    let prompt = p.get("prompt").and_then(Value::as_str).unwrap_or("").to_string();
    let iw = w as i32;
    let ih = h as i32;
    let window = Rect::new(ox, oy, ox.saturating_add(iw.max(0)), oy.saturating_add(ih.max(0)));
    if window.width() != w || window.height() != h {
        return Err(bad("the generated image origin overflows the canvas"));
    }
    let region = window.intersect(&st.doc.bounds());
    if region.is_empty() {
        return Err(bad("the generated image does not overlap the canvas"));
    }
    // Clip generated alpha to the live selection.
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let i = (y as usize).saturating_mul(w as usize).saturating_add(x as usize);
            let Some(p) = px.get_mut(i) else { continue };
            let cover = sel.sample_channel(ox.saturating_add(x), oy.saturating_add(y), 0).clamp(0.0, 1.0);
            p[3] *= cover;
        }
    }
    // Horde windows are multiples of 64 and can hang off a small canvas; keep only the overlap.
    let (px, w, h) = crop_to_region(px, w, window, region)?;
    let ox = region.x0;
    let oy = region.y0;
    let window = region;
    let name = layer_name(&prompt);
    let fmt_src = st.doc.layer(id).and_then(|l| l.surface()).map(|s| s.format()).unwrap_or_else(|| st.doc.pixel_format());
    let layer = s.edit("Generative Fill", |doc, active| {
        let fmt = PixelFormat { alpha: true, ..fmt_src };
        let n = fmt.channels();
        let mut buf = vec![0.0f32; (w as usize).saturating_mul(h as usize).saturating_mul(n.max(1))];
        for (i, src) in px.iter().enumerate() {
            let mut enc = [0.0f32; 8];
            from_rgba_into(&fmt, *src, &mut enc);
            let dst = i.saturating_mul(n);
            if let (Some(slice), Some(enc_n)) = (buf.get_mut(dst..dst + n), enc.get(..n)) {
                slice.copy_from_slice(enc_n);
            }
        }
        match output {
            "current" => {
                let locks = doc.effective_locks(id);
                let l = doc.layer_mut(id).ok_or(EngineError::NoLayer(id))?;
                if locks.all || locks.pixels {
                    return Err(EngineError::Other(format!("Could not complete your request because the layer \"{}\" is locked", l.name)));
                }
                let surf = l.surface_mut().ok_or_else(|| EngineError::Other("not a pixel layer".into()))?;
                composite_over(surf, window, w as usize, h as usize, &px);
                Ok(id)
            }
            _ => {
                let mut l = Layer::raster(doc.next_layer_name(&name), fmt);
                if let Some(surf) = l.surface_mut() {
                    surf.write_region(window, &buf);
                    surf.prune();
                }
                let nid = doc.insert_above(Some(id), l);
                *active = Some(nid);
                Ok(nid)
            }
        }
    })?;
    Ok(json!({
        "layer": layer.0,
        "origin": [ox, oy],
        "size": [w, h],
        "prompt": prompt,
    }))
}

/// Keep the overlap of `window` with the canvas (`region`).
pub(crate) fn crop_to_region(px: Vec<[f32; 4]>, w: u32, window: Rect, region: Rect) -> Result<(Vec<[f32; 4]>, u32, u32)> {
    if region == window {
        return Ok((px, w, window.height()));
    }
    let rw = region.width();
    let rh = region.height();
    let n = u64::from(rw).saturating_mul(u64::from(rh));
    if n > MAX_PIXELS {
        return Err(bad("the generated image is too large"));
    }
    let dx = region.x0.checked_sub(window.x0).ok_or_else(|| bad("the generated image origin overflows the canvas"))?;
    let dy = region.y0.checked_sub(window.y0).ok_or_else(|| bad("the generated image origin overflows the canvas"))?;
    let mut out = Vec::new();
    if out.try_reserve(n as usize).is_err() {
        return Err(bad("the generated image is too large"));
    }
    let src_w = w as usize;
    for y in 0..rh as i32 {
        for x in 0..rw as i32 {
            let sx = dx.saturating_add(x);
            let sy = dy.saturating_add(y);
            if sx < 0 || sy < 0 {
                out.push([0.0; 4]);
                continue;
            }
            let i = (sy as usize).saturating_mul(src_w).saturating_add(sx as usize);
            out.push(px.get(i).copied().unwrap_or([0.0; 4]));
        }
    }
    Ok((out, rw, rh))
}

pub(crate) fn origin_xy(p: &Value, hb: Rect) -> Result<(i32, i32)> {
    if let Some(origin) = p.get("origin") {
        let a = origin.as_array().ok_or_else(|| bad("`origin` must be [x, y]"))?;
        if a.len() != 2 {
            return Err(bad("`origin` must be [x, y]"));
        }
        let n = |i: usize| {
            a.get(i)
                .and_then(Value::as_f64)
                .filter(|f| f.is_finite())
                .and_then(|v| {
                    let r = v.round();
                    (r >= f64::from(i32::MIN) && r <= f64::from(i32::MAX)).then_some(r as i32)
                })
                .ok_or_else(|| bad("`origin` must be [x, y] finite 32-bit coordinates"))
        };
        return Ok((n(0)?, n(1)?));
    }
    let ox = int_i32(CMD, p, "x")?.or(int_i32(CMD, p, "originX")?).unwrap_or(hb.x0);
    let oy = int_i32(CMD, p, "y")?.or(int_i32(CMD, p, "originY")?).unwrap_or(hb.y0);
    Ok((ox, oy))
}

/// Blend generated pixels over an existing layer (straight alpha).
fn composite_over(surf: &mut Surface, window: Rect, w: usize, h: usize, px: &[[f32; 4]]) {
    let fmt = surf.format();
    let mut existing = surf.read_region(window);
    let en = fmt.channels();
    for y in 0..h {
        for x in 0..w {
            let i = y.saturating_mul(w).saturating_add(x);
            let Some(src) = px.get(i) else { continue };
            let a = src[3];
            if a <= 0.0 {
                continue;
            }
            let di = i.saturating_mul(en);
            let Some(dst_ch) = existing.get(di..di + en) else { continue };
            let dst = photocraft_raster::to_rgba(&fmt, dst_ch);
            let out = [src[0] * a + dst[0] * (1.0 - a), src[1] * a + dst[1] * (1.0 - a), src[2] * a + dst[2] * (1.0 - a), a + dst[3] * (1.0 - a)];
            let mut enc = [0.0f32; 8];
            from_rgba_into(&fmt, out, &mut enc);
            if let (Some(slot), Some(enc_n)) = (existing.get_mut(di..di + en), enc.get(..en)) {
                slot.copy_from_slice(enc_n);
            }
        }
    }
    surf.write_region(window, &existing);
}

pub fn specs() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: CMD,
        label: "Generative Fill…",
        menu: &["Edit"],
        shortcut: None,
        params: r##"{"prompt":text?,"png":base64?,"webp":base64?,"width":px?,"height":px?,"rgba":[u8]?,"origin":[x,y]?,"x":px?,"y":px?,"output":"new|current"="new"}"##,
        enabled: can_generative_fill,
        run: generative_fill,
        journal: true,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_doc::LayerId;
    use serde_json::json;

    fn session(depth: u32) -> Session {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 64, "height": 48, "depth": depth, "background": "white"})).unwrap();
        s
    }

    fn px(s: &Session, x: i32, y: i32) -> Vec<f32> {
        let st = s.active().unwrap();
        st.doc.layer(st.active_layer.unwrap()).unwrap().surface().unwrap().pixel(x, y)
    }

    fn select_blob(s: &mut Session) {
        s.execute("select.rect", json!({"x": 10, "y": 8, "width": 12, "height": 10})).unwrap();
    }

    fn green_rgba(w: u32, h: u32) -> Vec<Value> {
        (0..w * h).flat_map(|_| [json!(0), json!(200), json!(0), json!(255)]).collect()
    }

    #[test]
    fn needs_a_selection_on_a_pixel_layer() {
        let mut s = session(8);
        assert!(!s.is_enabled(CMD));
        assert!(s.execute(CMD, json!({"width": 1, "height": 1, "rgba": [0, 0, 0, 255]})).is_err());
        select_blob(&mut s);
        assert!(s.is_enabled(CMD));
        s.execute("layer.newAdjustmentLayer.invert", json!({})).unwrap();
        assert!(!s.is_enabled(CMD));
    }

    #[test]
    fn places_pixels_on_a_new_layer_clipped_to_the_selection() {
        for depth in [8, 16, 32] {
            let mut s = session(depth);
            select_blob(&mut s);
            let base = s.active().unwrap().active_layer.unwrap();
            let r = s
                .execute(
                    CMD,
                    json!({
                        "prompt": "a green patch",
                        "width": 12,
                        "height": 10,
                        "origin": [10, 8],
                        "rgba": green_rgba(12, 10),
                    }),
                )
                .unwrap();
            let nid = LayerId(r["layer"].as_u64().unwrap());
            assert_ne!(nid, base);
            assert_eq!(s.active().unwrap().active_layer, Some(nid));
            let name = &s.active().unwrap().doc.layer(nid).unwrap().name;
            assert!(name.contains("Generative Fill"), "{name}");
            // Inside the selection: green from the generated pixels.
            let v = px(&s, 12, 10);
            assert!(v[1] > 0.6, "depth {depth}: {v:?}");
            assert!(v[0] < 0.2, "depth {depth}: {v:?}");
            // Outside the selection the new layer is empty; the white background shows through
            // the layer's own pixels (transparent).
            let outside = s.active().unwrap().doc.layer(nid).unwrap().surface().unwrap().pixel(1, 1);
            assert!(outside[3] < 0.01, "depth {depth}: leaked outside selection: {outside:?}");
            assert_eq!(s.active().unwrap().history.undo_label(), Some("Generative Fill"));
            s.undo();
            assert_eq!(s.active().unwrap().active_layer, Some(base));
            assert!(s.active().unwrap().doc.layer(nid).is_none());
        }
    }

    #[test]
    fn bad_params_do_not_change_the_document() {
        let mut s = session(8);
        select_blob(&mut s);
        let before = s.active().unwrap().doc.clone();
        let past = s.active().unwrap().history.past_len();
        for (name, p) in [
            ("no pixels", json!({"prompt": "x"})),
            ("bad output", json!({"width": 1, "height": 1, "rgba": [1, 2, 3, 4], "output": "elsewhere"})),
            ("short rgba", json!({"width": 2, "height": 2, "rgba": [1, 2, 3]})),
            ("zero size", json!({"width": 0, "height": 1, "rgba": [0, 0, 0, 255]})),
            ("huge side", json!({"width": 1e30, "height": 1, "rgba": [0, 0, 0, 255]})),
            ("bad origin", json!({"width": 1, "height": 1, "rgba": [0, 0, 0, 255], "origin": [1e40, 0]})),
            ("bad base64", json!({"png": "%%%%"})),
        ] {
            let err = s.execute(CMD, p).expect_err(name).to_string();
            assert!(!err.is_empty(), "{name}");
            let st = s.active().unwrap();
            assert!(std::sync::Arc::ptr_eq(&before, &st.doc), "{name}: document changed");
            assert_eq!(st.history.past_len(), past, "{name}: history step");
        }
    }

    #[test]
    fn current_output_paints_the_active_layer() {
        let mut s = session(8);
        select_blob(&mut s);
        let base = s.active().unwrap().active_layer.unwrap();
        let r = s
            .execute(
                CMD,
                json!({
                    "width": 12,
                    "height": 10,
                    "origin": [10, 8],
                    "output": "current",
                    "rgba": green_rgba(12, 10),
                }),
            )
            .unwrap();
        assert_eq!(r["layer"].as_u64(), Some(base.0));
        let v = px(&s, 12, 10);
        assert!(v[1] > 0.6, "{v:?}");
        s.undo();
        let v = px(&s, 12, 10);
        assert!(v[0] > 0.9 && v[1] > 0.9, "undo restores white: {v:?}");
    }

    #[test]
    fn pixels_hanging_off_the_canvas_are_clipped() {
        let mut s = session(8);
        select_blob(&mut s);
        let r = s
            .execute(
                CMD,
                json!({
                    "width": 64,
                    "height": 64,
                    "origin": [0, -16],
                    "rgba": green_rgba(64, 64),
                }),
            )
            .unwrap();
        assert_eq!(r["origin"], json!([0, 0]));
        assert_eq!(r["size"], json!([64, 48]));
        let nid = LayerId(r["layer"].as_u64().unwrap());
        let v = px(&s, 12, 10);
        assert!(v[1] > 0.6, "{v:?}");
        let surf = s.active().unwrap().doc.layer(nid).unwrap().surface().unwrap();
        let outside = surf.pixel(12, -1);
        assert!(outside[3] < 0.01, "wrote above the canvas: {outside:?}");
    }
}
