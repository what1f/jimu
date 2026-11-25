//! Built-in user documentation embedded into the `jimu` CLI binary and lazily
//! synced to `<jimu_home>/docs/` so the assistant can read it on demand.

use std::{
    fs,
    path::{Path, PathBuf},
};

struct DocFile {
    name: &'static str,
    content: &'static str,
}

const USER_DOCS: &[DocFile] = &[
    DocFile {
        name: "index.md",
        content: include_str!("../docs/index.md"),
    },
    DocFile {
        name: "usage.md",
        content: include_str!("../docs/usage.md"),
    },
    DocFile {
        name: "models.md",
        content: include_str!("../docs/models.md"),
    },
    DocFile {
        name: "customization.md",
        content: include_str!("../docs/customization.md"),
    },
];

const STAMP_FILE: &str = ".stamp";

fn docs_stamp() -> String {
    // Deterministic FNV-1a 64-bit digest over crate version and bundled doc contents
    // so docs refresh automatically whenever either the version or text changes.
    let mut hash: u64 = 0xcbf29ce484222325;
    let mut feed = |bytes: &[u8]| {
        for &b in bytes {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x100000001b3);
    };
    feed(env!("CARGO_PKG_VERSION").as_bytes());
    for doc in USER_DOCS {
        feed(doc.name.as_bytes());
        feed(doc.content.as_bytes());
    }
    format!("{}-{hash:016x}", env!("CARGO_PKG_VERSION"))
}

/// Ensure the bundled user documentation is present and up-to-date under `<jimu_home>/docs/`.
///
/// Returns `None` without failing startup if the target directory is not writable.
pub fn ensure_user_docs(jimu_home: &Path) -> Option<PathBuf> {
    let dir = jimu_home.join("docs");
    let stamp = docs_stamp();
    let stamp_path = dir.join(STAMP_FILE);

    let up_to_date = fs::read_to_string(&stamp_path)
        .ok()
        .is_some_and(|existing| existing.trim() == stamp)
        && USER_DOCS.iter().all(|doc| dir.join(doc.name).is_file());
    if up_to_date {
        return Some(dir);
    }

    fs::create_dir_all(&dir).ok()?;
    for doc in USER_DOCS {
        fs::write(dir.join(doc.name), doc.content).ok()?;
    }
    let _ = fs::write(stamp_path, format!("{stamp}\n"));
    Some(dir)
}
