use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

pub fn write_then_rename(path: &Path, text: &str) -> bool {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let Some(parent) = path.parent() else { return false };
    if std::fs::create_dir_all(parent).is_err() {
        return false;
    }
    let tmp = path.with_extension(format!(
        "{}.{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let written = std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, path).is_ok();
    if !written {
        let _ = std::fs::remove_file(tmp);
    }
    written
}

