//! Agent file search: fuzzy names + content + capped reads.
//!
//! Boring-standard stack, all pure Rust, no background threads, no on-disk
//! state (an indexed watcher would only pay off for interactive keystroke
//! search; agent calls are sporadic):
//! - walking: `ignore` (gitignore-aware, skips hidden/.git by default),
//! - name ranking: `nucleo-matcher` (helix's matcher, whole-query fuzzy),
//! - content: ripgrep's `grep` crate (literal/regex, binary-aware).
//!
//! Safety policy (read-only tools still show the user files to the model,
//! which may forward them to a *cloud* provider — see tool descriptions):
//! - roots are allowlisted: the user's home plus the OS temp dir.
//!   Everything else (including `/`) refuses with an explanation.
//! - hidden files/dirs are skipped (covers `.env`, `.ssh`, `.git`),
//!   symlinks are never followed, resolved paths must stay under root,
//! - denylisted build/cache dirs (`target`, `node_modules`, …) are pruned,
//! - files over 1 MB are skipped, outputs are capped and lines truncated.
use std::path::{Path, PathBuf};

/// Roots the tools may search. Home covers the user's files; temp covers
/// scratch (and keeps the unit tests hermetic without touching home).
fn allowed_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = home_dir() {
        roots.push(home);
    }
    roots.push(std::env::temp_dir());
    roots
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

/// Resolve a requested root (or the default): canonicalized, must sit
/// inside an allowed root. Empty/blank means the home dir.
pub fn resolve_root(raw: Option<&str>) -> Result<PathBuf, String> {
    let requested = raw.map(str::trim).filter(|s| !s.is_empty());
    let base = match requested {
        Some(r) => PathBuf::from(r),
        None => home_dir().ok_or_else(|| "home directory unknown".to_string())?,
    };
    let canon = base
        .canonicalize()
        .map_err(|_| format!("not a readable directory: {}", base.display()))?;
    if !canon.is_dir() {
        return Err(format!("not a directory: {}", base.display()));
    }
    let allowed = allowed_roots();
    if allowed.iter().any(|r| {
        r.canonicalize()
            .map(|c| canon.starts_with(&c))
            .unwrap_or(false)
    }) {
        Ok(canon)
    } else {
        Err("search is limited to your home directory (pass a folder inside it, or omit root)".to_string())
    }
}

/// Directory names never descended into (build output, caches, VCS).
const DENY_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "dist",
    "build",
    ".venv",
    "venv",
    "__pycache__",
    ".cache",
];

/// Files over this size are skipped (binaries, dumps, lockfiles-gone-wild).
const MAX_FILE_BYTES: u64 = 1024 * 1024;

fn denied(entry: &ignore::DirEntry) -> bool {
    if entry.file_type().is_some_and(|t| t.is_dir()) {
        if let Some(name) = entry.file_name().to_str() {
            if DENY_DIRS.contains(&name) {
                return true;
            }
        }
    }
    // No symlink escapes: links are never followed, never read.
    if std::fs::symlink_metadata(entry.path())
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        return true;
    }
    // Size gate (one extra stat per file; keeps dumps out of results).
    if entry.file_type().is_some_and(|t| t.is_file()) {
        if entry
            .metadata()
            .map(|m| m.len() > MAX_FILE_BYTES)
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

fn walker(root: &Path) -> ignore::Walk {
    ignore::WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .follow_links(false)
        .max_depth(None)
        .filter_entry(|e| !denied(e))
        .build()
}

/// Walk cap: huge trees stop early instead of hanging a turn.
const MAX_WALK_ENTRIES: usize = 50_000;

fn collect_files(root: &Path, cap_files: usize) -> (Vec<PathBuf>, bool) {
    let mut out = Vec::new();
    let mut walked = 0usize;
    let mut cut = false;
    for entry in walker(root) {
        walked += 1;
        if walked > MAX_WALK_ENTRIES {
            cut = true;
            break;
        }
        let Ok(entry) = entry else { continue };
        if entry.file_type().is_some_and(|t| t.is_file()) {
            // Belt and suspenders next to the symlink filter: the final
            // path must still sit under root.
            if entry.path().starts_with(root) {
                out.push(entry.path().to_path_buf());
            }
            if out.len() >= cap_files {
                cut = true;
                break;
            }
        }
    }
    (out, cut)
}

fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string_lossy().into_owned())
}

/// Fuzzy file-name search. Whole-query fuzzy over the relative path
/// (spaces split into AND-atoms by the pattern parser, like fzf).
pub fn search_files(
    root: &Path,
    query: &str,
    limit: usize,
) -> Vec<(String, u32)> {
    use nucleo_matcher::{
        pattern::{AtomKind, CaseMatching, Normalization, Pattern},
        Config, Matcher,
    };
    let (files, _) = collect_files(root, 20_000);
    let rels: Vec<String> = files.iter().map(|p| rel(root, p)).collect();
    let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
    // Literal match (no `^`-prefix semantics): the model types fragments,
    // not patterns. Case-insensitive, smart normalization for paths.
    let pattern = Pattern::new(
        query,
        CaseMatching::Ignore,
        Normalization::Smart,
        AtomKind::Fuzzy,
    );
    let mut scored: Vec<(String, u32)> = pattern
        .match_list(rels.iter().map(String::as_str), &mut matcher)
        .into_iter()
        .map(|(s, score)| (s.to_string(), score))
        .collect();
    // Highest score first; ties broken short-first (closer files win).
    scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.len().cmp(&b.0.len())));
    scored.truncate(limit);
    scored
}

struct CappedSink {
    matches: Vec<(u64, String)>,
    cap: usize,
}

impl<'s> grep::searcher::Sink for CappedSink {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &grep::searcher::Searcher,
        mat: &grep::searcher::SinkMatch<'_>,
    ) -> Result<bool, std::io::Error> {
        if self.matches.len() >= self.cap {
            return Ok(false);
        }
        let line_no = mat.line_number().unwrap_or(0);
        let mut text = String::from_utf8_lossy(mat.bytes()).into_owned();
        // One line per hit in the output: flatten + truncate.
        text = text
            .chars()
            .filter(|c| *c != '\n' && *c != '\r')
            .take(200)
            .collect();
        self.matches.push((line_no, text));
        Ok(self.matches.len() < self.cap)
    }
}

fn escape_literal(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    for c in pattern.chars() {
        if ".+*?()|[]{}^$\\".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Content search over the walked files. `regex=false` (default) matches
/// the literal text; `regex=true` compiles `pattern` as a regex.
/// Returns `(hits, truncated)` with hits capped at `cap_matches`.
pub fn search_content(
    root: &Path,
    pattern: &str,
    regex: bool,
    case_insensitive: bool,
    cap_matches: usize,
) -> Result<(Vec<(String, u64, String)>, bool), String> {
    use grep::regex::RegexMatcherBuilder;
    use grep::searcher::{BinaryDetection, SearcherBuilder};
    let source = if regex {
        pattern.to_string()
    } else {
        escape_literal(pattern)
    };
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(case_insensitive)
        .build(&source)
        .map_err(|e| format!("bad pattern: {e}"))?;
    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .line_number(true)
        .build();
    let (files, _) = collect_files(root, 20_000);
    let mut hits = Vec::new();
    let mut truncated = false;
    for file in &files {
        if hits.len() >= cap_matches {
            truncated = true;
            break;
        }
        let remaining = cap_matches - hits.len();
        let mut sink = CappedSink {
            matches: Vec::new(),
            cap: remaining,
        };
        match searcher.search_path(&matcher, file, &mut sink) {
            Ok(()) => {
                for (line_no, text) in sink.matches {
                    hits.push((rel(root, file), line_no, text));
                }
            }
            // Binary/unreadable files stop this file only (quit mode).
            Err(_) => continue,
        }
    }
    Ok((hits, truncated))
}

/// Capped file read for following up on search hits. 1-based `offset`,
/// at most `limit` lines; longer lines truncate at 500 chars.
pub fn read_file(
    root: &Path,
    path: &str,
    offset: u64,
    limit: usize,
) -> Result<(Vec<(u64, String)>, u64), String> {
    let joined = root.join(path.trim_start_matches('/'));
    let canon = joined
        .canonicalize()
        .map_err(|_| format!("cannot read: {path}"))?;
    if !canon.starts_with(root) {
        return Err("path escapes the search root".to_string());
    }
    let meta = std::fs::metadata(&canon).map_err(|_| format!("cannot read: {path}"))?;
    if !meta.is_file() {
        return Err(format!("not a file: {path}"));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err("file too large to read (over 1 MB)".to_string());
    }
    let text = std::fs::read_to_string(&canon)
        .map_err(|_| format!("not readable as text: {path}"))?;
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len() as u64;
    let start = offset.saturating_sub(1).min(total) as usize;
    let end = (start + limit).min(lines.len());
    let out = lines[start..end]
        .iter()
        .enumerate()
        .map(|(i, l)| {
            (
                start as u64 + i as u64 + 1,
                l.chars().take(500).collect(),
            )
        })
        .collect();
    Ok((out, total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fixture() -> (TempDir, PathBuf) {
        let dir = std::env::temp_dir().join(format!("smartpc-search-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {\n  hello();\n}\n").unwrap();
        std::fs::write(dir.join("src/lib.rs"), "// nothing\n").unwrap();
        std::fs::write(dir.join("node_modules/x.js"), "hello();\n").unwrap();
        std::fs::write(dir.join(".git/config"), "hello();\n").unwrap();
        std::fs::write(dir.join(".env"), "SECRET=abc\n").unwrap();
        (TempDir(dir.clone()), dir)
    }

    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn fuzzy_names_rank_and_respect_ignores() {
        let (_tmp, dir) = fixture();
        let root = resolve_root(Some(dir.to_str().unwrap())).unwrap();
        let hits = search_files(&root, "man rs", 10);
        assert!(
            hits.iter().any(|(p, _)| p == "src/main.rs"),
            "main.rs ranks: {hits:?}"
        );
        // node_modules/.git/dotfiles never surface.
        assert!(!hits.iter().any(|(p, _)| p.contains("node_modules")));
        assert!(!hits.iter().any(|(p, _)| p.contains(".git")));
        // Best score first.
        let scores: Vec<u32> = hits.iter().map(|(_, s)| *s).collect();
        let mut sorted = scores.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(scores, sorted);
    }

    #[test]
    fn content_search_skips_hidden_and_caps() {
        let (_tmp, dir) = fixture();
        let root = resolve_root(Some(dir.to_str().unwrap())).unwrap();
        let (hits, _) = search_content(&root, "hello", false, true, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "src/main.rs");
        assert_eq!(hits[0].1, 2);
        // Regex mode works; bad regex fails closed with a message.
        let (rhits, _) = search_content(&root, "he..o", true, true, 10).unwrap();
        assert_eq!(rhits.len(), 1);
        assert!(search_content(&root, "([", true, true, 10).is_err());
    }

    #[test]
    fn roots_stay_inside_home_or_temp() {
        assert!(resolve_root(Some("/")).is_err());
        assert!(resolve_root(Some("/etc")).is_err());
        let tmp = resolve_root(None).or_else(|_| {
            resolve_root(Some(std::env::temp_dir().to_str().unwrap()))
        });
        assert!(tmp.is_ok());
    }

    #[test]
    fn reads_are_capped_and_cannot_escape() {
        let (_tmp, dir) = fixture();
        let root = resolve_root(Some(dir.to_str().unwrap())).unwrap();
        let (lines, total) = read_file(&root, "src/main.rs", 2, 10).unwrap();
        assert_eq!(total, 3);
        assert_eq!(lines[0].0, 2);
        assert!(read_file(&root, "../outside.rs", 1, 10).is_err());
        assert!(read_file(&root, "node_modules/x.js", 1, 10).is_ok());
    }
}
