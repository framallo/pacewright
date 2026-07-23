# Pipeline runs and verified steps

*Status: proposed, 2026-07-22. Supersedes the orchestration half of `RIVERSIDE_PIPELINE_PLAN.md`.*

## Goal

Run a whole multi-step publishing pipeline as one durable, resumable, paced unit, and return
the real output URLs, without pacewright learning anything about podcasts.

Driving case: given a Riverside project id, title, description, publish date, and episode number,
produce the YouTube long URL, the shorts URLs, and the Spotify URL, having confirmed each against
the platform rather than against the browser recipe's own report.

## Constraints

1. **The engine stays generic.** No podcast, Riverside, or YouTube concepts in `core`. Per-step
   logic lives in recipes. A pipeline is a declaration that references recipes by name.
2. **Runs are automatic.** No approval gates; a run does not pause for a human.
3. **Every step is validated before the next one starts.** A step is not done because it returned;
   it is done because an independent check says the platform agrees.
4. **Resume is by completion.** Re-running a pipeline must not redo a step that already succeeded.
5. **Humanized pacing between steps**, and **retry on failure**.

## Why this is needed

`depends_on` (`crates/core/src/scheduler.rs:12`) is pure status gating: a child runs when the parent
reaches `Succeeded` and receives nothing from it. Tasks persist a `result`, but nothing feeds a
parent's result into a child's params. So a chain cannot pass a video id forward, and cannot report
URLs at the end.

Worse, `Succeeded` is not trustworthy today. Two recorded incidents: `riverside/share_spotify`
reported success while leaving a blank Spotify draft, and `riverside/publish_clips` reported success
while publishing zero shorts. A dependency chain gated on `Succeeded` cascades straight through a
silent no-op and produces a confident, wrong report. Validation is therefore not a final step; it is
the edge between steps.

## Model

A **run** is a group of ordinary tasks sharing a `run_id`. Each task carries a `step_name`. Existing
`depends_on` wires them. Nothing about scheduling, limits, pacing, retry, or recovery changes: the
scheduler and runner operate on tasks exactly as they do now.

Two new columns on `tasks`:

```sql
run_id    TEXT,
step_name TEXT
CREATE UNIQUE INDEX idx_tasks_run_step ON tasks(run_id, step_name) WHERE run_id IS NOT NULL;
```

### P1. Result references in params

Before dispatch, the runner resolves references in a task's `params`:

```
{{ steps.<step_name>.result.<json.path> }}
{{ vars.<name> }}
```

against sibling tasks in the same `run_id` (their stored `result`) and the run's variables.
Resolution is plain JSON-path substitution and is adapter-agnostic. An unresolvable reference fails
the task with a clear error rather than sending a literal `{{ … }}` to a browser.

Recipes already return a JSON object keyed by eval-step name, so `{{ steps.publish_long.result.publish.video_id }}`
addresses the existing shape without changing the recipe engine.

### P2. Verified steps

A step declares up to three recipe references. All three are ordinary adapter invocations, which is
what keeps the engine generic:

| Slot | Purpose | On success | On failure |
|---|---|---|---|
| `recipe` | do the thing | run `verify` | retry per `attempts`, then fail the run |
| `verify` | independently confirm it | mark step `Succeeded`, release dependents | run `fallback` |
| `fallback` | adjudicate a failing verify | see below | fail the step, retry per `attempts` |

A step **is not `Succeeded` until its `verify` passes.** This is the core change. Dependents release
on verified completion, never on "the recipe returned".

A step with no `verify` is allowed, and is recorded as `unverified` in the run report so it is
obvious which parts of a green run were never actually checked.

### P3. The fallback adjudicator, and its guardrail

`fallback` exists because a verify can itself be wrong (an API lags, a check is too strict). It is a
generic adapter invocation; in practice `claude/adjudicate`, which runs Claude headless with the
step's result, the verify's failure, and whatever evidence the pipeline passes it.

Because an adjudicator that is asked "did this work?" will drift toward yes, and that is precisely
the failure mode this whole spec exists to prevent:

- The adjudicator prompt is **adversarial**: it must find concrete evidence that the action
  succeeded, and returns `ok: false` when uncertain. Absence of evidence is failure, not success.
- It must return structured `{ ok, reason, evidence }`, and `evidence` must be non-empty to pass.
- A step passed this way is recorded `adjudicated`, never `verified`. The run report distinguishes
  them, and a run containing any `adjudicated` step is flagged.
- The adjudicator can only overturn a **verify** failure. It can never overturn a **step** failure,
  and it can never invent a result value used by a later step.

### P4. Resume

`dedup_key = "<run_id>:<step_name>"`. Starting a run that already exists recreates only steps that
are missing or in a non-terminal/failed state; any step whose task is `Succeeded` is skipped, its
stored result still addressable by later steps.

`Store::find_active_by_dedup` (`crates/core/src/store.rs:164`) only matches active tasks, so resume
needs a lookup that also sees terminal ones. That is the whole change.

Consequence: `pacewright run podcast/episode --run-id ep172 …` is idempotent. Re-running after a
mid-pipeline failure continues from the failed step and re-does nothing that worked.

### P5. Pacing and retry

A pipeline declares `pace min= max=`. When a step becomes eligible, its `next_eligible_at` is set to
now plus a jittered delay in that window, drawn from the existing `Rng`. Per-step `attempts` maps
onto the existing `max_attempts` and backoff. No second pacing concept is introduced; per-recipe
`limit-key` caps still apply on top.

## Declaration format

A new KDL document type, living alongside recipes. Generic: it names steps, deps, params, verifies.

```kdl
pipeline "podcast/episode" {
    description "Riverside project -> published episode with verified URLs"
    pace min="90s" max="5m"

    var "project_id"     required=#true
    var "episode_number" required=#true
    var "title"          required=#true
    var "description"    default=""
    var "publish_date"   default=""

    step "render_clips" recipe="riverside/render_clips" {
        params { project_id "{{ vars.project_id }}" }
        attempts 3
        verify recipe="riverside/verify_exports" {
            params { project_id "{{ vars.project_id }}" min_clips "1" }
        }
    }

    step "publish_long" recipe="riverside/share_youtube" after="render_clips" {
        params {
            project_id "{{ vars.project_id }}"
            title      "EP{{ vars.episode_number }} {{ vars.title }}"
            do_publish "true"
        }
        attempts 2
        verify recipe="youtube/verify_video" {
            params {
                video_id       "{{ steps.publish_long.result.publish.video_id }}"
                expect_privacy "unlisted"
                expect_title   "EP{{ vars.episode_number }} {{ vars.title }}"
            }
        }
        fallback recipe="claude/adjudicate" {
            params {
                question "Is the EP{{ vars.episode_number }} long video actually on the PreVetted channel as Unlisted?"
                context  "{{ steps.publish_long.result }}"
            }
        }
    }

    output {
        youtube_url "{{ steps.publish_long.verify.result.url }}"
        shorts_urls "{{ steps.publish_shorts.verify.result.urls }}"
        spotify_url "{{ steps.share_spotify.verify.result.url }}"
    }
}
```

Every podcast specific, which export tile is the real edit, which modal the Share button opens, what
"Magic episode 01" means, stays inside the referenced recipes, untouched by this work.

## CLI

```
pacewright run <pipeline> --run-id <id> --params '{…}'   # start or resume; idempotent
pacewright runs                                          # list runs and their status
pacewright show <run-id>                                 # per-step status + verified/adjudicated + outputs
pacewright cancel-run <run-id>
```

`show` prints, per step: status, attempts, whether it was `verified` / `adjudicated` / `unverified`,
and the resolved `output` block once the run completes.

## Verify recipes needed

These are ordinary recipes/adapters, built as part of this work but not part of the engine. They hit
platform APIs, not the DOM, so they are independent of what the browser recipe believed:

| Verify | Checks |
|---|---|
| `youtube/verify_video` | video id exists, privacy status, title, on the expected channel |
| `youtube/verify_shorts` | each expected short exists and is Unlisted; count matches |
| `spotify/verify_episode` | episode exists, has non-empty description and art, correct publish date |
| `riverside/verify_exports` | the expected export tiles exist and are not still exporting |

YouTube already has OAuth wired (`pacewright oauth`), so `youtube/verify_*` can be an API adapter
action rather than a browser recipe.

## Build order

1. **Engine.** `run_id`/`step_name` columns, result-reference resolution, verified-step semantics,
   dedup resume, paced eligibility, `run`/`runs`/`show` CLI. Testable end to end with
   `adapter-dummy`, no browser.
2. **Verify recipes**, starting with `youtube/verify_video` and `youtube/verify_shorts`. Run them
   against recent episodes immediately: they will show whether past "succeeded" runs actually worked.
3. **The `podcast/episode` pipeline**, wiring the recipes that already exist.
4. **Harden `riverside/share_spotify`**, the one step still unverified, now with a real verify behind it.

Phase 2 has value on its own even if the rest slips: it answers "is the current state of the channel
what we think it is."

## Testing

- Engine work is covered with `adapter-dummy`: result references, verify-gates-dependents,
  fallback-overturns-verify-only, resume-skips-succeeded, pacing sets `next_eligible_at`.
- A fake failing verify must block dependents; this is the regression test for the two silent no-ops.
- Adjudicator guardrail: a fallback returning `ok: true` with empty `evidence` must NOT pass.

## Risks

- **The adjudicator becomes a rubber stamp.** Mitigated by the adversarial prompt, the mandatory
  evidence field, and the `adjudicated` marking. Watch the ratio of adjudicated to verified steps.
- **Verify recipes drift** from what the platform actually returns, turning green into meaningless.
  Mitigated by having them hit APIs rather than the DOM.
- **A long run holds a browser session** for a while. Pacing between steps mitigates; renders are
  already polled via `retry-if-positive` rather than blocking.
