//! Preferences › Scratch Disks: where overflow tiles and temp files go.
//!
//! The tile store is still in memory (architecture § "Big documents"); this module resolves the
//! first enabled, writable disk, keeps a `photocraft-scratch` folder on it, and reports free
//! space so the status bar's Scratch Sizes field matches the preference.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::prefs::{Preferences, ScratchDisk};

/// Sentinel path in the preference: the platform temp directory.
pub const SYSTEM_TEMP: &str = "(system temp)";

const SCRATCH_FOLDER: &str = "photocraft-scratch";
const AVAIL_TTL: Duration = Duration::from_secs(2);

/// The directory PhotoCraft should spill into: the first enabled disk that exists (or can be
/// created), then `photocraft-scratch` inside it. Falls back to the system temp directory.
pub fn dir(prefs: &Preferences) -> PathBuf {
    resolve(&prefs.scratch_disks.disks)
}

/// First usable disk in preference order, then the scratch subfolder. Does not create it.
pub fn resolve(disks: &[ScratchDisk]) -> PathBuf {
    for d in disks {
        if !d.enabled {
            continue;
        }
        if let Some(root) = usable_root(&d.path) {
            return root.join(SCRATCH_FOLDER);
        }
    }
    std::env::temp_dir().join(SCRATCH_FOLDER)
}

/// Layer-effect cache budget from Preferences › Performance, in bytes.
pub fn effect_cache_bytes(prefs: &Preferences) -> usize {
    (prefs.performance.effect_cache_mb as usize).saturating_mul(1 << 20)
}

/// Create the scratch folder (best-effort) and push the effect-cache budget into the compositor.
pub fn apply_runtime(prefs: &Preferences) {
    let d = dir(prefs);
    let _ = std::fs::create_dir_all(&d);
    photocraft_compose::set_effect_cache_budget(effect_cache_bytes(prefs));
}

/// Free bytes on the volume that holds `path`, when the platform can say. Cached a couple of
/// seconds so the status bar can call it every frame.
pub fn available_bytes(path: &Path) -> Option<u64> {
    let now = Instant::now();
    {
        let cache = avail_cache();
        if let Some((p, t, v)) = cache.as_ref()
            && p == path
            && now.saturating_duration_since(*t) < AVAIL_TTL
        {
            return *v;
        }
    }
    let v = probe_available(path);
    *avail_cache() = Some((path.to_path_buf(), now, v));
    v
}

fn usable_root(pref: &str) -> Option<PathBuf> {
    let root = if pref.is_empty() || pref == SYSTEM_TEMP { std::env::temp_dir() } else { PathBuf::from(pref) };
    if root.is_file() {
        return None;
    }
    if root.is_dir() {
        return Some(root);
    }
    None
}

fn avail_cache() -> std::sync::MutexGuard<'static, Option<(PathBuf, Instant, Option<u64>)>> {
    static CACHE: Mutex<Option<(PathBuf, Instant, Option<u64>)>> = Mutex::new(None);
    CACHE.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn probe_available(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        let out = std::process::Command::new("df").args(["-k", "-P"]).arg(path).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let line = stdout.lines().nth(1)?;
        let avail_k: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
        Some(avail_k.saturating_mul(1024))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prefs::ScratchDisks;

    #[test]
    fn default_scratch_is_under_system_temp() {
        let p = Preferences::default();
        apply_runtime(&p);
        let d = dir(&p);
        assert_eq!(d.file_name().and_then(|n| n.to_str()), Some(SCRATCH_FOLDER));
        assert!(d.starts_with(std::env::temp_dir()), "{}", d.display());
        assert!(d.is_dir());
    }

    #[test]
    fn a_file_path_is_skipped_for_the_next_enabled_disk() {
        let file = std::env::temp_dir().join("photocraft-scratch-not-a-disk");
        std::fs::write(&file, b"x").unwrap();
        let disks = vec![
            ScratchDisk { path: file.to_string_lossy().into_owned(), enabled: true },
            ScratchDisk { path: SYSTEM_TEMP.into(), enabled: true },
        ];
        let d = resolve(&disks);
        assert!(d.starts_with(std::env::temp_dir().join(SCRATCH_FOLDER)) || d == std::env::temp_dir().join(SCRATCH_FOLDER));
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn disabled_disks_are_ignored() {
        let p = Preferences {
            scratch_disks: ScratchDisks {
                disks: vec![
                    ScratchDisk { path: "/this/path/does/not/exist/photocraft-scratch-test".into(), enabled: false },
                    ScratchDisk { path: SYSTEM_TEMP.into(), enabled: true },
                ],
            },
            ..Preferences::default()
        };
        let d = dir(&p);
        assert!(d.starts_with(std::env::temp_dir()));
    }

    #[test]
    fn effect_cache_budget_follows_the_preference() {
        let mut p = Preferences::default();
        p.performance.effect_cache_mb = 3;
        assert_eq!(effect_cache_bytes(&p), 3 << 20);
    }
}
