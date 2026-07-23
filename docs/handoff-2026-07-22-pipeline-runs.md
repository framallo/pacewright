---
type: handoff
date: 2026-07-22
topic: pacewright pipeline runs + verified steps
---

# Handoff: pacewright pipeline runs

Goal set by Federico: one command takes a Riverside project id, title, description, publish date and
episode number, runs the whole podcast publishing pipeline, and returns the YouTube URL, the shorts
URLs and the Spotify URL, **having confirmed each against the platform**.

**Phase 1 (engine) is complete and tested. Phase 2 (verify recipes) is 1 of 4 done.**

## Read this first: why verification is the whole point

Two recorded incidents drive every design decision here:

- `riverside/publish_clips` reported **success while publishing zero shorts**.
- `riverside/share_spotify` reported **success while leaving a blank Spotify draft**.

So `Succeeded` from a browser recipe is not evidence. The engine therefore treats a step as complete
only when an **independent** check says the platform agrees. If you find yourself making something
pass more easily, you are re-creating the bug.

## Two repos changed

| Repo | Commits | Branch state |
|---|---|---|
| `~/work/pacewright` | 13 (`31d1c89` → `ddda2ff`) | clean, 18 test targets green, clippy clean |
| `~/work/chrome-agent` | 2 (`bf810d4`, `003526b`) | clean; `extract_tests` has 2-5 **flaky live-Chrome** failures that predate this work |

Neither repo is pushed. `pacewright` had 49 unpushed commits before this session and now has more.

## What Phase 1 built (pacewright)

A **run** is a group of ordinary tasks sharing `run_id`, each tagged `step_name`, wired with the
existing `depends_on`. The scheduler, limits, runner and recovery are otherwise untouched.

- **Verify gate, with zero runner changes.** A step declaring `verify` expands into **two** tasks
  (`<step>` and `<step>.verify`), and dependents depend on the **verify**. "Not done until confirmed"
  falls out of the existing dependency scheduler. This is the single most important idea in the design.
- **Result passing.** `{{ steps.<step>.result.<path> }}` and `{{ vars.<name> }}` resolve at *dispatch*
  time. An unresolvable reference **fails** the task rather than handing a literal `{{ … }}` to a browser.
- **Resume.** `dedup_key = "<run_id>:<step_name>"`. Re-running recreates only missing or failed steps;
  anything already `Succeeded` is skipped. `--retry-failed` re-queues just the failures.
- **Pacing.** `pace min= max=` gives each released task a jittered `next_eligible_at`.
- **Adjudication.** A `fallback` is released by a **failure edge** (`dep_on_failure`) and may overturn
  a failing verify — but only with `ok:true` **and** non-empty `evidence`, only a *verify* (never a real
  action's failure), and the step is then marked `adjudicated`, never `verified`.
- **Run dataset.** `~/.pacewright/runs/<run_id>.json` is written before each dispatch and its path is
  passed in as the `dataset` var, so recipes (a separate process) can see the run. It is a
  **regenerable projection**; the task rows stay the source of truth.
- **CLI.** `pacewright run <pipeline> --run-id <id> --params '{…}'` (start *or* resume), `runs`, `show`.

Pipelines live in `~/.pacewright/recipes/pipelines/<name>.kdl` (`/` flattened to `-`). The shipped
`podcast/episode` is in `packaging/pipelines/` with a test that parses it.

## What Phase 2 still needs

`youtube/verify_video` is written and **validates** (`chrome-agent recipe check` → ok). It is the
template; the rest follow it exactly.

1. `youtube/verify_shorts` — count recent uploads at the expected privacy, assert `>= min_count`.
   This is the check that would have caught the zero-shorts incident.
2. `spotify/verify_episode` — episode exists, description non-empty, art present.
3. `riverside/verify_exports` — expected export tiles exist and are not still exporting.
4. `claude/adjudicate` — the adversarial verdict adapter the two `fallback` blocks reference.
5. **Restart the daemon.** The running one dates from **Jul 19**, predates everything here, and
   answers `no_adapter` for every recipe task. Then `pacewright recipe reload`.
6. First live run: `pacewright run podcast/episode --run-id ep172 --params '{…}'`.

## Traps already paid for. Do not re-learn these.

- **No domain adapters in `crates/`.** I built `crates/adapter-youtube` in Rust; Federico rejected it,
  correctly. pacewright stays generic; per-step logic lives in **recipes**. It has been deleted.
- **`__pw` is the locator runtime, not a variables bag.** `__pw.vars` does not exist. I shipped a
  recipe built on that assumption and deleted it. An `eval` step also needs a **page**, which defeats
  the token-only design, hence the `value` condition added to chrome-agent.
- **One `capture` per `api` step.** `ApiRequest` holds a single `capture_key`; a second `capture`
  child **silently overwrites** the first. `youtube/verify_video` therefore does one GET per field
  (1 quota unit each). Multi-capture on `ApiRequest` is a clean generic follow-up.
- **KDL needs one node per line.** On a single line, a following node is absorbed as an *entry* of the
  previous one. `params { got "x" label "y" }` silently drops `label`. This cost two debugging cycles.
- **A missing `capture` path yields `null`, it does not fail the step.** That is why the assertions,
  not `expect-status`, are the real check. YouTube answers **200 with empty `items`** for an unknown id.
- **`std::env::set_var` in tests destabilizes parallel runs.** `run::home_dir()` is read by callers,
  never inside helpers, for this reason.

## chrome-agent changes (both generic, no platform knowledge)

- `bf810d4` — `capture_response` indexes arrays by **numeric path segment**, so
  `items.0.status.privacyStatus` resolves. It previously captured `null` always.
- `003526b` — a **`value` condition** for `expect`, so a browser-less recipe can assert:

```kdl
step {
    expect on-fail="terminal" message="nothing was published" {
        value "{{ found }}" not-equals="0"
    }
}
```
Supports `equals` / `not-equals` / `non-empty`, requires at least one, and touches no browser method
so it works under `NativeBrowser`.

## Open questions

- **Does pacewright inject `token` for a `youtube`-provider recipe** the way it does for LinkedIn?
  The secret is imported and the recipe declares `var "token" required=#true`, but the
  provider-to-recipe mapping was never inspected. **Check this before the first live run.**
- **Does chrome-agent have a *read* step for a JSON file?** The `output` sink covers writing, so the
  run dataset can be contributed to, but a recipe reading it is unconfirmed.
- `output` block resolution into one final run report (parsed and stored; `show` prints per-step
  results, so URLs are visible, just not collapsed).
- Multi-dependency steps: `Task.depends_on` is a single `Option<String>`; `expand` **rejects**
  `after=` with more than one entry rather than silently honouring the first.

## Credentials

YouTube OAuth **is** configured now. The token from `~/.youtube-cli-token-prevetted-channel.json`
was imported into `~/.pacewright/secrets.json` (scope `…/auth/youtube`, refresh token present, shows
`token expired` which is fine — it refreshes on use). Backup: `secrets.json.bak-20260722`, mode 600.

## Key files

- Spec: `docs/specs/2026-07-22-pipeline-runs-and-verified-steps.md` (+ 2 addenda: the run dataset, and
  the assert/capture blockers)
- Plan: `docs/superpowers/plans/2026-07-22-pipeline-runs-phase1.md`
- Engine: `crates/core/src/{run,pipeline,refs}.rs`; gate logic in `run::expand`
- Scheduler edges: `crates/core/src/scheduler.rs` (`dep_on_failure`, the escalation race fix)
- Adjudication: `crates/core/src/runner.rs` (evidence guardrail)
- Pipeline: `packaging/pipelines/podcast-episode.kdl`
- Recipe template: `~/.pacewright/recipes/youtube-verify-video.kdl`

## A caution about the first live run

`podcast/episode` publishes to YouTube and Spotify. Its verify steps reference recipes that do not
exist yet, so **until they are written the run will fail at its first verify — by design.** Do not
"fix" that by removing the verify blocks. A step with no `verify` is recorded `unverified`, which is
honest; a step whose verify was deleted to make the run go green is the original bug wearing a hat.
