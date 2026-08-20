---
name: pacewright
description: >
  Drive the pacewright automation daemon — queue, schedule, and monitor paced
  browser-automation tasks (a social network, a media platform, and more) with human-like pacing
  and daily limits. Use when the user says "pacewright", "pcw", "queue a task",
  "check the queue/daemon", "run a recipe", "schedule a recurring task", "pace
  this", "pause/resume the daemon", or asks to render/publish/download through
  the automation daemon.
user-invokable: true
license: AGPL-3.0-only
metadata:
  author: pacewright
  version: "1.0.0"
  category: automation
---

# Driving pacewright

pacewright is a background daemon (`pacewrightd`) that **queues, schedules, and runs
browser-automation tasks with human pacing** (daily caps, min gaps, active-hours, jitter). You drive
it, don't do the automation by hand. A task over its limit is **deferred to the next eligible slot,
never dropped**.

Prefer the **MCP tools** (`pacewright` server, 26 tools) when they're connected. Fall back to the
`pcw` / `pacewright` CLI (they hit the same daemon socket) when MCP isn't available.

## First: is the daemon up?

Every tool call needs a running daemon. Check with the `status` tool (or `pcw status`). If it errors
"daemon call failed / no socket", the daemon is down — tell the user to start it
(`./target/release/pacewrightd &` from the repo, or `launchctl load` if installed) rather than starting
it yourself unless they ask.

## Core moves

- **Queue a task** — `add_task { adapter, action, params?, at?, every?, depends_on?, priority?, dedup?, max_attempts? }`.
  `adapter`/`action` name a recipe (`recipe "<adapter>/<action>"`); `params` is its JSON input.
  `at` = epoch-ms first run, `every` = cron-like recurrence, `dedup` returns an existing active task
  instead of a duplicate. CLI: `pcw add <adapter> <action> --params '{…}' [--at MS] [--every CRON]`.
- **Watch it** — `get_task { id }` (task + full event history), `list_tasks { status?, adapter?, limit? }`,
  `status` (pending/running counts + paused scopes).
- **Discover** — `list_adapters` (adapters + their actions), `limits` (today's per-key counters).
- **Control** — `cancel_task { id }`, `run_now { id }` (make eligible now), `pause`/`resume { scope }`
  (`all`/`daemon` = whole engine, else an adapter name).
- **Pacing** — `set_limit { key, daily_cap?, min_gap?, jitter?, active? }` (`min_gap`/`active` are human
  strings like `8m` / `09:00-17:00`; persisted). Deferred tasks show their next slot in the TUI's `run at`.

## Recipes

Recipes are declarative KDL files in `~/.pacewright/recipes/`, run **in-process** against the
attached Chrome (the vendored `pacewright-chrome` engine — no `chrome-agent` binary). Install with
`pcw recipe add <owner>/<repo>[@ref][#subdir]`, or copy files in. After adding/editing, **`recipe_reload`**
(tool) or `pcw recipe reload` makes them runnable **without restarting the daemon** (`pcw recipe add`
reloads automatically). Caveat: account recipes (`accounts/*`) and a new daemon *binary* still need a
restart.

- **Authoring** — the full KDL grammar (verbs, locators, vars, limit-keys, auth, output) is in
  [`docs/RECIPES.md`](../../../docs/RECIPES.md). Recipe steps cover navigation, `fill`/`insert`/`select`/
  `click`/`upload` (**form fill**), `extract`, `expect` tripwires, `request`/`api` (HTTP), and **`solve`**
  (Claude-vision **captcha** workaround: screenshots the challenge, asks Claude to read it, types the
  answer — the daemon wires the solver; paid by the Max/Pro login).
- **Private recipe repo** — `pcw recipe add` shells `git clone` over HTTPS, so a **private** repo works
  through git's own auth. One-time: `git config --global url."git@github.com:".insteadOf "https://github.com/"`,
  then `pcw recipe add <owner>/pacewright-recipes`. Provenance (repo + pinned SHA) is recorded.
- Example recipes live in the gitignored `recipes/` dir at the repo root (and in `~/.pacewright/recipes/`).

## Built-in adapters (no recipe needed)

These ship with the daemon and are usable straight from `add_task` / schedules / pipelines:

- **`agent/ask`, `agent/adjudicate`** (aliased `claude/*`) — single-turn Anthropic Messages completer.
- **`claude_cli/run`** — a full headless `claude -p` round. Params: `prompt` | `prompt_file`, `model`,
  `add_dir[]`, `cap_secs`. The daemon owns a wall-clock cap + retry-on-fast-fail; it runs on the
  Claude Max/Pro subscription (it strips `ANTHROPIC_API_KEY` from the subprocess).
- **`pipeline/start`** — launch a fresh dated pipeline run (`<run_prefix>-YYYYMMDD`). Params
  `{pipeline, run_prefix?, params?}`. This is how a scan→act pipeline recurs on the schedule.
- **`data/append`** (`{dataset, items, key?}`) and **`data/read`** (`{dataset, limit?, chunk_size?}`) —
  the JSON datastore. `chunk_size` returns `{chunks:[{index,items}]}`, ready to fan out.
- **`http/request`** — non-browser REST. `{method, url, headers?, query?, body?|json?, secret?}` where
  `secret = {env, as}` and `as` = `bearer` | `header:X` | `query:X` | `body:X` (injected from env at
  call time, never stored). limit-key `http.request`.

The schedule validator accepts these built-ins (`dummy`/`agent`/`claude`/`claude_cli`/`pipeline`/`data`/`http`).

## Pipelines & fan-out

A **pipeline** is a run of ordered steps under a run id; succeeded steps are never redone (resume by
re-using the id). Drive it with `pcw run <pipeline> --run-id <id> [--params JSON] [--retry-failed]`,
list runs with `pcw runs`, inspect one with `pcw show <run-id>`.

**Fan-out** turns a producer step's array result into paced, deduped, per-item act tasks:

    fanout after=<step> recipe=<a/a> items=<path> as=<var> scope=<s> id=<tmpl> { params { … } }

On the producer's success the runner materializes one act task per item, each keyed on the ledger id,
spending the act recipe's `limit-key` for cap/gap, and marking the **ledger** on its own success.

## Ledger (never act twice)

An all-time dedup ledger (`touched(scope,target_id,…)`) records every target a fan-out act touched.
Once marked, that target is never re-acted, across all time. Inspect it with the `ledger_stats` tool.

## Datasets — save output as JSON, not CSV

Task output lands in per-dataset JSON files at `~/.pacewright/data/<name>.json` (an array of objects),
appended with all-time dedup on a key field. Read them: `pcw data list`, `pcw data show <name> [--limit N]`
(tools `data_list` / `data_show`).

## Escalations — "call Claude when there's an issue"

On a terminal task failure or an auto-paused scope, the daemon writes an escalation to
`~/.pacewright/escalations/*.json` (with a repair hint) and, if `PACEWRIGHT_CLAUDE_NOTIFY=1`, spawns
`claude -p`. Read the outbox with `pcw escalations [--drain]` (tool `escalations`); it also surfaces in
the `digest`'s `waiting_on_human`. Triage with `get_task` + `resume`.

## Claude Max/Pro subscription (for the agent adapters)

`agent/*` and `claude_cli/run` prefer a signed-in Claude Max/Pro subscription over an API key. Sign in
once: `pcw anthropic login [--paste]` (PKCE OAuth to claude.ai; tokens auto-refresh). Check/clear with
`pcw anthropic status` / `pcw anthropic logout` (tool `anthropic_status`). Auth precedence:
`ANTHROPIC_OAUTH_TOKEN` env > stored Max login (auto-refreshed) > `ANTHROPIC_API_KEY`.

## Built-in adapters (no recipe needed)

These ship with the daemon and are usable straight from `add_task` / schedules / pipelines:

- **`agent/ask`, `agent/adjudicate`** (aliased `claude/*`) — single-turn Anthropic Messages completer.
- **`claude_cli/run`** — a full headless `claude -p` round. Params: `prompt` | `prompt_file`, `model`,
  `add_dir[]`, `cap_secs`. The daemon owns a wall-clock cap + retry-on-fast-fail; it runs on the
  Claude Max/Pro subscription (it strips `ANTHROPIC_API_KEY` from the subprocess).
- **`pipeline/start`** — launch a fresh dated pipeline run (`<run_prefix>-YYYYMMDD`). Params
  `{pipeline, run_prefix?, params?}`. This is how a scan→act pipeline recurs on the schedule.
- **`data/append`** (`{dataset, items, key?}`) and **`data/read`** (`{dataset, limit?, chunk_size?}`) —
  the JSON datastore. `chunk_size` returns `{chunks:[{index,items}]}`, ready to fan out.
- **`http/request`** — non-browser REST. `{method, url, headers?, query?, body?|json?, secret?}` where
  `secret = {env, as}` and `as` = `bearer` | `header:X` | `query:X` | `body:X` (injected from env at
  call time, never stored). limit-key `http.request`.

The schedule validator accepts these built-ins (`dummy`/`agent`/`claude`/`claude_cli`/`pipeline`/`data`/`http`).

## Pipelines & fan-out

A **pipeline** is a run of ordered steps under a run id; succeeded steps are never redone (resume by
re-using the id). Drive it with `pcw run <pipeline> --run-id <id> [--params JSON] [--retry-failed]`,
list runs with `pcw runs`, inspect one with `pcw show <run-id>`.

**Fan-out** turns a producer step's array result into paced, deduped, per-item act tasks:

    fanout after=<step> recipe=<a/a> items=<path> as=<var> scope=<s> id=<tmpl> { params { … } }

On the producer's success the runner materializes one act task per item, each keyed on the ledger id,
spending the act recipe's `limit-key` for cap/gap, and marking the **ledger** on its own success.

## Ledger (never act twice)

An all-time dedup ledger (`touched(scope,target_id,…)`) records every target a fan-out act touched.
Once marked, that target is never re-acted, across all time. Inspect it with the `ledger_stats` tool.

## Datasets — save output as JSON, not CSV

Task output lands in per-dataset JSON files at `~/.pacewright/data/<name>.json` (an array of objects),
appended with all-time dedup on a key field. Read them: `pcw data list`, `pcw data show <name> [--limit N]`
(tools `data_list` / `data_show`).

## Escalations — "call Claude when there's an issue"

On a terminal task failure or an auto-paused scope, the daemon writes an escalation to
`~/.pacewright/escalations/*.json` (with a repair hint) and, if `PACEWRIGHT_CLAUDE_NOTIFY=1`, spawns
`claude -p`. Read the outbox with `pcw escalations [--drain]` (tool `escalations`); it also surfaces in
the `digest`'s `waiting_on_human`. Triage with `get_task` + `resume`.

## Claude Max/Pro subscription (for the agent adapters)

`agent/*` and `claude_cli/run` prefer a signed-in Claude Max/Pro subscription over an API key. Sign in
once: `pcw anthropic login [--paste]` (PKCE OAuth to claude.ai; tokens auto-refresh). Check/clear with
`pcw anthropic status` / `pcw anthropic logout` (tool `anthropic_status`). Auth precedence:
`ANTHROPIC_OAUTH_TOKEN` env > stored Max login (auto-refreshed) > `ANTHROPIC_API_KEY`.

## Accounts / auth

Recipes with `auth account="…"` need a signed-in browser session first:

- `auth_list` — each account's signed-in / out / unknown status and which recipes use it.
- `auth_login { account }` — navigates the account's tab in the **attached** always-on Chrome to the
  recipe's login URL and raises the window (in-process via `NativeLoginLauncher` — it no longer
  *launches* a browser, and there's no `chrome-agent` binary). A human signs in by hand.
  `auth_login_all` opens one per signed-out account.
- `auth_recheck { account? }` — re-run the headless signed-in check and refresh the cached status.

Never enter the user's credentials yourself — `auth_login` is human-in-the-loop by design.

**Google accounts:** because pacewright now attaches to your real, everyday always-on Chrome (not a
CDP-*launched* throwaway), the old "Google blocks a launched browser" trap is mostly gone — the raised
tab is your normal Chrome. Still sign in by hand in that window; if Google flags the automated session,
finish the sign-in in the same Chrome outside pacewright, then `auth_recheck`.

## Scheduling (recurrent tasks)

Declarative schedule files live in `~/.pacewright/schedules/*.toml` (a recipe = *how*, a schedule =
*when* + on/off). `schedule_list`, `schedule_apply { prune? }`, `schedule_enable { id }`,
`schedule_disable { id }`. `pcw schedule check` validates the files offline.

## Guidance

- Report task outcomes faithfully — if a task is `deferred`/`failed`, say so and show `last_error` from
  `get_task`, don't claim success.
- Side-effectful automation (publishing, sending, purchasing) still needs the user's explicit go-ahead
  before you queue it — queuing it *is* scheduling the side effect.
- The socket defaults to `~/.pacewright/pw.sock`; `PACEWRIGHT_SOCK` overrides it.
