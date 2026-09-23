use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::scan::Format;

pub const AGENTS: &str = "AGENTS.md";

/// Provenance marker inserted when source content is appended into AGENTS.md.
pub fn merge_marker(rel: &str) -> String {
    format!("<!-- merged from {rel} by dump-shitty-claude-md -->")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    /// No AGENTS.md — move source content into AGENTS.md.
    Rename,
    /// Identical (or whitespace-equivalent) — delete the source file.
    DeleteDuplicate,
    /// Every source line already in AGENTS.md — delete the source file.
    DeleteSubsumed,
    /// AGENTS.md ⊂ source — source content replaces AGENTS.md, delete source.
    OverwriteAgents,
    /// Partial overlap — append source into AGENTS.md, delete source.
    Merge,
    /// Unsafe to touch — report only.
    Skip,
}

impl Action {
    pub fn label(self) -> &'static str {
        match self {
            Action::Rename => "rename",
            Action::DeleteDuplicate => "dedupe",
            Action::DeleteSubsumed => "dedupe",
            Action::OverwriteAgents => "replace",
            Action::Merge => "merge",
            Action::Skip => "skip",
        }
    }

    pub fn mutates(self) -> bool {
        !matches!(self, Action::Skip)
    }

    /// Lossless actions — no content merging, nothing to review.
    pub fn is_trivial(self) -> bool {
        matches!(
            self,
            Action::Rename | Action::DeleteDuplicate | Action::DeleteSubsumed
        )
    }
}

#[derive(Debug, Serialize)]
pub struct Plan {
    pub repo: PathBuf,
    /// Source file relative to the repo root (`CLAUDE.md`,
    /// `.github/copilot-instructions.md`, …).
    pub file: PathBuf,
    pub format: Format,
    pub action: Action,
    pub reason: String,
    pub warnings: Vec<String>,
    /// Instruction files inside the repo but not at a migratable root
    /// position (untouched).
    pub nested: Vec<PathBuf>,
}

/// Classify one repo-root instruction file against the repo's AGENTS.md state.
/// `source` is the absolute file path; `rel` its path inside the repo.
pub fn classify(
    repo: &Path,
    source: &Path,
    format: Format,
    nested: Vec<PathBuf>,
    agents_ignored: bool,
) -> Result<Plan> {
    let rel = source
        .strip_prefix(repo)
        .unwrap_or(source)
        .to_path_buf();
    let rel_str = rel.to_string_lossy().to_string();
    let agents = repo.join(AGENTS);
    let mut warnings = Vec::new();
    if agents_ignored {
        warnings.push(
            "AGENTS.md is gitignored here — migrated content won't be committed".into(),
        );
    }

    macro_rules! skip {
        ($reason:expr) => {
            Plan {
                repo: repo.to_path_buf(),
                file: rel.clone(),
                format,
                action: Action::Skip,
                reason: $reason,
                warnings: warnings.clone(),
                nested: nested.clone(),
            }
        };
    }

    let src_meta = source
        .symlink_metadata()
        .with_context(|| format!("{} vanished", source.display()))?;

    if crate::scan::is_bare_repo(repo) {
        return Ok(skip!("bare repo (no worktree) — left alone".into()));
    }

    if src_meta.file_type().is_symlink() {
        return Ok(skip!(format!(
            "{rel_str} is a symlink (dotfiles-managed?) — left alone"
        )));
    }

    // FIFOs/sockets would block forever on read — only regular files.
    if !src_meta.file_type().is_file() {
        return Ok(skip!(format!("{rel_str} is not a regular file — left alone")));
    }

    // Gemini CLI only loads AGENTS.md when configured to (context.fileName in
    // .gemini/settings.json) — silently migrating would orphan the file.
    if format == Format::Gemini && !gemini_reads_agents(repo) {
        return Ok(skip!(
            "Gemini CLI won't read AGENTS.md — add \"context.fileName\": [\"AGENTS.md\",\"GEMINI.md\"] to .gemini/settings.json first"
                .into()
        ));
    }

    // AGENTS.md is a symlink: identical content → deleting source is still
    // safe; anything needing a write to AGENTS.md would escape the repo.
    let agents_is_symlink = agents
        .symlink_metadata()
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);

    let src_bytes =
        fs::read(source).with_context(|| format!("cannot read {}", source.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if src_meta.nlink() > 1 {
            warnings.push(format!(
                "{rel_str} is hardlinked ({} names) — deleting removes only this name",
                src_meta.nlink()
            ));
        }
    }

    if !agents.exists() {
        attach_warnings(&src_bytes, repo, format, &mut warnings);
        return Ok(Plan {
            repo: repo.to_path_buf(),
            file: rel,
            format,
            action: Action::Rename,
            reason: format!("no AGENTS.md — rename {rel_str}"),
            warnings,
            nested,
        });
    }

    // AGENTS.md present but not a regular file/symlink (fifo, socket…) →
    // reading it would block.
    if let Ok(m) = agents.symlink_metadata() {
        if !m.file_type().is_file() && !m.file_type().is_symlink() {
            return Ok(skip!("AGENTS.md is not a regular file — left alone".into()));
        }
    }

    let agents_bytes =
        fs::read(&agents).with_context(|| format!("cannot read {}", agents.display()))?;

    let (action, reason) = compare(&src_bytes, &agents_bytes, agents_is_symlink, &rel_str);
    attach_warnings(&src_bytes, repo, format, &mut warnings);

    Ok(Plan {
        repo: repo.to_path_buf(),
        file: rel,
        format,
        action,
        reason,
        warnings,
        nested,
    })
}

/// Does `.gemini/settings.json` in this repo allow AGENTS.md as a context
/// file? `context.fileName` may be a string or an array.
fn gemini_reads_agents(repo: &Path) -> bool {
    let Ok(text) = fs::read_to_string(repo.join(".gemini/settings.json")) else {
        return false;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    match &json["context"]["fileName"] {
        serde_json::Value::String(s) => s == "AGENTS.md",
        serde_json::Value::Array(a) => a.iter().any(|v| v.as_str() == Some("AGENTS.md")),
        _ => false,
    }
}

fn compare(src: &[u8], agents: &[u8], agents_is_symlink: bool, rel: &str) -> (Action, String) {
    if src == agents {
        return (
            Action::DeleteDuplicate,
            format!("identical content → delete {rel}"),
        );
    }
    if normalize(src) == normalize(agents) {
        return (
            Action::DeleteDuplicate,
            format!("whitespace-only differences → delete {rel}"),
        );
    }
    let src_lines = line_set(src);
    let agents_lines = line_set(agents);
    if src_lines.is_subset(&agents_lines) {
        return (
            Action::DeleteSubsumed,
            format!("content already covered by AGENTS.md → delete {rel}"),
        );
    }
    if agents_is_symlink {
        return (
            Action::Skip,
            "AGENTS.md is a symlink and content differs — left alone".into(),
        );
    }
    if agents_lines.is_subset(&src_lines) {
        return (
            Action::OverwriteAgents,
            format!("{rel} covers all of AGENTS.md → replace + delete"),
        );
    }
    (
        Action::Merge,
        format!("content differs → append {rel} into AGENTS.md"),
    )
}

/// Execute a plan inside `dir` (repo root or worktree). Re-reads files.
/// Hard guard: refuses to touch anything that isn't a git repo root —
/// instruction files outside git are never removed.
pub fn apply(dir: &Path, rel: &Path, action: Action) -> Result<()> {
    if dir.join(".git").symlink_metadata().is_err() {
        anyhow::bail!("{} is not a git repo — refusing to modify", dir.display());
    }
    let source = dir.join(rel);
    let agents = dir.join(AGENTS);
    match action {
        // AGENTS.md may have appeared since classification (another source
        // file already migrated) — never clobber it, append instead.
        Action::Rename if !agents.exists() => fs::rename(&source, &agents)?,
        Action::DeleteDuplicate | Action::DeleteSubsumed => fs::remove_file(&source)?,
        Action::OverwriteAgents if !agents.exists() => {
            fs::write(&agents, fs::read(&source)?)?;
            fs::remove_file(&source)?;
        }
        Action::Rename | Action::OverwriteAgents | Action::Merge => {
            let mut merged = fs::read(&agents).unwrap_or_default();
            let src = fs::read(&source)?;
            // Idempotent: a re-run (existing branch, repeated --apply) must not
            // append the same block twice.
            let marker = merge_marker(&rel.to_string_lossy());
            if String::from_utf8_lossy(&merged).contains(&marker)
                && line_set(&src).is_subset(&line_set(&merged))
            {
                // Already merged and nothing new — just delete the source.
                fs::remove_file(&source)?;
                return Ok(());
            }
            if !merged.ends_with(b"\n") {
                merged.push(b'\n');
            }
            merged.extend_from_slice(format!("\n{marker}\n\n").as_bytes());
            merged.extend_from_slice(&src);
            if !merged.ends_with(b"\n") {
                merged.push(b'\n');
            }
            fs::write(&agents, merged)?;
            fs::remove_file(&source)?;
        }
        Action::Skip => {}
    }
    Ok(())
}

/// Whole-file compare after BOM strip, per-line trim + empty-line drop.
fn normalize(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let text = text.strip_prefix('\u{feff}').unwrap_or(text.as_ref());
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

fn line_set(bytes: &[u8]) -> HashSet<String> {
    normalize(bytes).into_iter().collect()
}

fn attach_warnings(src_bytes: &[u8], repo: &Path, format: Format, warnings: &mut Vec<String>) {
    let text = String::from_utf8_lossy(src_bytes);
    if text.lines().any(|l| l.trim_start().starts_with('@')) {
        warnings.push(
            "uses @import syntax — verify it resolves under AGENTS.md for your agents".into(),
        );
    }
    if format == Format::Claude {
        if repo.join("CLAUDE.local.md").exists() {
            warnings.push(
                "CLAUDE.local.md also present — Claude-specific local file, not migrated".into(),
            );
        }
        if repo.join("AGENTS.local.md").exists() {
            warnings.push(
                "AGENTS.local.md present — Claude Code only reads AGENTS.md; fold it in or it is dead content".into(),
            );
        }
    }
    if std::str::from_utf8(src_bytes).is_err() {
        warnings.push("source file is not valid UTF-8 — merged content may need review".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dscm-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir(dir.join(".git")).unwrap(); // satisfy the repo guard
        dir
    }

    fn plan(src: &[u8], agents: Option<&[u8]>, tag: &str) -> Action {
        let dir = tmpdir(tag);
        fs::write(dir.join("CLAUDE.md"), src).unwrap();
        if let Some(a) = agents {
            fs::write(dir.join(AGENTS), a).unwrap();
        }
        let p = classify(&dir, &dir.join("CLAUDE.md"), Format::Claude, vec![], false).unwrap();
        fs::remove_dir_all(&dir).ok();
        p.action
    }

    #[test]
    fn rename_when_no_agents() {
        assert_eq!(plan(b"rules", None, "t1"), Action::Rename);
    }

    #[test]
    fn dedupe_identical() {
        assert_eq!(plan(b"same", Some(b"same"), "t2"), Action::DeleteDuplicate);
    }

    #[test]
    fn dedupe_whitespace() {
        assert_eq!(
            plan(b"a\n\nb\n", Some(b"  a\nb\n\n\n"), "t3"),
            Action::DeleteDuplicate
        );
    }

    #[test]
    fn dedupe_subsumed() {
        assert_eq!(
            plan(b"a\nb", Some(b"header\na\nb\nc"), "t4"),
            Action::DeleteSubsumed
        );
    }

    #[test]
    fn overwrite_when_agents_subset() {
        assert_eq!(
            plan(b"a\nb\nc", Some(b"a\nb"), "t5"),
            Action::OverwriteAgents
        );
    }

    #[test]
    fn merge_partial_overlap() {
        assert_eq!(plan(b"a\nx", Some(b"a\ny"), "t6"), Action::Merge);
    }

    #[test]
    fn merge_appends_with_marker() {
        let dir = tmpdir("t7");
        fs::write(dir.join("CLAUDE.md"), b"claude rules\n").unwrap();
        fs::write(dir.join(AGENTS), b"agents rules\n").unwrap();
        apply(&dir, Path::new("CLAUDE.md"), Action::Merge).unwrap();
        let out = fs::read_to_string(dir.join(AGENTS)).unwrap();
        assert!(out.contains("agents rules"));
        assert!(out.contains("merged from CLAUDE.md"));
        assert!(out.contains("claude rules"));
        assert!(!dir.join("CLAUDE.md").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rename_moves_content() {
        let dir = tmpdir("t8");
        fs::write(dir.join("CLAUDE.md"), b"keep me\n").unwrap();
        apply(&dir, Path::new("CLAUDE.md"), Action::Rename).unwrap();
        assert_eq!(fs::read(dir.join(AGENTS)).unwrap(), b"keep me\n");
        assert!(!dir.join("CLAUDE.md").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn gemini_skipped_without_agents_config() {
        let dir = tmpdir("t9");
        fs::write(dir.join("GEMINI.md"), b"gem rules\n").unwrap();
        let p = classify(&dir, &dir.join("GEMINI.md"), Format::Gemini, vec![], false).unwrap();
        assert_eq!(p.action, Action::Skip);
        assert!(p.reason.contains("context.fileName"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn gemini_migrates_when_agents_allowed() {
        let dir = tmpdir("t10");
        fs::create_dir_all(dir.join(".gemini")).unwrap();
        fs::write(
            dir.join(".gemini/settings.json"),
            br#"{"context": {"fileName": ["GEMINI.md", "AGENTS.md"]}}"#,
        )
        .unwrap();
        fs::write(dir.join("GEMINI.md"), b"gem rules\n").unwrap();
        let p = classify(&dir, &dir.join("GEMINI.md"), Format::Gemini, vec![], false).unwrap();
        assert_eq!(p.action, Action::Rename);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn copilot_source_renames_to_root_agents() {
        let dir = tmpdir("t11");
        fs::create_dir_all(dir.join(".github")).unwrap();
        fs::write(dir.join(".github/copilot-instructions.md"), b"copilot rules\n").unwrap();
        let p = classify(
            &dir,
            &dir.join(".github/copilot-instructions.md"),
            Format::Copilot,
            vec![],
            false,
        )
        .unwrap();
        assert_eq!(p.action, Action::Rename);
        apply(&dir, &p.file, p.action).unwrap();
        assert_eq!(fs::read(dir.join(AGENTS)).unwrap(), b"copilot rules\n");
        assert!(!dir.join(".github/copilot-instructions.md").exists());
        fs::remove_dir_all(&dir).ok();
    }
}
