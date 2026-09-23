# 🗑️ dump-shitty-claude-md

> **One `AGENTS.md` to rule them all — consolidate every agent instruction file in your home directory.**

```bash
bunx dump-shitty-claude-md
```

```
DRY RUN — 290 CLAUDE.md in repos, 25 strays
MIGRATION PLAN & NOTES
  ACTION   PATH                           REASON
✓ dedupe   ~/code/api-server              identical content → delete CLAUDE.md
~ merge    ~/code/dashboard               content differs → append CLAUDE.md into AGENTS.md
✓ rename   ~/code/cli-tool                no AGENTS.md — rename
! skip     ~/code/dotfiles                CLAUDE.md is a symlink — left alone
! stray    ~/Desktop/playground/CLAUDE.md not inside a git repo
IGNORED — left alone
  ACTION   PATH                           REASON
· global   ~/CLAUDE.md                    protected
```

Output is colored in terminals (`--color always|auto|never`, `NO_COLOR` honored); ignored files are grouped separately.

## 🧠 The decision matrix

Every file is classified before anything is touched:

| `CLAUDE.md` | `AGENTS.md` | Action |
|---|---|---|
| exists | absent | rename → `AGENTS.md` |
| exists | identical / whitespace-equal (BOM/CRLF-tolerant) | delete `CLAUDE.md` |
| exists | superset of `CLAUDE.md` | delete `CLAUDE.md` |
| exists | subset of `CLAUDE.md` | `CLAUDE.md` replaces `AGENTS.md` |
| exists | partial overlap | append with a `<!-- merged from CLAUDE.md -->` marker |
| symlink | any | skip + report |
| bare repo / FIFO / socket | — | skip + report |
| outside a git repo | — | never touched (Trash is opt-in) |
| inside a linked worktree | — | ignored — the main checkout gets migrated |
| `~/CLAUDE.md`, `~/.claude/**` | — | protected, never touched |

Also warns on: `@import` syntax, `CLAUDE.local.md`, `AGENTS.local.md`,
`.claude/CLAUDE.md`, non-UTF8, hardlinks, nested files, dead spellings
(`claude.md`, `AGENT.md`).

## 🧩 Supported formats

`CLAUDE.md` is always in scope. Everything else is **off by default** — pick
in the interactive prompt or pass `--migrate` / `--all-formats`:

| File | Tool |
|---|---|
| `CLAUDE.md` | Claude Code — ✅ default |
| `.github/copilot-instructions.md` | GitHub Copilot |
| `GEMINI.md` | Gemini CLI — only when `.gemini/settings.json` allows `AGENTS.md` |
| `.cursorrules` `.windsurfrules` `.clinerules` `.roorules` `.kilocoderules` `.rules` | Cursor, Windsurf, Cline, Roo, Kilo, Zed (legacy files) |
| `.goosehints` | Goose |
| `WARP.md` | Warp |
| `CONVENTIONS.md` | Aider |
| `.cursor/rules`, `.claude/rules`, `.github/instructions`, `.kiro/steering`, `.trae/rules`, `.openhands`, `.junie`, `.amazonq`, … | ❌ report-only — glob/frontmatter semantics `AGENTS.md` can't express |

Multiple files in one repo merge in a single pass — `CLAUDE.md` wins the
rename, the rest append with provenance markers.

## 🚩 Flags

| Flag | Effect |
|---|---|
| *(none)* | dry run — print the plan, touch nothing |
| `--apply` | migrate files in place |
| `--pr` | migrate on a branch + push + open PR/MR (`gh` / `glab`) |
| `--migrate <fmt>` | also migrate formats (`cursor,copilot,gemini,…`) |
| `--all-formats` | migrate every supported format |
| `-y` | non-interactive, safe defaults (CLAUDE.md only, worktree) |
| `--trivial` | lossless actions only — merges report-only |
| `--trash-strays` | non-repo strays → OS Trash (Put Back-able) |
| `--json` | machine-readable plan |
| `PATH`, `--exclude`, `--follow-links` | scan control |

## 🛡️ Safety model

- **Dry run first** — nothing mutates without `--apply` / `--pr`.
- **No data loss** — content always lands in `AGENTS.md`; committed files recoverable via git.
- **Protected zones** — `~/CLAUDE.md`, `~/.claude/**`, bare repos, symlinks, strays.
- **Idempotent** — re-runs converge, no double merges.
- **Headless-safe** — `GIT_TERMINAL_PROMPT=0`, ssh `BatchMode`: no prompt can hang it.

## Install

`bunx` is the way — no install. The npm package ships prebuilt binaries for
macOS arm64/x64, Linux x64/arm64 and Windows x64, all cross-compiled and
published by the GitHub release workflow on every version tag:

```bash
bunx dump-shitty-claude-md           # dry run
bunx dump-shitty-claude-md --apply   # migrate
bunx dump-shitty-claude-md --pr      # migrate via PRs
```

Alternatives: `npm i -g dump-shitty-claude-md` · `cargo install dump-shitty-claude-md`

## 📝 License

MIT
