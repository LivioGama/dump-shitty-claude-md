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

fn print_row(repo: &str, file: &str, label: &str, reason: &str, warnings: &[String], nested: &[PathBuf]) {
    let mark = match label {
        "skip" => "!",
        "merge" | "replace" => "~",
        _ => "✓",
    };
    println!("{mark} {label:<8} {repo:<44} {reason}");
    if file != "CLAUDE.md" && !file.is_empty() {
        println!("  {:<10} {:<44} file: {file}", "", "");
    }
    for w in warnings {
        println!("  {:<10} {:<44} ⚠ {w}", "", "");
    }
    for n in nested.iter().take(5) {
        println!("  {:<10} {:<44} nested: {}", "", "", shorten(n));
    }
    if nested.len() > 5 {
        println!("  {:<10} {:<44} … +{} more nested", "", "", nested.len() - 5);
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
                "scan_errors": scan.errors,
            }))?
        );
        return Ok(());
    }

    println!(
        "{} — {} instruction file{} in repos, {} stray{}",
        if dry { "DRY RUN" } else { "RUN" },
        plans.len(),
        if plans.len() == 1 { "" } else { "s" },
        strays.len(),
        if strays.len() == 1 { "" } else { "s" }
    );

    for p in &plans {
        print_row(
            &shorten(&p.repo),
            &p.file.to_string_lossy(),
            p.action.label(),
            &p.reason,
            &p.warnings,
            &p.nested,
        );
    }
    for s in &strays {
        println!("! stray    {:<44} not inside a git repo", shorten(s));
    }
    for v in &scan.variants {
        println!("! variant  {:<44} non-standard spelling — no agent reads this", shorten(v));
    }
    for p in &scan.protected {
        println!("· global   {:<44} protected — left alone", shorten(p));
    }
    for g in &scan.gitignored {
        println!("· ignored  {:<44} gitignored — personal file, left alone", shorten(g));
    }
    for d in &scan.rule_dirs {
        println!("! rules    {:<44} tool-specific rule dir — not migratable", shorten(d));
    }
    for e in &scan.errors {
        eprintln!("  scan error: {e}");
    }

    if dry {
        println!(
            "\ndry run — rerun with --apply (files) or --pr (files + branch + PR){}",
            if strays.is_empty() {
                ""
            } else {
                " · --trash-strays removes strays"
            }
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
            println!("- held     {:<44} merge — rerun without --trivial", shorten(&p.repo));
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
                    println!("✓ {:<8} {}{}", p.action.label(), shorten(&p.repo), file_suffix(p));
                }
                Err(e) => {
                    failures += 1;
                    eprintln!("✗ {:<44} {e:#}", shorten(&p.repo));
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
                    println!("- skipped  {}", shorten(&repo));
                    counts.2 += 1;
                    continue;
                }
            };
            if strategy == gitops::Strategy::InPlace && !clean {
                println!(
                    "! {:<44} dirty tree — in-place refused, use worktree",
                    shorten(&repo)
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
            let mut line = format!("✓ migrated {}", shorten(&repo));
            if outcome.committed {
                line.push_str(&format!("  → {}", outcome.branch.clone().unwrap_or_default()));
            }
            if outcome.pushed {
                line.push_str(" pushed");
            }
            if let Some(u) = &outcome.pr_url {
                line.push_str(&format!("  PR: {u}"));
            }
            println!("{line}");
            for w in &outcome.warnings {
                println!("  ⚠ {w}");
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
                        println!("🗑 stray    {} → Trash", shorten(s));
                    }
                    Err(e) => {
                        failures += 1;
                        eprintln!("✗ {:<44} trash failed: {e}", shorten(s));
                    }
                }
            }
        }
    }

    println!(
        "\ndone: {} migrated, {} PRs, {} skipped{}",
        counts.0,
        counts.1,
        counts.2,
        if trashed > 0 {
            format!(", {trashed} trashed")
        } else {
            String::new()
        }
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
