//! Download and cache filter lists (EasyList + uBlock Origin's own lists).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub struct FilterList {
    pub id: &'static str,
    pub url: &'static str,
    /// Lists maintained by uBO are allowed to use `trusted-*` scriptlets,
    /// mirroring how uBO itself treats them.
    pub trusted: bool,
}

pub const LISTS: &[FilterList] = &[
    FilterList { id: "ubo-filters", url: "https://ublockorigin.github.io/uAssets/filters/filters.txt", trusted: true },
    FilterList { id: "ubo-privacy", url: "https://ublockorigin.github.io/uAssets/filters/privacy.txt", trusted: true },
    FilterList { id: "ubo-quick-fixes", url: "https://ublockorigin.github.io/uAssets/filters/quick-fixes.txt", trusted: true },
    FilterList { id: "ubo-unbreak", url: "https://ublockorigin.github.io/uAssets/filters/unbreak.txt", trusted: true },
    FilterList { id: "easylist", url: "https://easylist.to/easylist/easylist.txt", trusted: false },
    FilterList { id: "easyprivacy", url: "https://easylist.to/easylist/easyprivacy.txt", trusted: false },
];

pub struct LoadedList {
    pub list: &'static FilterList,
    pub text: String,
}

fn path_for(dir: &Path, list: &FilterList) -> PathBuf {
    dir.join(format!("{}.txt", list.id))
}

fn is_stale(path: &Path, max_age: Duration) -> bool {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .map_or(true, |age| age > max_age)
}

fn download(url: &str) -> Result<String, String> {
    let resp = ureq::get(url)
        .timeout(Duration::from_secs(30))
        .call()
        .map_err(|e| e.to_string())?;
    let mut body = String::new();
    std::io::Read::read_to_string(&mut resp.into_reader(), &mut body).map_err(|e| e.to_string())?;
    if body.len() < 64 {
        return Err("response too small".into());
    }
    Ok(body)
}

/// Downloads every list that is missing or older than `max_age`.
/// Returns how many lists were updated. Failures keep the previous copy.
pub fn refresh(dir: &Path, max_age: Duration) -> usize {
    let _ = std::fs::create_dir_all(dir);
    let mut updated = 0;
    for list in LISTS {
        let path = path_for(dir, list);
        if !is_stale(&path, max_age) {
            continue;
        }
        match download(list.url) {
            Ok(text) => {
                let tmp = path.with_extension("tmp");
                if std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, &path)).is_ok() {
                    log::info!("updated filter list {}", list.id);
                    updated += 1;
                }
            }
            Err(e) => log::warn!("could not download {}: {e}", list.id),
        }
    }
    updated
}

/// True if no list has been downloaded yet (first run).
pub fn none_cached(dir: &Path) -> bool {
    LISTS.iter().all(|l| !path_for(dir, l).exists())
}

pub fn load_cached(dir: &Path) -> Vec<LoadedList> {
    LISTS
        .iter()
        .filter_map(|list| {
            std::fs::read_to_string(path_for(dir, list))
                .ok()
                .map(|text| LoadedList { list, text })
        })
        .collect()
}

/// Stable FNV-1a hash used to key compiled content filters.
pub fn fingerprint(lists: &[LoadedList]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for l in lists {
        for b in l.list.id.bytes().chain(l.text.bytes()) {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    format!("{h:016x}")
}
