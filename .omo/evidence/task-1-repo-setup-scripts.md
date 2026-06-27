# Task 1: Safety Baseline & Dirty-Worktree Guard — repo-setup-scripts

**Date:** 2026-06-27
**Plan:** `repo-setup-scripts`
**Goal:** Record the pre-execution dirty state and establish a guard against touching unrelated files.

---

## 1. Git Status (`git status --short`)

```
 M .omo/boulder.json
?? .claude/
?? .omo/drafts/
?? .omo/plans/repo-setup-scripts.md
?? .omo/run-continuation/ses_0f7fedb43ffdOXyzW9Y8VNS9hV.json
?? .omo/run-continuation/ses_0f7ff0cd1ffeubN0PWobf3UpSL.json
?? .omo/run-continuation/ses_0f8096666ffeCy9KPuD3xNBRVj.json
?? .omo/run-continuation/ses_0f8096683ffeoNRJ3tjmcZosPP.json
?? .omo/run-continuation/ses_0f80a07e9ffec7O9qUJA413LQY.json
?? .omo/run-continuation/ses_0f89e05c3ffeNNMnuIj8G62pem.json
?? AGENTS.md
?? CLAUDE.md
```

## 2. Out-of-Scope Untracked Paths (MUST NOT TOUCH)

These files/directories are unrelated to the repo-setup-scripts plan and must remain untouched:

| Path | Reason |
|---|---|
| `.claude/` | Opencode/Claude configuration directory |
| `AGENTS.md` | Agent instructions file |
| `CLAUDE.md` | Claude project instructions file |
| `.omo/run-continuation/*.json` | Run-continuation state files |
| `.omo/drafts/` | Draft workspace files |
| `.omo/boulder.json` | Boulder state (modified, not staged) — do not commit |

## 3. `.env` Ignore Confirmation

**`.gitignore` lines 8–11:**
```
# Environment and secrets
.env
.env.*
!.env.example
```

- `.env` → **ignored** (`git check-ignore .env` returns `.env`)
- `.env.*` → **ignored** (confirmed: `.env.local`, `.env.production` also ignored)
- `.env.example` → **NOT ignored** (negation rule `!.env.example`)

This means the plan can safely create/modify `.env.example` for documentation purposes without it being git-ignored.

## 4. Scope Boundaries

No product Rust source files will be edited. All edits are strictly limited to:

| File | Purpose |
|---|---|
| `setup.sh` | Unix interactive setup script |
| `setup.bat` | Windows interactive setup script |
| `README.md` | Update quick-start to reference setup scripts |
| `docs/DOCKER-QUICKSTART.md` | New dedicated Docker quick-start guide |
| `.omo/evidence/*` | Plan evidence files (this file and subsequent tasks) |

## 5. Pre-existing Evidence Files (preserved)

- `.omo/evidence/task-1-git-status.txt`
- `.omo/evidence/task-1-impact-baseline.md`
- `.omo/evidence/task-8-verification.txt`

These are from a previous session — left untouched.

## 6. Verification

- [x] `git status --short` captured and recorded
- [x] Out-of-scope untracked paths explicitly listed
- [x] `.env` confirmed git-ignored via `git check-ignore`
- [x] `.gitignore:8-11` inspected and documented
- [x] Scope limited to setup scripts + docs only (no Rust edits)
- [x] No product files modified by this evidence task
