# pacewright

A **wright** (craftsman) of **pace** — a Rust tool that **queues, schedules, and runs browser-automation tasks** with human-like pacing and per-platform daily limits.

pacewright replaces a pile of ad-hoc daemon scripts (and eventually [Postiz](https://postiz.com)) with one background service you drive from a CLI, a live TUI, an MCP server, or a desktop app. It is built to automate social, media, and content platforms through their UIs without tripping bot-detection — the safety comes from driving a real, logged-in browser over CDP and from a scheduler that enforces daily caps, minimum gaps, active-hours windows, and jitter.

> **Status: engine + recipe automation + declarative scheduler + web dashboard complete.** The core engine (queue, scheduler, pacing/limits, runner, durable tracking), the **KDL recipe engine** (declarative browser automation, run as paced tasks via `adapter-recipe`), the **declarative scheduler** (enable-able recurrent tasks in `~/.pacewright/schedules/*.toml`), and a **local web dashboard** (Feed / Schedule / Limits, live over WebSocket) are done and tested (`clippy -D warnings` clean). It also runs **multi-step pipelines with fan-out** over an all-time dedup ledger, non-browser **built-in adapters** (`agent`/`claude_cli`/`http`/`data`), a **JSON datastore** for task output, and signs into a **Claude Max/Pro subscription** for its Claude calls. Driven interchangeably from the CLI, the TUI, or the browser. Real platform adapters ship as recipes.

---

## Why it exists

Automating an authenticated, logged-in session safely is mostly about *not looking like a bot*: consistent fingerprint, a real session, and — critically — **human pacing** (bounded volume, spacing between actions, working hours, randomness). pacewright puts that pacing in the engine, declared per action, so every adapter inherits it for free. See [`docs/specs/`](docs/specs) for the full design and the anti-detection research behind it.

## Architecture

```
                    launchd (keeps it alive)
                           │
                           ▼
             ┌─────────────────────────────┐
             │        pacewrightd           │   one long-running daemon owns:
             │  scheduler + limits + runner │   queue, scheduler, pacing,
             │        SQLite (WAL)          │   runner, SQLite, (later) the
             └──────────────┬──────────────┘   live browser session
                            │ JSON-RPC over ~/.pacewright/pw.sock
      ┌──────────┬──────────┼───────────┬──────────┐
   pacewright  pcw tui   MCP server  Tauri app    (thin clients)
     (CLI)    (dashboard)   (Claude)    (later)
```

The engine is **platform-agnostic**. Adapters implement one trait (`execute(action, params) -> Result`) and *declare* which daily-limit keys each action spends; the engine enforces the limits, persists everything, and never needs to know what any particular platform is. That boundary is what lets the whole engine be tested with a fake adapter and zero browser.

### Workspace

| Crate | Responsibility |
|---|---|
| `pacewright-core` | Task model, SQLite store, injectable `Clock`/`Rng`, limits engine (cap · min-gap · active-hours · jitter), scheduler, runner (retry/backoff/recurrence/deps), engine (boot-recovery + tick) |
| `pacewright-proto` | JSON-RPC request/response wire types shared by daemon + clients |
| `pacewright-daemon` | `pacewrightd` — Unix-socket server + 1s tick loop |
| `pacewright-cli` | `pacewright` (alias `pcw`) — client subcommands + live ratatui TUI |
| `pacewright-mcp` | `pacewright-mcp` — stdio MCP server bridging an MCP client (Claude) to the daemon socket |
| `pacewright-adapter-dummy` | reference `Adapter` (`echo`/`slow`/`flaky`/`always_fail`/`rate_heavy`/`panic`) for testing the engine |
| `pacewright-chrome` | vendored `chrome-agent` (MIT): CDP client + KDL recipe engine (incl. the `solve` captcha step) — driven **in-process** by `pacewright-adapter-recipe`, so the daemon needs no external `chrome-agent` binary |
| `pacewright-browser` | legacy `CliBrowser` `BrowserHandle` (shelled the `chrome-agent` CLI); superseded by in-process `pacewright-chrome`, kept for reference/tests |
| `pacewright-adapter-recipe` | KDL recipe engine glue: `RecipeAdapter`/`RecipeRegistry`, `NativeRecipeRunner` (in-process, attaches to the always-on Chrome) + `NativeLoginLauncher`, the declarative scheduler, and account auth |
| `pacewright-adapter-agent` | Claude adapters: `agent`/`claude` (Messages API) + `claude_cli` (`claude -p`) + the Claude Max/Pro OAuth client + `ClaudeSolver` (Claude-vision captcha solver behind the recipe `solve` step) |

## Build

Rust 1.96+ (this repo pins it via `rust-toolchain.toml`). On a machine where Rust isn't on `PATH`, source the env first:

```bash
source ~/.cargo/env
cargo build --release        # -> target/release/{pacewrightd, pacewright}
```

Or use the Makefile:

```bash
make          # help
make release  # optimized build
make check    # fmt + clippy (-D warnings) + tests
```

## Run

```bash
# 1. start the daemon (creates ~/.pacewright/{pw.sock, pacewright.db})
./target/release/pacewrightd &

# 2. enqueue tasks
pacewright add dummy echo --params '{"hello":"world"}'
pacewright add dummy flaky --params '{"fail_times":2}'   # retries with backoff, then succeeds

# 3. observe
pacewright list --status succeeded
pacewright adapters        # registered adapters + their actions
pacewright status          # queue depth
pacewright tui             # live dashboard (id · adapter · action · status · try · run-at), q to quit
```

### CLI

| Command | What it does |
|---|---|
| `add <adapter> <action> [--params JSON] [--at MS] [--every CRON] [--depends-on ID] [--priority N] [--dedup KEY]` | enqueue a task |
| `list [--status S]` | list tasks |
| `get <id>` | one task + its full event history |
| `cancel <id>` | cancel a pending/deferred task |
| `run-now <id> [--force]` | make a task eligible immediately |
| `pause` / `resume <scope>` | pause/resume the tick loop (`scope` = `all`/`daemon` for global, or an adapter name) |
| `limits` | today's per-key counters |
| `adapters` / `status` | discovery + daemon status |
| `recipe add/list/reload/job` | install recipes from GitHub, list them, hot-reload the daemon, run a vault job note |
| `schedule check/list/apply/enable/disable` | manage the declarative schedule of recurrent tasks |
| `auth status/login/recheck` | establish & inspect the logged-in sessions account recipes need |
| `run <pipeline> --run-id ID [--params JSON] [--retry-failed]` / `runs` / `show <run-id>` | start/resume a pipeline run (idempotent — succeeded steps are never redone), list runs, show one run's steps |
| `data list` / `data show <name> [--limit N]` | inspect the JSON datasets task output is saved into |
| `digest` | today's structured summary — what ran / is queued / failed / is waiting on a human |
| `escalations [--drain]` | read the escalation outbox (terminal failures + auto-paused scopes) |
| `anthropic login [--paste] / status / logout` | sign the daemon into a Claude Max/Pro subscription for Claude calls |
| `tui` | live dashboard (Feed / Schedule / Limits / Accounts panes — `tab` to cycle) |

## Drive it from Claude (MCP)

`pacewright-mcp` is a stdio [MCP](https://modelcontextprotocol.io) server that exposes the daemon's
whole surface as 26 tools (`add_task`, `list_tasks`, `get_task`, `status`, `pause`/`resume`,
`set_limit`, `digest`, `escalations`, `data_list`/`data_show`, `ledger_stats`, `anthropic_status`, the
`schedule_*` and `auth_*` RPCs, `recipe_reload`, …). It bridges each `tools/call`
to `~/.pacewright/pw.sock` — so **the daemon must be running** for tool calls to return data
(handshake and `tools/list` work without it).

Register it with Claude Code:

```bash
claude mcp add pacewright -- /absolute/path/to/target/release/pacewright-mcp
```

or add it to a project/user `.mcp.json`:

```json
{
  "mcpServers": {
    "pacewright": { "command": "/absolute/path/to/target/release/pacewright-mcp" }
  }
}
```

The socket path defaults to `~/.pacewright/pw.sock`; override it with the `PACEWRIGHT_SOCK` env var
(useful for a non-default daemon). A call made while the daemon is down returns a tool error with an
actionable message rather than failing the protocol.

## Teach an agent to drive it (skill)

MCP gives an agent the **tools**; the bundled **skill** gives it the **know-how** — the mental model
(pacing, pipelines, fan-out, the all-time ledger, escalations) and the workflows the raw tool schemas
don't teach. It ships in this repo at [`.claude/skills/pacewright/SKILL.md`](.claude/skills/pacewright/SKILL.md).

For a new user on Claude Code / omp, install it once:

```bash
# user-level (available in every project):
mkdir -p ~/.claude/skills && cp -R .claude/skills/pacewright ~/.claude/skills/
# or project-level: it's already discovered when you work inside a checkout of this repo.
```

With the skill installed and the MCP server registered, the agent picks it up automatically when you
say "pacewright" / "queue a task" / "run a recipe" and drives the daemon for you. To **author** new
recipes, point the agent at [`docs/RECIPES.md`](docs/RECIPES.md) (the KDL grammar); recipes and
schedules themselves are best kept in a **private** repo (`pcw recipe add <owner>/<repo>` installs
them — private repos work through your git auth).

## Configure limits & pacing

`~/.pacewright/config.toml` — every `(platform, action)` that needs throttling gets a limit key. Keys not listed are unrestricted.

```toml
[limits."dummy.capped"]
daily_cap = 3
min_gap   = "8m"          # min spacing between spends of this key
jitter    = 0.5           # ±50% randomization of the gap
active    = "09:00-18:00" # only run inside this local window
```

A task that's over cap, too soon, or outside its window is **deferred** (not dropped) to the next eligible slot — the `run at` column in the TUI shows when.

## Schedule recurrent tasks

Three separated concepts: a **recipe** is *how* to execute (shared, installed via `pcw recipe add`), a **task** is a *specific case* (its params), and a **schedule** is *when* + whether it's enabled. Schedules live in their own files — `~/.pacewright/schedules/*.toml` — so one recipe serves many cases on many cadences, and the schedule never gets baked into a shared recipe.

```toml
# ~/.pacewright/schedules/daily.toml   (see packaging/schedule.example.toml)
[[task]]
id     = "hn-digest"                       # stable id — the reconcile key
recipe = "news/hackernews"                 # the "how"
every  = "0 9 * * *"                       # cron: 09:00 daily  (or `at = "..."`, or neither)
params = { url = "https://news.ycombinator.com/", out_dir = "~/vault/digests" }
```

```bash
pcw schedule check                # validate the files offline (recipes, params, cron)
pcw schedule apply                # reconcile into the queue (the daemon also does this on boot)
pcw schedule list                 # the catalog: id · recipe · when · next-fire · on/off · status
pcw schedule enable hn-digest     # turn a recurrent task on/off at runtime (wins over the file)
pcw schedule disable hn-digest
pcw schedule apply --prune        # also cancel live tasks whose entries were deleted
```

Enabling/disabling is a first-class runtime toggle (persisted, overriding the file's declared default) — the same thing the TUI's **Schedule** pane does with the space bar. The reconciler is desired-state: it queues only the effectively-enabled entries, updates changed ones in place, and (`--prune`) cancels removed ones.

## Built-in adapters, pipelines & the JSON datastore

Beyond browser recipes, the daemon ships **built-in adapters** you can queue or schedule directly, with
no recipe file: `agent/ask` + `agent/adjudicate` (single-turn Anthropic completer, aliased `claude/*`),
`claude_cli/run` (a full headless `claude -p` round — `prompt`/`prompt_file`, `model`, `add_dir[]`,
`cap_secs` — under a daemon-owned wall-clock cap + retry), `pipeline/start` (launch a fresh dated
pipeline run so a scan→act flow recurs on a schedule), `data/append` + `data/read`, and `http/request`
(non-browser REST that injects a secret from an env var at call time, never storing it).

A **pipeline** runs ordered steps under a run id; succeeded steps are never redone, so re-running the
id resumes. A **fan-out** block turns a producer step's array result into paced, deduped, per-item act
tasks, each keyed on an **all-time dedup ledger** (`touched`) so a target is never acted on twice.

```bash
pcw run outreach/matchmaker --run-id 2026-08-10   # start/resume a pipeline run
pcw runs                                          # list runs + step rollups
pcw show 2026-08-10                               # one run's steps, in order
pcw data list                                     # datasets task output was saved into
pcw data show x/pool-ai --limit 20                # rows of one dataset
pcw escalations --drain                           # pull the escalation outbox (failures / paused scopes)
```

Task output is saved as **JSON, not CSV** — per-dataset files at `~/.pacewright/data/<name>.json` (an
array of objects), appended with all-time dedup on a key field.

## Sign in to Claude (Max/Pro subscription)

The agent adapters (`agent/*`, `claude_cli/run`) prefer a signed-in **Claude Max/Pro subscription** over
an API key, so Claude calls draw on the subscription instead of API quota. Sign in once with a local
PKCE OAuth flow to claude.ai; tokens land in `~/.pacewright/secrets.json` (0600) and auto-refresh.

```bash
pcw anthropic login          # opens a browser (`--paste` for a headless paste flow)
pcw anthropic status         # signed-in state + token freshness
pcw anthropic logout         # clear the stored tokens
```

Auth precedence: `ANTHROPIC_OAUTH_TOKEN` env > a stored Max/Pro login (auto-refreshed) > `ANTHROPIC_API_KEY`.

## Log in to authenticated sites

Recipes that touch a signed-in site (a media platform, a content studio, a social network…) reuse a **persistent, per-account browser session** instead of copying a snapshot of your everyday Chrome — which goes stale within minutes as short-lived tokens rotate. A **login is itself a recipe**, living under `~/.pacewright/recipes/accounts/<account>.kdl`:

```kdl
recipe "accounts/prevetted-globex" {
    login-url "https://globex.com/login"        // where you sign in (pacewright opens this headed)
    // The steps ARE the signed-in check, run headless in the account profile. `expect` is a TRIPWIRE
    // (fails when its condition is true), so it trips on the signed-OUT signal: /dashboard bounces to
    // /login when signed out; signed in it stays on /dashboard, so the recipe succeeds.
    step { goto "https://globex.com/dashboard" }
    step { expect on-fail="terminal" message="signed out" { settled-url-matches #"globex\.com/login"# } }
}
```

A normal recipe references its account by evolving the `auth` flag — `auth account="prevetted-globex"` (was `auth #true`). Every recipe sharing an account shares one session, run in the persistent profile `--browser prevetted-globex` with **no cookie copy**, so the site refreshes its own tokens.

```bash
pcw auth status                   # table: account · signed-in/out/unknown · recipes using it · last checked
pcw auth login prevetted-globex   # daemon pops a headed Chrome window — sign in by hand; it goes green
pcw auth login --all              # open a login window for every signed-out account, one at a time
pcw auth recheck [account]        # force a status refresh (runs each account's check headless)
```

The daemon owns the status cache and login orchestration; the CLI, TUI **Accounts** pane (`l` = login, `r` = recheck), and web **Accounts** pane are thin clients. After a login the daemon polls the account's check for up to 5 minutes and flips it to *signed in* automatically. Credentials never pass through pacewright — you sign in yourself in the real window.

## Web dashboard

The daemon also serves a **local web dashboard** — the same control plane as the CLI/TUI, in a browser. It's bound to `127.0.0.1:7878` (localhost only; override with `PACEWRIGHT_WEB_ADDR`, set empty to disable) and runs alongside the socket + tick loop, so the browser, CLI, and TUI all act on the one engine.

```bash
pacewrightd                 # the daemon logs: "pacewright dashboard on http://127.0.0.1:7878"
open http://127.0.0.1:7878  # Feed · Schedule · Limits · Accounts
```

- **Feed** — the live task list (status pills, attempts, last error) with per-row **Run now** / **Cancel** and a global **Pause/Resume all**.
- **Schedule** — the seo-os-style catalog: a **toggle switch** per recurrent task (the same enable/disable as the CLI), recipe, when (every/at/on-apply), next fire, live status, plus **Apply** (`--prune` optional).
- **Limits** — per-key spend (`count` today, last-spent) and an inline editor that issues `set_limit` (daily cap · min gap · jitter · active window).
- **Accounts** — one row per account recipe: a signed-in/out/unknown/logging-in **pill**, the recipes using it, last-checked, with per-row **Log in** / **Recheck** and header **Log in all**. A login pops a headed Chrome on the machine and flips green live when the check passes.

It updates live over a WebSocket (a full snapshot pushed once a second — no polling) and adds no backend logic: every action is the same `proto::Request` the socket takes, funnelled through the one dispatch. A single self-contained HTML page (inlined CSS+JS, no build step), embedded in the binary.

A second **feature dashboard** lives at **`/next`** (`open http://127.0.0.1:7878/next`), surfacing the
newer capabilities the classic panes don't: **Overview** (live counts + Claude subscription status),
**Fleet** (schedule toggles), **Escalations** (the "call Claude on issue" outbox, with **Drain**),
**Datasets** (saved JSON task output, with an inline row view), **Ledger** (per-scope all-time dedup
counts), and **Limits**. Same WebSocket snapshot, same `POST /api` control plane.

## Install as a background service (macOS launchd)

```bash
cp packaging/config.example.toml ~/.pacewright/config.toml
sed "s#__HOME__#$HOME#g" packaging/com.paperclip.pacewrightd.plist > ~/Library/LaunchAgents/com.paperclip.pacewrightd.plist
launchctl load ~/Library/LaunchAgents/com.paperclip.pacewrightd.plist
```

## Roadmap

| Milestone | Scope |
|---|---|
| **M1 ✅** | Core engine + DummyAdapter + CLI + TUI + launchd + recipe engine + declarative scheduler + **web dashboard** |
| M2 | Extend chrome-agent (larger viewport, real CDP input, human mouse movement, `Runtime.enable` audit) + browser handle in `RunCtx` |
| M3 | **profile** adapter (scrape + avatar) — port the profile scraper to Rust |
| M4 | **post / edit-mentions / reply-comments** + **pages** adapter |
| M5 | globex adapter (extract raw, export magic clips, → Spotify, → unlisted upload) |
| M6 | Content-platform upload adapter + daily limits |
| **M7 ✅** | MCP server (`pacewright-mcp`, 20 tools over stdio) + Claude skill (`.claude/skills/pacewright`) |
| M8 | Tauri desktop GUI (Postiz replacement) + migrate off Postiz |

## Known gaps (M1)

Intentionally deferred to later milestones, not oversights:

- **`set_limit` RPC** — limits are set via `config.toml` + restart; no runtime RPC yet.
- **`run_now --force`** — resets `scheduled_for`/clears `next_eligible_at` (re-queues now) but does not yet override an active limit defer.
- **`subscribe` push** — the TUI polls `list` + `status` each second instead.
- **Single-instance guard** is best-effort (`AddrInUse` refusal, no lock file); launchd enforces one instance in practice.

## Design docs

- Recipe authoring (KDL grammar): [`docs/RECIPES.md`](docs/RECIPES.md)
- Spec: [`docs/specs/2026-07-07-core-engine-design.md`](docs/specs/2026-07-07-core-engine-design.md)
- M1 implementation plan: [`docs/plans/2026-07-07-m1-core-engine.md`](docs/plans/2026-07-07-m1-core-engine.md)

## License

MIT
