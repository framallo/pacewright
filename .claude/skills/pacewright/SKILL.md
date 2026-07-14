---
name: pacewright
description: >
  Drive the pacewright automation daemon — queue, schedule, and monitor paced
  browser-automation tasks (LinkedIn, Riverside, YouTube) with human-like pacing
  and daily limits. Use when the user says "pacewright", "pcw", "queue a task",
  "check the queue/daemon", "run a recipe", "schedule a recurring task", "pace
  this", "pause/resume the daemon", or asks to render/publish/download through
  the automation daemon.
user-invokable: true
license: MIT
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

Prefer the **MCP tools** (`pacewright` server, 20 tools) when they're connected. Fall back to the
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

Recipes are KDL files in `~/.pacewright/recipes/` (installed via `pcw recipe add owner/repo[#subdir]`,
or copied in). After adding/editing recipe files, **`recipe_reload`** (tool) or `pcw recipe reload`
makes them runnable **without restarting the daemon** (`pcw recipe add` reloads automatically).
Caveat: account recipes (`accounts/*`) and a new daemon *binary* still need a restart. Example recipes
live in the gitignored `recipes/` dir at the repo root (and in the runtime `~/.pacewright/recipes/`).

## Accounts / auth

Recipes with `auth account="…"` need a signed-in browser session first:

- `auth_list` — each account's signed-in / out / unknown status and which recipes use it.
- `auth_login { account }` — pops a **headed** Chrome (chrome-agent-launched) and a human signs in by
  hand. `auth_login_all` opens one window per signed-out account.
- `auth_recheck { account? }` — re-run the headless signed-in check and refresh the cached status.

Never enter the user's credentials yourself — `auth_login` is human-in-the-loop by design.

**⚠ Google/YouTube accounts can't use `auth_login`.** `auth_login` launches a chrome-agent/CDP
automation browser, and **Google blocks sign-in on it** ("this browser or app may not be secure").
Riverside tolerates it; Google does not. So for a YouTube Studio / Google account (e.g.
`prevetted-youtube`), do NOT run `auth_login` / `pcw auth login` — it just gets stuck at "logging in…".
Instead use the CDP-**attach** workaround: launch a *normal* Chrome with a dedicated `--user-data-dir`
+ `--remote-debugging-port=9222`, have the user sign in by hand, then drive it with
`chrome-agent --connect auto` (attach, not launch). This path is outside pacewright's `auth` system.

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
