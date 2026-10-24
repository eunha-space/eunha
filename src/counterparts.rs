//! Which Mastodon files each eunha module implements.
//!
//! Eunha is laid out by feature, Mastodon by Rails layer, so one module here
//! often stands for a service, a worker and a serializer there. Rather than
//! bend one layout into the other, a module names its counterparts in its
//! documentation, one or more lines of the form
//!
//! ```text
//! //! Mastodon: app/services/remove_status_service.rb, app/workers/removal_worker.rb
//! ```
//!
//! A path ending in `/` names a whole directory. The paths are checked against
//! `mastodon/files.txt`, the tracked release's tree, so a header cannot name a
//! file that does not exist, and adopting a release that renames or deletes
//! one fails the suite until the header is corrected. `eunha-schema changes`
//! reads the same headers the other way round: for each upstream file changed
//! between two releases, the eunha modules to re-read.
//!
//! Every module declares a header, `Mastodon: none` when it has no
//! counterpart, so that a changed upstream file no module names means eunha
//! does not implement it rather than that a header was forgotten.
//!
//! Mastodon: none

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The marker a counterpart line starts with, after `//!`.
const MARKER: &str = "Mastodon:";

/// The tracked release's tree, as recorded by `eunha-schema record-files`.
const FILES: &str = include_str!("../mastodon/files.txt");

/// One Mastodon path a module names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Counterpart {
    /// The module's file, relative to the crate root (`src/feed.rs`).
    pub module: PathBuf,
    /// 1-based line of the header in that file.
    pub line: usize,
    /// Path in the Mastodon repository; ends in `/` for a directory.
    pub path: String,
}

/// Whether an upstream path is part of what eunha tracks: the server, not its
/// web client, translations, test suite or emoji images.
///
/// `db/migrate` and `db/post_migrate` are left out because `eunha-schema plan`
/// already covers them, one migration at a time.
#[must_use]
pub fn in_scope(path: &str) -> bool {
    const INCLUDED: [&str; 5] = ["app/", "config/", "db/views/", "lib/", "streaming/"];
    const EXCLUDED: [&str; 3] = ["app/javascript/", "config/locales", "lib/assets/"];
    INCLUDED.iter().any(|p| path.starts_with(p)) && !EXCLUDED.iter().any(|p| path.starts_with(p))
}

/// The tag `mastodon/files.txt` was recorded from, and its paths.
///
/// # Panics
/// Panics if the file lacks its `# vX.Y.Z` first line, which
/// `eunha-schema record-files` always writes.
#[must_use]
pub fn tracked_files() -> (&'static str, BTreeSet<&'static str>) {
    let mut lines = FILES.lines();
    let tag = lines
        .next()
        .and_then(|l| l.strip_prefix("# "))
        .expect("mastodon/files.txt does not start with `# vX.Y.Z`");
    (tag, lines.filter(|l| !l.is_empty()).collect())
}

/// Whether `path` names a file in `files`, or a directory holding one.
#[must_use]
pub fn exists(path: &str, files: &BTreeSet<&str>) -> bool {
    if path.ends_with('/') {
        files
            .range(path..)
            .next()
            .is_some_and(|f| f.starts_with(path))
    } else {
        files.contains(path)
    }
}

/// Whether the counterpart `path` covers the upstream file `file`.
#[must_use]
pub fn covers(path: &str, file: &str) -> bool {
    if path.ends_with('/') {
        file.starts_with(path)
    } else {
        path == file
    }
}

/// Parse the counterpart lines out of one module's source.
///
/// `Mastodon: none` declares a module with no counterpart and yields nothing.
/// A path that is not a bare repository path (a stray backtick, a class name)
/// is an error, so the check cannot be satisfied by text it does not read.
pub fn parse(module: &Path, source: &str) -> Result<Vec<Counterpart>> {
    let mut found = Vec::new();
    for (index, line) in source.lines().enumerate() {
        let Some(doc) = line.trim_start().strip_prefix("//!") else {
            continue;
        };
        let Some(rest) = doc.trim().strip_prefix(MARKER) else {
            continue;
        };
        let rest = rest.trim();
        if rest == "none" {
            continue;
        }
        for path in rest.split(',').map(str::trim) {
            anyhow::ensure!(
                !path.is_empty()
                    && path
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c)),
                "{}:{}: `{path}` is not a Mastodon repository path",
                module.display(),
                index + 1
            );
            found.push(Counterpart {
                module: module.to_path_buf(),
                line: index + 1,
                path: path.to_owned(),
            });
        }
    }
    Ok(found)
}

/// Every Rust file under `src/`, given the crate root, relative to it.
pub fn modules(root: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut pending = vec![root.join("src")];
    while let Some(dir) = pending.pop() {
        let entries =
            std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))?;
        for entry in entries {
            let path = entry?.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                found.push(path.strip_prefix(root).unwrap_or(&path).to_path_buf());
            }
        }
    }
    found.sort();
    Ok(found)
}

/// Whether a module's source has a counterpart line, `none` included.
#[must_use]
pub fn declares(source: &str) -> bool {
    source.lines().any(|line| {
        line.trim_start()
            .strip_prefix("//!")
            .is_some_and(|doc| doc.trim().starts_with(MARKER))
    })
}

/// Every counterpart named under `src/`, given the crate root.
pub fn scan(root: &Path) -> Result<Vec<Counterpart>> {
    let mut found = Vec::new();
    for module in modules(root)? {
        let path = root.join(&module);
        let source = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        found.extend(parse(&module, &source)?);
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> &'static Path {
        Path::new(env!("CARGO_MANIFEST_DIR"))
    }

    #[test]
    fn files_are_recorded_for_the_tracked_release() {
        let (tag, files) = tracked_files();
        assert_eq!(
            tag,
            format!("v{}", crate::version::MASTODON),
            "mastodon/files.txt is from another release; run \
             `mise run mastodon:files` after adopting one"
        );
        assert!(files.iter().all(|f| in_scope(f)));
        assert!(files.contains("app/services/post_status_service.rb"));
    }

    #[test]
    fn every_counterpart_exists_in_the_tracked_release() {
        let (tag, files) = tracked_files();
        let missing: Vec<String> = scan(root())
            .unwrap()
            .into_iter()
            .filter(|c| !exists(&c.path, &files))
            .map(|c| format!("{}:{}: {}", c.module.display(), c.line, c.path))
            .collect();
        assert!(
            missing.is_empty(),
            "these Mastodon paths are not in {tag} (or are outside what \
             mastodon/files.txt records):\n  {}",
            missing.join("\n  ")
        );
    }

    #[test]
    fn every_module_declares_its_counterparts() {
        let undeclared: Vec<String> = modules(root())
            .unwrap()
            .into_iter()
            .filter(|m| !declares(&std::fs::read_to_string(root().join(m)).unwrap()))
            .map(|m| m.display().to_string())
            .collect();
        assert!(
            undeclared.is_empty(),
            "these modules name no Mastodon counterpart; add `//! Mastodon: <path>` \
             to the module doc, or `//! Mastodon: none`:\n  {}",
            undeclared.join("\n  ")
        );
    }

    #[test]
    fn parses_lists_directories_and_none() {
        let source = "//! Text.\n//!\n//! Mastodon: app/a.rb, app/b/\n//! Mastodon: none\n";
        let found = parse(Path::new("src/x.rs"), source).unwrap();
        let paths: Vec<_> = found.iter().map(|c| (c.line, c.path.as_str())).collect();
        assert_eq!(paths, [(3, "app/a.rb"), (3, "app/b/")]);
    }

    #[test]
    fn rejects_what_is_not_a_path() {
        assert!(parse(Path::new("src/x.rs"), "//! Mastodon: `app/a.rb`").is_err());
        assert!(parse(Path::new("src/x.rs"), "//! Mastodon: app/a.rb,").is_err());
    }

    #[test]
    fn directories_cover_what_is_under_them() {
        let files = BTreeSet::from(["app/lib/a.rb", "app/lib/b/c.rb"]);
        assert!(exists("app/lib/b/", &files));
        assert!(!exists("app/li/", &files));
        assert!(!exists("app/lib/b", &files));
        assert!(covers("app/lib/", "app/lib/b/c.rb"));
        assert!(!covers("app/lib/a.rb", "app/lib/a.rbx"));
    }
}
