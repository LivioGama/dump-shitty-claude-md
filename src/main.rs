mod gitops;
mod plan;
mod scan;

use clap::Parser;
use dialoguer::{Confirm, MultiSelect, Select};
use plan::{Action, Plan};
use scan::{Format, FoundFile};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::io::IsTerminal;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "dump-shitty-claude-md",
    about = "Consolidate agent instruction files (CLAUDE.md and friends) into AGENTS.md\n\
             across all repos in your home directory. One file serves every agent.",
    version
)]
struct Cli {
    /// Directory to scan [default: $HOME]
    path: Option<PathBuf>,

    /// Apply file changes (default: dry run)
    #[arg(long)]
    apply: bool,

    /// Apply + commit on a branch + push + open a PR (implies --apply).
    /// The migration lives on the PR branch; your checkout is left untouched.
    #[arg(long)]
    pr: bool,

    /// Git strategy for --pr instead of prompting
    #[arg(long, value_enum)]
    strategy: Option<gitops::Strategy>,

    /// Non-interactive: accept recommended defaults (CLAUDE.md only, worktree)
    #[arg(short = 'y', long)]
    yes: bool,

    /// Extra instruction formats to migrate alongside CLAUDE.md (repeatable,
    /// comma-separated): cursor, windsurf, cline, copilot, gemini, zed,
    /// aider, roo, kilo, goose, warp
    #[arg(long, value_enum, value_delimiter = ',')]
    migrate: Vec<Format>,

    /// Migrate every supported format (equivalent to picking all in the prompt)
    #[arg(long)]
    all_formats: bool,

    /// Only lossless actions (rename + dedupe) — merges are report-only
    #[arg(long)]
    trivial: bool,

    /// Move stray CLAUDE.md files (not inside a git repo) to the Trash.
    /// Without this flag an interactive run asks once; -y alone never trashes.
    #[arg(long)]
    trash_strays: bool,

    /// Emit the plan as JSON
    #[arg(long)]
    json: bool,

    /// Extra directory to exclude (repeatable)
    #[arg(long)]
    exclude: Vec<PathBuf>,

    /// Descend into symlinked directories (default: skip — avoids loops)
    #[arg(long)]
    follow_links: bool,

    /// When to use ANSI colors [default: auto — on for terminals, off when
    /// piped; NO_COLOR env also disables]
    #[arg(long, value_enum, default_value = "auto")]
    color: ColorWhen,
}

#[derive(Serialize)]
struct Row {
    repo: PathBuf,
    file: PathBuf,
    format: Format,
    action: plan::Action,
    reason: String,
    warnings: Vec<String>,
    nested: Vec<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pr: Option<gitops::PrOutcome>,
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_default()
}

fn shorten(p: &std::path::Path) -> String {
    let h = home();
    match p.strip_prefix(&h) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => p.display().to_string(),
    }
}

/// Which formats to migrate. CLAUDE.md is always in scope; the rest are
/// opt-in via --migrate / --all-formats / the interactive picker.
fn select_formats(cli: &Cli, found: &[FoundFile]) -> BTreeSet<Format> {
    let mut selected: BTreeSet<Format> = cli.migrate.iter().copied().collect();
    if cli.all_formats {
        selected.extend(Format::OPT_IN.iter().copied());
    }
    selected.insert(Format::Claude);

    // Interactive picker only when something beyond CLAUDE.md exists and the
    // user didn't pre-answer via flags / -y / --json / non-tty.
    let present: BTreeSet<Format> = found
        .iter()
        .filter(|f| f.is_repo_root_file())
        .map(|f| f.format)
        .collect();
    let extra: Vec<Format> = present
        .iter()
        .copied()
        .filter(|f| *f != Format::Claude)
        .collect();
    let interactive = std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && !cli.yes
        && !cli.json
        && cli.migrate.is_empty()
        && !cli.all_formats;
    if interactive && !extra.is_empty() {
        let labels: Vec<String> = std::iter::once(Format::Claude)
            .chain(extra.iter().copied())
            .map(|f| {
                let n = found
                    .iter()
                    .filter(|x| x.format == f && x.is_repo_root_file())
                    .count();
                format!("{} ({} repo{})", f.label(), n, if n == 1 { "" } else { "s" })
            })
            .collect();
        let defaults: Vec<bool> = labels
            .iter()
            .enumerate()
            .map(|(i, _)| i == 0) // only CLAUDE.md pre-checked
            .collect();
        if let Ok(picks) = MultiSelect::new()
            .with_prompt("Instruction files to consolidate into AGENTS.md")
            .items(&labels)
            .defaults(&defaults)
            .interact()
        {
            let all: Vec<Format> = std::iter::once(Format::Claude)
                .chain(extra.iter().copied())
                .collect();
            selected = picks.into_iter().map(|i| all[i]).collect();
        }
    }
    selected
}

/// Group scan hits into per-repo plans + strays (CLAUDE.md outside repos).
fn build_plans(
    found: Vec<FoundFile>,
    selected: &BTreeSet<Format>,
    ignored_agents: &BTreeSet<PathBuf>,
) -> (Vec<Plan>, Vec<PathBuf>) {
    let mut roots: BTreeMap<PathBuf, Vec<FoundFile>> = BTreeMap::new();
    let mut strays = Vec::new();

    for f in found {
        match &f.repo_root {
            Some(r) => roots.entry(r.clone()).or_default().push(f),
            None => strays.push(f.path),
        }
    }

    let home_dir = home();
    let mut plans = Vec::new();
    for (root, files) in roots {
        let nested: Vec<PathBuf> = files
            .iter()
            .filter(|f| !f.is_repo_root_file())
            .map(|f| f.path.clone())
            .collect();
        let mut root_files: Vec<&FoundFile> = files
            .iter()
            .filter(|f| f.is_repo_root_file() && selected.contains(&f.format))
            .collect();
        // CLAUDE.md first (canonical), then other formats by path — so the
        // primary file wins the rename and later sources merge into AGENTS.md.
        root_files.sort_by(|a, b| {
            (a.format != Format::Claude, a.rel()).cmp(&(b.format != Format::Claude, b.rel()))
        });

        if root_files.is_empty() {
            if !nested.is_empty() {
                plans.push(Plan {
                    repo: root.clone(),
                    file: PathBuf::new(),
                    format: Format::Claude,
                    action: Action::Skip,
                    reason: "no root instruction file".into(),
                    warnings: vec![],
                    nested,
                });
            }
            continue;
        }

        let mut agents_touched = false;
        for (i, f) in root_files.iter().enumerate() {
            // $HOME as a repo (bare-dotfiles setups): instruction files there
            // are global-ish — never touch them.
            if root == home_dir {
                plans.push(Plan {
                    repo: root.clone(),
                    file: f.rel(),
                    format: f.format,
                    action: Action::Skip,
                    reason: "repo root is $HOME — treated as global, skipped".into(),
                    warnings: vec![],
                    nested: if i == 0 { nested.clone() } else { vec![] },
                });
                continue;
            }
            match plan::classify(
                &root,
                &f.path,
                f.format,
                if i == 0 { nested.clone() } else { vec![] },
                ignored_agents.contains(&root),
            ) {
                Ok(mut p) => {
                    // Several sources may all want to rename/overwrite into a
                    // not-yet-existing AGENTS.md — only the first may; the
                    // rest must append or earlier content would be clobbered.
                    if agents_touched
                        && matches!(p.action, Action::Rename | Action::OverwriteAgents)
                    {
                        p.action = Action::Merge;
                        p.reason = format!(
                            "AGENTS.md written by an earlier migration — append {}",
                            p.file.display()
                        );
                    }
                    if matches!(
                        p.action,
                        Action::Rename | Action::OverwriteAgents | Action::Merge
                    ) {
                        agents_touched = true;
                    }
                    if p.nested
                        .iter()
                        .any(|n| n.components().any(|c| c.as_os_str() == ".claude"))
                    {
                        p.warnings.push(
                            "repo also has .claude/CLAUDE.md — still read by Claude Code after migration"
                                .into(),
                        );
                    }
                    plans.push(p);
                }
                Err(e) => plans.push(Plan {
                    repo: root.clone(),
                    file: f.rel(),
                    format: f.format,
                    action: Action::Skip,
                    reason: format!("{e:#}"),
                    warnings: vec![],
                    nested: vec![],
                }),
            }
        }
    }
    (plans, strays)
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum ColorWhen {
    Auto,
    Always,
    Never,
}

fn colorize(value: &str, color: &str, enabled: bool) -> String {
    if enabled {
        format!("\x1b[{color}m{value}\x1b[0m")
    } else {
        value.to_owned()
    }
}

/// Fixed-width "✓ label   " badge — every row starts with one so columns
/// line up across plan/stray/variant/ignored lines.
fn badge(mark: &str, label: &str, code: &str, on: bool) -> String {
    colorize(&format!("{mark} {label:<8}"), code, on)
}

/// One table row: badge + path + reason, plus indented detail lines
/// (file:/⚠/nested:) printed under the reason column.
struct TRow {
    mark: &'static str,
    label: String,
    code: &'static str,
    path: String,
    reason: String,
    subs: Vec<(String, bool)>, // (text, is_warning)
}

fn mark_code(label: &str) -> (&'static str, &'static str) {
    match label {
        "skip" => ("!", "1;31"),
        "merge" | "replace" => ("~", "1;33"),
        _ => ("✓", "1;32"),
    }
}

/// Middle-ellipsis for paths longer than the column: keeps the repo name
/// (the end) and the leading ~/ (the start) visible.
fn ellipsize(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_owned();
    }
    let keep = max - 1;
    let head = keep / 2;
    let tail = keep - head;
    let h: String = s.chars().take(head).collect();
    let t: String = s.chars().skip(n - tail).collect();
    format!("{h}…{t}")
}

fn print_table(rows: &[TRow], on: bool) {
    if rows.is_empty() {
        return;
    }
    let w = rows
        .iter()
        .map(|r| r.path.chars().count())
        .max()
        .unwrap_or(4)
        .max(4)
        .min(60);
    println!(
        "{}",
        colorize(&format!("  {:<8} {:<w$} REASON", "ACTION", "PATH"), "1", on)
    );
    for r in rows {
        println!(
            "{} {} {}",
            badge(r.mark, &r.label, r.code, on),
            colorize(&format!("{:<w$}", ellipsize(&r.path, w)), "36", on),
            colorize(&r.reason, "2", on),
        );
        for (sub, warn) in &r.subs {
            let code = if *warn { "33" } else { "2" };
            println!(
                "{}",
                colorize(&format!("{}{}", " ".repeat(12 + w), sub), code, on)
            );
        }
    }
}

fn trow(
    mark: &'static str,
    label: &str,
    code: &'static str,
    path: &std::path::Path,
    reason: &str,
) -> TRow {
    TRow {
        mark,
        label: label.into(),
        code,
        path: shorten(path),
        reason: reason.into(),
        subs: vec![],
    }
}

fn choose_strategy(repo: &std::path::Path, clean: bool) -> Option<gitops::Strategy> {
    let items = if clean {
        vec![
            "worktree  (recommended — your checkout is never touched)",
            "in-place  (branch in this checkout, restored afterwards)",
            "skip this repo",
        ]
    } else {
        vec![
            "worktree  (recommended — dirty tree is never touched)",
            "in-place  (uncommitted files stay untouched, only migration is committed)",
            "skip this repo",
        ]
    };
    let pick = Select::new()
        .with_prompt(format!("{} — strategy?", shorten(repo)))
        .items(&items)
        .default(0)
        .interact()
        .ok()?;
    match pick {
        0 => Some(gitops::Strategy::Worktree),
        1 => Some(gitops::Strategy::InPlace),
        _ => None,
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let root = cli.path.clone().unwrap_or_else(home);
    let root = root.canonicalize().unwrap_or(root);

    let scan = scan::scan(&root, &cli.exclude, cli.follow_links)?;
    let selected = select_formats(&cli, &scan.found);
    let (plans, strays) = build_plans(scan.found, &selected, &scan.ignored_agents);
    let dry = !cli.apply && !cli.pr;

    if cli.json {
        let rows: Vec<Row> = plans
            .iter()
            .map(|p| Row {
                repo: p.repo.clone(),
                file: p.file.clone(),
                format: p.format,
                action: p.action,
                reason: p.reason.clone(),
                warnings: p.warnings.clone(),
                nested: p.nested.clone(),
                pr: None,
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "plans": rows,
                "strays": strays,
                "variants": scan.variants,
                "protected": scan.protected,
                "rule_dirs": scan.rule_dirs,
                "gitignored": scan.gitignored,
                "ignored_agents": scan.ignored_agents,
                "worktrees": scan.worktrees,
                "scan_errors": scan.errors,
            }))?
        );
        return Ok(());
    }

    let color_enabled = match cli.color {
        ColorWhen::Always => true,
        ColorWhen::Never => false,
        ColorWhen::Auto => {
            std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
        }
    };
    let path_c = |p: &std::path::Path| colorize(&format!("{:<44}", shorten(p)), "36", color_enabled);
    let dim = |s: &str| colorize(s, "2", color_enabled);

    println!(
        "{} — {} instruction file{} in repos, {} stray{}",
        colorize(
            if dry { "DRY RUN" } else { "RUN" },
            if dry { "1;36" } else { "1;33" },
            color_enabled
        ),
        colorize(&plans.len().to_string(), "1", color_enabled),
        if plans.len() == 1 { "" } else { "s" },
        colorize(&strays.len().to_string(), "1", color_enabled),
        if strays.len() == 1 { "" } else { "s" }
    );

    let mut rows: Vec<TRow> = Vec::new();
    for p in &plans {
        let (mark, code) = mark_code(p.action.label());
        let mut r = trow(mark, p.action.label(), code, &p.repo, &p.reason);
        let file = p.file.to_string_lossy();
        if file != "CLAUDE.md" && !file.is_empty() {
            r.subs.push((format!("file: {file}"), false));
        }
        for w in &p.warnings {
            r.subs.push((format!("⚠ {w}"), true));
        }
        for n in p.nested.iter().take(5) {
            r.subs.push((format!("nested: {}", shorten(n)), false));
        }
        if p.nested.len() > 5 {
            r.subs.push((format!("… +{} more nested", p.nested.len() - 5), false));
        }
        rows.push(r);
    }
    for s in &strays {
        rows.push(trow("!", "stray", "1;31", s, "not inside a git repo"));
    }
    for v in &scan.variants {
        rows.push(trow(
            "!",
            "variant",
            "1;31",
            v,
            "non-standard spelling — no agent reads this",
        ));
    }
    for d in &scan.rule_dirs {
        rows.push(trow(
            "!",
            "rules",
            "1;31",
            d,
            "tool-specific rule dir — not migratable",
        ));
    }
    if !rows.is_empty() || !scan.errors.is_empty() {
        println!("\n{}", colorize("MIGRATION PLAN & NOTES", "1;36", color_enabled));
        print_table(&rows, color_enabled);
    }
    for e in &scan.errors {
        eprintln!("{}", colorize(&format!("  scan error: {e}"), "31", color_enabled));
    }

    let mut irows: Vec<TRow> = Vec::new();
    for p in &scan.protected {
        irows.push(trow("·", "global", "2", p, "protected"));
    }
    for g in &scan.gitignored {
        irows.push(trow("·", "ignored", "2", g, "gitignored — personal file"));
    }
    for w in &scan.worktrees {
        irows.push(trow(
            "·",
            "worktree",
            "2",
            w,
            "linked worktree — migrate the main checkout",
        ));
    }
    if !irows.is_empty() {
        println!("\n{}", colorize("IGNORED — left alone", "1;36", color_enabled));
        print_table(&irows, color_enabled);
    }

    if dry {
        println!(
            "{}",
            dim(&format!(
                "\ndry run — rerun with --apply (files) or --pr (files + branch + PR){}",
                if strays.is_empty() {
                    ""
                } else {
                    " · --trash-strays removes strays"
                }
            ))
        );
        return Ok(());
    }

    // --apply / --pr — filter actionable plans, then group by repo so all
    // selected files in a repo migrate in a single commit.
    let mut counts = (0usize, 0usize, 0usize); // done, pr'd, skipped
    let mut failures = 0usize;
    let mut actionable: Vec<&Plan> = Vec::new();
    for p in &plans {
        if !p.action.mutates() {
            continue;
        }
        if cli.trivial && !p.action.is_trivial() {
            println!(
                "{} {} {}",
                badge("-", "held", "1;33", color_enabled),
                path_c(&p.repo),
                dim("merge — rerun without --trivial")
            );
            counts.2 += 1;
            continue;
        }
        actionable.push(p);
    }

    if !cli.pr {
        for p in actionable {
            match plan::apply(&p.repo, &p.file, p.action) {
                Ok(()) => {
                    counts.0 += 1;
                    println!(
                        "{} {}{}",
                        badge("✓", p.action.label(), "1;32", color_enabled),
                        colorize(&shorten(&p.repo), "36", color_enabled),
                        dim(&file_suffix(p))
                    );
                }
                Err(e) => {
                    failures += 1;
                    eprintln!(
                        "{} {}",
                        colorize("✗", "1;31", color_enabled),
                        colorize(&format!("{:<44} {e:#}", shorten(&p.repo)), "31", color_enabled)
                    );
                }
            }
        }
    } else {
        let mut by_repo: BTreeMap<PathBuf, Vec<&Plan>> = BTreeMap::new();
        for p in actionable {
            by_repo.entry(p.repo.clone()).or_default().push(p);
        }
        for (repo, repo_plans) in by_repo {
            let clean = gitops::is_clean(&repo);
            let strategy = match cli.strategy.or_else(|| {
                if cli.yes || !std::io::stdout().is_terminal() {
                    Some(gitops::Strategy::Worktree)
                } else {
                    choose_strategy(&repo, clean)
                }
            }) {
                Some(s) => s,
                None => {
                    println!(
                        "{} {}",
                        badge("-", "skipped", "1;33", color_enabled),
                        path_c(&repo)
                    );
                    counts.2 += 1;
                    continue;
                }
            };
            if strategy == gitops::Strategy::InPlace && !clean {
                println!(
                    "{} {} {}",
                    badge("!", "skip", "1;31", color_enabled),
                    path_c(&repo),
                    dim("dirty tree — in-place refused, use worktree")
                );
                counts.2 += 1;
                continue;
            }

            let outcome = gitops::run(&repo, &repo_plans, strategy);
            if outcome.pr_url.is_some() {
                counts.1 += 1;
            } else {
                counts.0 += 1;
            }
            let line = format!(
                "{} {}",
                colorize("✓ migrated", "1;32", color_enabled),
                colorize(&shorten(&repo), "36", color_enabled)
            );
            let mut detail = String::new();
            if outcome.committed {
                detail.push_str(&format!("  → {}", outcome.branch.clone().unwrap_or_default()));
            }
            if outcome.pushed {
                detail.push_str(" pushed");
            }
            if let Some(u) = &outcome.pr_url {
                detail.push_str(&format!("  PR: {u}"));
            }
            println!("{}{}", line, dim(&detail));
            for w in &outcome.warnings {
                println!("{}", colorize(&format!("  ⚠ {w}"), "33", color_enabled));
            }
        }
    }

    // Strays: never migrated (not repos), optionally trashed.
    let mut trashed = 0usize;
    if !strays.is_empty() {
        let trash = cli.trash_strays
            || (!cli.yes
                && std::io::stdout().is_terminal()
                && Confirm::new()
                    .with_prompt(format!(
                        "move {} stray CLAUDE.md (not in git repos) to Trash?",
                        strays.len()
                    ))
                    .default(false)
                    .interact()
                    .unwrap_or(false));
        if trash {
            for s in &strays {
                match trash::delete(s) {
                    Ok(()) => {
                        trashed += 1;
                        println!(
                            "{} {} {}",
                            colorize("🗑 stray", "1;35", color_enabled),
                            colorize(&shorten(s), "36", color_enabled),
                            dim("→ Trash")
                        );
                    }
                    Err(e) => {
                        failures += 1;
                        eprintln!(
                            "{} {}",
                            colorize("✗", "1;31", color_enabled),
                            colorize(
                                &format!("{:<44} trash failed: {e}", shorten(s)),
                                "31",
                                color_enabled
                            )
                        );
                    }
                }
            }
        }
    }

    println!(
        "{}",
        colorize(
            &format!(
                "\ndone: {} migrated, {} PRs, {} skipped{}",
                counts.0,
                counts.1,
                counts.2,
                if trashed > 0 {
                    format!(", {trashed} trashed")
                } else {
                    String::new()
                }
            ),
            if failures == 0 { "1;32" } else { "1;33" },
            color_enabled
        )
    );
    if failures > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// `/filename` suffix shown next to a repo when the migrated file isn't
/// CLAUDE.md itself.
fn file_suffix(p: &Plan) -> String {
    if p.format == Format::Claude {
        String::new()
    } else {
        format!("  ({})", p.file.display())
    }
}

#[cfg(test)]
mod output_tests {
    use super::{badge, colorize};

    #[test]
    fn badge_pads_label_to_fixed_width() {
        assert_eq!(badge("!", "stray", "1;31", false), "! stray   ");
        assert_eq!(badge("✓", "rename", "1;32", false).chars().count(), 10);
    }

    #[test]
    fn colorize_omits_ansi_when_output_is_not_a_terminal() {
        assert_eq!(colorize("✓ rename", "32", false), "✓ rename");
    }

    #[test]
    fn colorize_wraps_terminal_output_in_ansi_color() {
        assert_eq!(colorize("✓ rename", "32", true), "\x1b[32m✓ rename\x1b[0m");
    }
}
