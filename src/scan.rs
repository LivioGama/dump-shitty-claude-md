use anyhow::Result;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::{WalkBuilder, WalkState};
use std::collections::{BTreeSet, HashMap};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// A vendor-specific instruction file the tool can consolidate into AGENTS.md.
/// `Claude` is always migrated; every other format is opt-in.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, clap::ValueEnum, serde::Serialize,
)]
#[serde(rename_all = "kebab-case")]
#[value(rename_all = "kebab-case")]
pub enum Format {
    /// CLAUDE.md — Claude Code (AGENTS.md fallback ≥2.1.277)
    Claude,
    /// .cursorrules — deprecated Cursor legacy file
    Cursor,
    /// .windsurfrules — Windsurf legacy file
    Windsurf,
    /// .clinerules — Cline single-file rules
    Cline,
    /// .github/copilot-instructions.md — GitHub Copilot repo-wide
    Copilot,
    /// GEMINI.md — Gemini CLI (migrated only when context.fileName allows AGENTS.md)
    Gemini,
    /// .rules — Zed legacy rules file
    Zed,
    /// CONVENTIONS.md / conventions.md — Aider
    Aider,
    /// .roorules — Roo Code legacy file
    Roo,
    /// .kilocoderules — Kilo Code legacy file
    Kilo,
    /// .goosehints — Goose (Block) hints file
    Goose,
    /// WARP.md — Warp terminal agent
    Warp,
}

impl Format {
    /// Every opt-in format — everything except `Claude`, which is always in
    /// scope. Drives `--all-formats` and the interactive picker.
    pub const OPT_IN: &'static [Format] = &[
        Format::Cursor,
        Format::Windsurf,
        Format::Cline,
        Format::Copilot,
        Format::Gemini,
        Format::Zed,
        Format::Aider,
        Format::Roo,
        Format::Kilo,
        Format::Goose,
        Format::Warp,
    ];

    /// Display name used in reports and merge markers.
    pub fn label(self) -> &'static str {
        match self {
            Format::Claude => "CLAUDE.md",
            Format::Cursor => ".cursorrules",
            Format::Windsurf => ".windsurfrules",
            Format::Cline => ".clinerules",
            Format::Copilot => ".github/copilot-instructions.md",
            Format::Gemini => "GEMINI.md",
            Format::Zed => ".rules",
            Format::Aider => "CONVENTIONS.md",
            Format::Roo => ".roorules",
            Format::Kilo => ".kilocoderules",
            Format::Goose => ".goosehints",
            Format::Warp => "WARP.md",
        }
    }
}

/// Map a file path to its instruction format, if it is one we handle.
/// `copilot-instructions.md` only counts inside a `.github` directory.
fn format_for(path: &Path) -> Option<Format> {
    let name = path.file_name()?.to_str()?;
    match name {
        "CLAUDE.md" => Some(Format::Claude),
        ".cursorrules" => Some(Format::Cursor),
        ".windsurfrules" => Some(Format::Windsurf),
        ".clinerules" => Some(Format::Cline),
        ".roorules" => Some(Format::Roo),
        ".kilocoderules" => Some(Format::Kilo),
        ".goosehints" => Some(Format::Goose),
        "WARP.md" => Some(Format::Warp),
        ".rules" => Some(Format::Zed),
        "GEMINI.md" => Some(Format::Gemini),
        // Aider's documented name is uppercase; a lowercase conventions.md is
        // usually a generic doc, not an agent file — don't touch it.
        "CONVENTIONS.md" => Some(Format::Aider),
        "copilot-instructions.md" => path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .filter(|n| *n == ".github")
            .map(|_| Format::Copilot),
        _ => None,
    }
}

/// An instruction file found during the walk, classified by repo relationship.
#[derive(Debug)]
pub struct FoundFile {
    pub path: PathBuf,
    /// Nearest ancestor directory containing `.git` (dir or file).
    /// `None` → stray file outside any repo.
    pub repo_root: Option<PathBuf>,
    pub format: Format,
}

impl FoundFile {
    /// True when the file sits at its format's canonical position at the repo
    /// root (`.github/copilot-instructions.md` counts as root-level).
    pub fn is_repo_root_file(&self) -> bool {
        let Some(root) = self.repo_root.as_deref() else {
            return false;
        };
        match self.format {
            Format::Copilot => self.path == root.join(".github/copilot-instructions.md"),
            _ => self.path.parent() == Some(root),
        }
    }

    /// Path relative to the repo root, e.g. `CLAUDE.md` or
    /// `.github/copilot-instructions.md`.
    pub fn rel(&self) -> PathBuf {
        self.repo_root
            .as_deref()
            .and_then(|r| self.path.strip_prefix(r).ok())
            .unwrap_or(&self.path)
            .to_path_buf()
    }
}

/// Directories never descended into (matched on dir name).
const PRUNE_NAMES: &[&str] = &[
    ".git",
    "node_modules",
    "Library",
    ".Trash",
    ".cache",
    ".cargo",
    ".rustup",
    ".npm",
    ".bun",
    ".nvm",
    ".pyenv",
    ".rbenv",
    ".volta",
    ".deno",
    ".gradle",
    ".m2",
    ".ollama",
    "miniconda3",
    "anaconda3",
    ".conda",
    "OrbStack",
    // Vendored dependencies + build output across ecosystems: a CLAUDE.md in
    // these is generated/vendored content, not a user file. Generic names
    // like `dist`/`build` stay scannable — they can hold real docs.
    "Pods",
    "Carthage",
    "vendor",
    "deps",
    "target",
    ".venv",
    "venv",
    "site-packages",
    "bower_components",
    "jspm_packages",
    ".terraform",
    "DerivedData",
    "__pycache__",
    ".tox",
    ".dart_tool",
    ".pub-cache",
    "elm-stuff",
    ".stack-work",
    ".pnpm-store",
    ".next",
    ".nuxt",
];

/// Path prefixes never scanned, regardless of flags.
fn always_excluded(home: &Path, p: &Path) -> bool {
    p.starts_with(home.join(".claude"))
}

/// Prune on a path *suffix* (component-wise), for vendored/derived trees whose
/// dirname alone is too generic to prune by name.
const PRUNE_SUFFIXES: &[&[&str]] = &[&["go", "pkg", "mod"], &["Library", "Caches"]];

fn prune_by_suffix(p: &Path) -> bool {
    let depth = p.components().count();
    PRUNE_SUFFIXES.iter().any(|suffix| {
        suffix.len() <= depth
            && p.components()
                .rev()
                .take(suffix.len())
                .zip(suffix.iter().rev())
                .all(|(c, s)| c.as_os_str() == OsStr::new(s))
    })
}

/// True for directories that hold *non-migratable* rule formats — structured
/// rules with frontmatter/globs that AGENTS.md cannot express. Report-only.
fn is_rule_dir(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let parent = || {
        path.parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("")
    };
    match name {
        "rules" => matches!(
            parent(),
            ".cursor" | ".windsurf" | ".roo" | ".continue" | ".claude" | ".kilocode" | ".trae"
        ),
        "steering" => parent() == ".kiro",
        "microagents" => parent() == ".openhands",
        "instructions" | "agents" => parent() == ".github",
        ".clinerules" | ".roo" | ".junie" | ".amazonq" | ".openhands" => true,
        _ => false,
    }
}

/// .gitignore matcher for a repo — root .gitignore + .git/info/exclude +
/// the conventional global ignore (~/.config/git/ignore). Approximation:
/// nested .gitignore files and a custom core.excludesFile aren't consulted.
fn repo_gitignore(root: &Path) -> Gitignore {
    let mut b = GitignoreBuilder::new(root);
    for f in [
        root.join(".gitignore"),
        root.join(".git/info/exclude"),
        dirs_home().join(".config/git/ignore"),
    ] {
        if f.is_file() {
            b.add(f);
        }
    }
    b.build().unwrap_or_else(|_| Gitignore::empty())
}

pub struct ScanResult {
    pub found: Vec<FoundFile>,
    /// Files whose name is a case/spelling variant of CLAUDE.md or AGENTS.md —
    /// dead weight (no agent reads them), report-only.
    pub variants: Vec<PathBuf>,
    /// Global-ish CLAUDE.md files we refuse to touch (e.g. `~/CLAUDE.md`),
    /// listed so the user knows they exist.
    pub protected: Vec<PathBuf>,
    /// Non-migratable rule directories inside repos (.cursor/rules,
    /// .github/instructions, .junie, …) — reported so nothing is missed.
    pub rule_dirs: Vec<PathBuf>,
    /// Repo-root instruction files matched by .gitignore — personal files
    /// the user deliberately keeps out of git; migrating them to AGENTS.md
    /// would un-ignore (and potentially commit) them. Report-only.
    pub gitignored: Vec<PathBuf>,
    /// Repo roots whose AGENTS.md path is gitignored — a merge/rename there
    /// produces a file `--pr` can't commit. Surfaced as a plan warning.
    pub ignored_agents: BTreeSet<PathBuf>,
    /// Linked-worktree checkouts (`.git` file → `…/.git/worktrees/…`) — their
    /// instruction files are diverted here, never migrated: the main
    /// checkout is the one that gets the AGENTS.md.
    pub worktrees: Vec<PathBuf>,
    /// Walker errors (permission denied etc.), capped.
    pub errors: Vec<String>,
}

/// Walk `root` in parallel, yielding every known instruction file.
pub fn scan(root: &Path, excludes: &[PathBuf], follow_links: bool) -> Result<ScanResult> {
    let home = dirs_home();
    let excludes: Vec<PathBuf> = excludes
        .iter()
        .map(|e| e.canonicalize().unwrap_or_else(|_| e.clone()))
        .collect();

    let found = Mutex::new(Vec::new());
    let variants = Mutex::new(Vec::new());
    let protected = Mutex::new(Vec::new());
    let rule_dirs = Mutex::new(Vec::new());
    let errors = Mutex::new(Vec::new());
    let repo_cache = Mutex::new(HashMap::new());
    let home_claude = home.join("CLAUDE.md");

    let walker = WalkBuilder::new(root)
        .follow_links(follow_links)
        .hidden(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .require_git(false)
        .threads(0)
        .build_parallel();

    walker.run(|| {
        let found = &found;
        let variants = &variants;
        let protected = &protected;
        let rule_dirs = &rule_dirs;
        let errors = &errors;
        let excludes = &excludes;
        let home = &home;
        let home_claude = &home_claude;
        let repo_cache = &repo_cache;
        Box::new(move |entry| {
            let entry = match entry {
                Ok(e) => e,
                Err(err) => {
                    let mut errs = errors.lock().unwrap();
                    if errs.len() < 50 {
                        errs.push(err.to_string());
                    }
                    return WalkState::Continue;
                }
            };

            let path = entry.path();

            if always_excluded(home, path) || excludes.iter().any(|e| path.starts_with(e)) {
                return WalkState::Skip;
            }

            let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
            if is_dir {
                if path != root
                    && (prune_by_suffix(path)
                        || path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|name| PRUNE_NAMES.contains(&name)))
                {
                    return WalkState::Skip;
                }
                if is_rule_dir(path) && find_repo_root(path, repo_cache).is_some() {
                    rule_dirs.lock().unwrap().push(path.to_path_buf());
                    return WalkState::Skip;
                }
                return WalkState::Continue;
            }

            // ~/CLAUDE.md is a global instruction file — report, never touch.
            if path == home_claude.as_path() {
                protected.lock().unwrap().push(path.to_path_buf());
                return WalkState::Continue;
            }

            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                return WalkState::Continue;
            };
            if let Some(format) = format_for(path) {
                let repo_root = find_repo_root(path.parent().unwrap_or(Path::new("/")), repo_cache);
                // Non-Claude formats only matter inside repos — outside one
                // they are dead files nobody loads, not worth reporting.
                if format != Format::Claude && repo_root.is_none() {
                    return WalkState::Continue;
                }
                found.lock().unwrap().push(FoundFile {
                    path: path.to_path_buf(),
                    repo_root,
                    format,
                });
            } else if name != "AGENTS.md"
                && (name.eq_ignore_ascii_case("claude.md")
                    || name.eq_ignore_ascii_case("agents.md")
                    || name.eq_ignore_ascii_case("agent.md"))
            {
                // Non-exact spellings — dead weight on case-sensitive systems.
                variants.lock().unwrap().push(path.to_path_buf());
            }
            WalkState::Continue
        })
    });

    let mut found = found.into_inner().unwrap();
    found.sort_by(|a, b| a.path.cmp(&b.path));

    // Linked worktrees share the main checkout's AGENTS.md — migrating them
    // separately would double-apply. Drop their files entirely; report the
    // worktree root so the user sees it was deliberately ignored.
    let worktrees: BTreeSet<PathBuf> = found
        .iter()
        .filter_map(|f| f.repo_root.clone())
        .filter(|r| is_linked_worktree(r))
        .collect();
    if !worktrees.is_empty() {
        found.retain(|f| {
            f.repo_root
                .as_ref()
                .is_none_or(|r| !worktrees.contains(r))
        });
    }
    let worktrees: Vec<PathBuf> = worktrees.into_iter().collect();

    let mut variants = variants.into_inner().unwrap();
    variants.sort();
    let mut rule_dirs: Vec<PathBuf> = rule_dirs.into_inner().unwrap();
    rule_dirs.sort();
    rule_dirs.dedup();
    // Rule dirs inside a worktree are ignored along with the worktree.
    rule_dirs.retain(|d| !worktrees.iter().any(|w| d.starts_with(w)));

    // .gitignore evaluation — a gitignored root instruction file is a
    // personal file; renaming it would un-ignore (and possibly commit) it.
    // Also flag repos where the AGENTS.md *target* is ignored.
    let mut matchers: HashMap<PathBuf, Gitignore> = HashMap::new();
    let mut ignored_agents = BTreeSet::new();
    let mut gitignored = Vec::new();
    for root in found
        .iter()
        .filter_map(|f| f.repo_root.clone())
        .collect::<BTreeSet<_>>()
    {
        let m = matchers
            .entry(root.clone())
            .or_insert_with(|| repo_gitignore(&root));
        if m.matched(root.join(crate::plan::AGENTS), false).is_ignore() {
            ignored_agents.insert(root);
        }
    }
    let found: Vec<FoundFile> = found
        .into_iter()
        .filter(|f| {
            if f.is_repo_root_file()
                && f.repo_root
                    .as_ref()
                    .and_then(|r| matchers.get(r))
                    .is_some_and(|m| m.matched(&f.path, false).is_ignore())
            {
                gitignored.push(f.path.clone());
                return false;
            }
            true
        })
        .collect();

    Ok(ScanResult {
        found,
        variants,
        protected: protected.into_inner().unwrap(),
        rule_dirs,
        gitignored,
        ignored_agents,
        worktrees,
        errors: errors.into_inner().unwrap(),
    })
}

/// A linked worktree's `.git` is a file containing
/// `gitdir: <main>/.git/worktrees/<name>`. Submodules point into
/// `.git/modules/` instead — those are real repos and stay in scope.
/// Anything else (`.git` dir, unreadable/foreign content) is a normal repo.
fn is_linked_worktree(root: &Path) -> bool {
    let git = root.join(".git");
    if !git.is_file() {
        return false;
    }
    std::fs::read_to_string(&git)
        .ok()
        .and_then(|s| s.trim().strip_prefix("gitdir:").map(str::trim).map(String::from))
        .is_some_and(|d| d.replace('\\', "/").contains("/worktrees/"))
}

/// Nearest ancestor (starting at `dir`) containing a `.git` entry
/// or looking like a bare repo (HEAD + objects, no worktree).
/// `cache` memoizes per-directory answers across the parallel walk.
fn find_repo_root(dir: &Path, cache: &Mutex<HashMap<PathBuf, Option<PathBuf>>>) -> Option<PathBuf> {
    let mut trail = Vec::new();
    let mut cur = Some(dir);
    let hit = loop {
        let Some(d) = cur else { break None };
        if let Some(cached) = cache.lock().unwrap().get(d) {
            break cached.clone();
        }
        if d.join(".git").symlink_metadata().is_ok() || is_bare_repo(d) {
            break Some(d.to_path_buf());
        }
        trail.push(d.to_path_buf());
        cur = d.parent();
    };
    // Every directory on the trail resolves to the same nearest repo.
    let mut c = cache.lock().unwrap();
    for d in trail {
        c.insert(d, hit.clone());
    }
    hit
}

/// Bare repo heuristic: has HEAD + objects + refs but no `.git`.
pub fn is_bare_repo(dir: &Path) -> bool {
    dir.join("HEAD").is_file()
        && dir.join("objects").is_dir()
        && dir.join("refs").is_dir()
        && !dir.join(".git").exists()
}

fn dirs_home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every format's canonical file must map back to that format — keeps
    /// `label()` and `format_for()` from drifting apart.
    #[test]
    fn format_label_roundtrip() {
        let root = Path::new("/repo");
        for f in std::iter::once(Format::Claude).chain(Format::OPT_IN.iter().copied()) {
            assert_eq!(
                format_for(&root.join(f.label())),
                Some(f),
                "{:?} label {:?} not recognized",
                f,
                f.label()
            );
        }
    }

    #[test]
    fn linked_worktree_detection() {
        let base = std::env::temp_dir().join(format!("dscm-wt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);

        // Linked worktree: .git file → gitdir: …/.git/worktrees/x
        let wt = base.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt.join(".git"),
            "gitdir: /repos/main/.git/worktrees/wt\n",
        )
        .unwrap();
        assert!(is_linked_worktree(&wt));

        // Submodule: .git file → gitdir: …/.git/modules/x — real repo, kept.
        let sub = base.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join(".git"), "gitdir: ../.git/modules/sub\n").unwrap();
        assert!(!is_linked_worktree(&sub));

        // Normal checkout: .git directory.
        let main = base.join("main");
        std::fs::create_dir_all(main.join(".git")).unwrap();
        assert!(!is_linked_worktree(&main));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn prune_rules() {
        assert!(prune_by_suffix(Path::new("/a/go/pkg/mod")));
        assert!(prune_by_suffix(Path::new("/a/Library/Caches")));
        assert!(!prune_by_suffix(Path::new("/a/mod")));
        assert!(!prune_by_suffix(Path::new("/a/Library")));
        assert!(is_rule_dir(Path::new("/r/.cursor/rules")));
        assert!(is_rule_dir(Path::new("/r/.kiro/steering")));
        assert!(!is_rule_dir(Path::new("/r/rules")));
        assert!(!is_rule_dir(Path::new("/r/src/rules")));
    }
}
