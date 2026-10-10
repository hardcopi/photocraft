//! Right-click spelling suggestions for type editing and type-related text fields.
//!
//! The host dictionary is `/usr/share/dict/words` when that file can be read (never on wasm).
//! Load failures and a missing file both yield an empty list, so the submenu is hidden.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};

use egui::{PopupCloseBehavior, ViewportCommand};

/// Largest Damerau–Levenshtein distance considered for a suggestion.
const MAX_DISTANCE: usize = 2;
/// Suggestions shown in a context menu.
const MAX_SUGGESTIONS: usize = 5;
/// Dictionary words longer than the misspelling by more than this are skipped.
const MAX_LEN_DELTA: usize = 2;
/// Cap the number of dictionary words scored so a huge word list cannot hitch the UI.
const MAX_SCAN: usize = 8_000;

static DICT: Mutex<Option<Arc<HashSet<String>>>> = Mutex::new(None);

fn lock_dict() -> std::sync::MutexGuard<'static, Option<Arc<HashSet<String>>>> {
    DICT.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Host word list, loaded once. Empty when the file is missing, unreadable, or on wasm.
pub fn dictionary() -> Arc<HashSet<String>> {
    let mut slot = lock_dict();
    if let Some(dict) = slot.as_ref() {
        return Arc::clone(dict);
    }
    let dict = Arc::new(load_host_dictionary());
    *slot = Some(Arc::clone(&dict));
    dict
}

fn load_host_dictionary() -> HashSet<String> {
    #[cfg(target_arch = "wasm32")]
    {
        HashSet::new()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        match std::fs::read_to_string("/usr/share/dict/words") {
            Ok(body) => body.lines().map(|line| line.trim().to_lowercase()).filter(|w| !w.is_empty()).collect(),
            Err(_) => HashSet::new(),
        }
    }
}

/// Letter-only word under the caret (character index). `None` when the caret is not on a word.
pub fn word_at(text: &str, caret: usize) -> Option<(usize, usize, String)> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return None;
    }
    let n = chars.len();
    let mut i = caret.min(n);
    if i > 0 && (i == n || !chars[i].is_alphabetic()) && chars[i - 1].is_alphabetic() {
        i -= 1;
    }
    if i >= n || !chars[i].is_alphabetic() {
        return None;
    }
    let mut start = i;
    let mut end = i + 1;
    while start > 0 && chars[start - 1].is_alphabetic() {
        start -= 1;
    }
    while end < n && chars[end].is_alphabetic() {
        end += 1;
    }
    Some((start, end, chars[start..end].iter().collect()))
}

/// Replace the character range `start..end` in `text` with `replacement`.
pub fn replace_range(text: &str, start: usize, end: usize, replacement: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let a = start.min(n);
    let b = end.min(n).max(a);
    let mut out = String::new();
    out.extend(&chars[..a]);
    out.push_str(replacement);
    out.extend(&chars[b..]);
    out
}

/// Damerau–Levenshtein (optimal string alignment) distance, or `None` when above [`MAX_DISTANCE`].
pub fn distance(a: &str, b: &str) -> Option<usize> {
    let ac: Vec<char> = a.chars().collect();
    let bc: Vec<char> = b.chars().collect();
    distance_chars(&ac, &bc, MAX_DISTANCE)
}

fn distance_chars(a: &[char], b: &[char], max: usize) -> Option<usize> {
    let (n, m) = (a.len(), b.len());
    if n.abs_diff(m) > max {
        return None;
    }
    let mut prev2 = vec![0usize; m + 1];
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur = vec![0usize; m + 1];
    for i in 1..=n {
        cur[0] = i;
        let mut row_min = cur[0];
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut v = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(prev2[j - 2] + 1);
            }
            cur[j] = v;
            row_min = row_min.min(v);
        }
        if row_min > max {
            return None;
        }
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut cur);
    }
    (prev[m] <= max).then_some(prev[m])
}

/// Up to five host-dictionary suggestions for `word`, or none when it is in the dictionary.
pub fn suggestions(word: &str) -> Vec<String> {
    suggestions_in(word, dictionary().as_ref())
}

/// Suggestions from an injected word list (tests do not need `/usr/share/dict/words`).
pub fn suggestions_in(word: &str, dict: &HashSet<String>) -> Vec<String> {
    if word.is_empty() || dict.is_empty() {
        return Vec::new();
    }
    let lower = word.to_lowercase();
    if dict.contains(&lower) {
        return Vec::new();
    }
    let first = lower.chars().next();
    let n = lower.chars().count();
    let max_len = n.saturating_add(MAX_LEN_DELTA);
    let min_len = n.saturating_sub(MAX_LEN_DELTA);
    let needle: Vec<char> = lower.chars().collect();
    let mut near = Vec::new();
    let mut far = Vec::new();
    let mut scanned = 0usize;
    for cand in dict {
        if scanned >= MAX_SCAN {
            break;
        }
        let cn = cand.chars().count();
        if cn < min_len || cn > max_len {
            continue;
        }
        if cand.chars().next() != first {
            continue;
        }
        scanned += 1;
        let cc: Vec<char> = cand.chars().collect();
        match distance_chars(&needle, &cc, MAX_DISTANCE) {
            Some(1) => {
                near.push(cand.clone());
                if near.len() >= MAX_SUGGESTIONS {
                    break;
                }
            }
            Some(2) if near.len() + far.len() < MAX_SUGGESTIONS => far.push(cand.clone()),
            _ => {}
        }
    }
    near.extend(far);
    near.truncate(MAX_SUGGESTIONS);
    near.into_iter().map(|s| match_case(word, &s)).collect()
}

fn match_case(word: &str, suggestion: &str) -> String {
    let letters: Vec<char> = word.chars().filter(|c| c.is_alphabetic()).collect();
    if letters.len() > 1 && letters.iter().all(|c| c.is_uppercase()) {
        return suggestion.to_uppercase();
    }
    let Some(first) = word.chars().next() else {
        return suggestion.to_string();
    };
    if first.is_uppercase() {
        let mut chars = suggestion.chars();
        return match chars.next() {
            Some(c) => c.to_uppercase().chain(chars).collect(),
            None => suggestion.to_string(),
        };
    }
    suggestion.to_string()
}

/// Cut / Copy / Paste / Select All plus spelling suggestions on an egui text field used for type.
pub fn text_field_menu(response: &egui::Response, text: &mut String) {
    let id = response.id;
    egui::Popup::context_menu(response).close_behavior(PopupCloseBehavior::CloseOnClick).show(|ui| {
        let caret = egui::widgets::text_edit::TextEditState::load(ui.ctx(), id)
            .and_then(|s| s.cursor.char_range())
            .map(|r| r.primary.index.0)
            .unwrap_or_else(|| text.chars().count());
        let word = word_at(text, caret);
        let sugg = word.as_ref().map(|(_, _, w)| suggestions(w)).unwrap_or_default();
        let mut picked: Option<String> = None;
        for s in &sugg {
            if ui.button(s.as_str()).clicked() {
                picked = Some(s.clone());
            }
        }
        if !sugg.is_empty() {
            ui.separator();
        }
        let (sel_a, sel_b) = egui::widgets::text_edit::TextEditState::load(ui.ctx(), id)
            .and_then(|s| s.cursor.char_range())
            .map(|r| {
                let a = r.primary.index.0;
                let b = r.secondary.index.0;
                (a.min(b), a.max(b))
            })
            .unwrap_or((caret, caret));
        let has_sel = sel_a < sel_b;
        if ui.add_enabled(has_sel, egui::Button::new(tl!("Cut"))).clicked() {
            ui.ctx().send_viewport_cmd(ViewportCommand::RequestCut);
            ui.close();
        }
        if ui.add_enabled(has_sel, egui::Button::new(tl!("Copy"))).clicked() {
            ui.ctx().send_viewport_cmd(ViewportCommand::RequestCopy);
            ui.close();
        }
        if ui.button(tl!("Paste")).clicked() {
            ui.ctx().send_viewport_cmd(ViewportCommand::RequestPaste);
            ui.close();
        }
        if ui.button(tl!("Select All")).clicked() {
            if let Some(mut st) = egui::widgets::text_edit::TextEditState::load(ui.ctx(), id) {
                let n = text.chars().count();
                st.cursor.set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(n))));
                st.store(ui.ctx(), id);
            }
            ui.close();
        }
        if let (Some(s), Some((a, b, _))) = (picked, word) {
            *text = replace_range(text, a, b, &s);
            if let Some(mut st) = egui::widgets::text_edit::TextEditState::load(ui.ctx(), id) {
                let at = a + s.chars().count();
                st.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(at))));
                st.store(ui.ctx(), id);
            }
            ui.close();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_counts_insert_delete_substitute_and_transpose() {
        assert_eq!(distance("teh", "the"), Some(1));
        assert_eq!(distance("cat", "cut"), Some(1));
        assert_eq!(distance("cat", "cats"), Some(1));
        assert_eq!(distance("cats", "cat"), Some(1));
        assert_eq!(distance("abc", "ab"), Some(1));
        assert_eq!(distance("teh", "then"), Some(2));
        assert_eq!(distance("abc", "xyz"), None);
        assert_eq!(distance("same", "same"), Some(0));
    }

    #[test]
    fn suggestions_use_injected_list_and_skip_known_words() {
        let dict: HashSet<String> = ["the", "them", "then", "tea", "other", "tool", "a", "there"].into_iter().map(str::to_string).collect();
        assert!(suggestions_in("the", &dict).is_empty(), "a dictionary word is not flagged");
        let s = suggestions_in("teh", &dict);
        assert!(s.contains(&"the".to_string()), "{s:?}");
        assert!(s.contains(&"tea".to_string()), "{s:?}");
        assert!(!s.iter().any(|w| w == "other"), "different first letter is skipped: {s:?}");
        assert!(s.len() <= 5);
        assert!(suggestions_in("teh", &HashSet::new()).is_empty());
    }

    #[test]
    fn word_at_is_letters_only() {
        assert_eq!(word_at("teh word", 1), Some((0, 3, "teh".into())));
        assert_eq!(word_at("teh word", 3), Some((0, 3, "teh".into())));
        assert_eq!(word_at("teh word", 4), Some((4, 8, "word".into())));
        assert_eq!(word_at("hello-world", 7), Some((6, 11, "world".into())));
        assert_eq!(word_at("  ", 1), None);
        assert_eq!(replace_range("teh word", 0, 3, "the"), "the word");
    }
}
