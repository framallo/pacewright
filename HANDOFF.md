# pacewright — handoff

## ⛔ START HERE — non-browser jobs + JSON datastore + Claude Max/Pro OAuth (2026-08-10)

Follow-on to the outreach fleet migration below. The engine gained a JSON datastore and non-browser
REST/data adapters, the jobs that were deferred there (x-harvest, plus new podcast sourcing/hooks) are
now declarative pipelines, and the agent adapters run on the Claude Max/Pro subscription. **245+ tests
pass, clippy `-D warnings` clean.** Not committed.

### JSON datastore + data/http adapters
- **JSON datastore (`core::datastore`)** — per-dataset JSON files at `~/.pacewright/data/<name>.json`
  (an array of objects), appended with all-time dedup on a key field. "Save data as JSON, not CSV" —
  the result-sink the CSV-accumulating crawls needed, minus the CSV. Read via RPCs `DataList`/`DataShow`,
  CLI `pcw data list` / `pcw data show <name> [--limit N]`, MCP `data_list`/`data_show`.
- **`data/append`** (`{dataset, items: array|object, key?}`) and **`data/read`**
  (`{dataset, limit?, chunk_size?}`) — `chunk_size` returns `{chunks:[{index,items}]}`, ready to fan out.
- **`http/request`** — non-browser REST (`{method, url, headers, query, body|json, secret={env,as}}`
  where `as` = `bearer` | `header:X` | `query:X` | `body:X`), injecting a secret from env at call time
  (never stored). limit-key `http.request`.
- `schedule::partition` now also accepts the `data` + `http` built-ins (full set:
  `dummy`/`agent`/`claude`/`claude_cli`/`pipeline`/`data`/`http`).

### The non-browser jobs (finished the fleet)
- **x-harvest** — now migrated (supersedes the "NOT migrated" note in the section below):
  `outreach/x-harvest` pipeline (recipe `x/harvest` → `data/append` into the `x/pool-ai` + `x/pool-lib`
  JSON datasets, deduped).
- **podcast sourcing** — `outreach/podcast-sourcing` pipeline (`http/request` Apollo REST →
  `data/append` `podcast/hosts`; needs `APOLLO_API_KEY`; disabled by default).
- **podcast hooks (R13)** — `outreach/podcast-hooks` pipeline (`data/read` chunk → fanout → `claude_cli`).

### Claude Max/Pro OAuth — agent adapters on the subscription
- **`crates/adapter-agent/src/anthropic_oauth.rs`** — PKCE login to claude.ai, token exchange/refresh
  against `api.anthropic.com/v1/oauth/token`, SecretStore-backed with auto-refresh at call time. The
  `AnthropicCompleter` uses the subscription (Bearer + `anthropic-beta: oauth-2025-04-20` + a Claude
  Code system-identity block) before falling back to `ANTHROPIC_API_KEY`. `claude_cli/run` strips
  `ANTHROPIC_API_KEY` from its subprocess so it uses the Max/Pro login too.
- **Auth precedence:** `ANTHROPIC_OAUTH_TOKEN` env > stored Max login (auto-refreshed) > `ANTHROPIC_API_KEY`.
- CLI `pcw anthropic login [--paste] | status | logout` (a local OAuth flow — browser + secret store,
  no daemon socket). RPCs `AnthropicStatus` + `LedgerStats` feed the dashboard.
- MCP tools added this session: `escalations`, `data_list`, `data_show`, `ledger_stats`, `anthropic_status`
  (26 tools total).

### Cutover
`packaging/cutover-outreach.sh` (reversible, `--undo`) now verifies the daemon **and** `pcw anthropic
status`, applies the schedules, and retires the 6 migrated launchd plists.

## ⛔ START HERE — outreach fleet migration (2026-08-10)

Built to `channels/pacewright-outreach-requirements-2026-08-10.md`. The engine gained the pieces the
shell fleet needed, and the jobs are now declarative pacewright schedules. **233 tests pass, clippy
`-D warnings` clean.** Not committed.

### New engine capabilities (all tested)
- **All-time dedup ledger (R7)** — `touched(scope,target_id,first_touched_at)` in `core::store`
  (`is_touched`/`mark_touched`/`touched_count`). Replaces `mm-pitched-all.log` / `.apollo-revealed.txt`.
- **Fan-out (R5)** — a pipeline `fanout after=<step> recipe=… items=… as=… scope=… id=… { params }`
  block. On the producer step's success the runner turns its array `result` into one paced, deduped
  act task per item (`core::run::materialize_fanout`), each keyed on the ledger id
  (`dedup_key="touch:<scope>:<id>"`), spending the act recipe's `limit-key` for cap/gap, and marking
  the ledger on its own success. `Task` gained `fanout`/`touch_scope`/`touch_id`.
- **Escalation notifier — "call Claude when there is an issue"** — `core::notify::{Notifier,
  EscalationEvent}` (+`NullNotifier`/`RecordingNotifier`). The runner escalates on terminal failure /
  auto-paused scope. Daemon impl `daemon::notify::OutboxNotifier` writes
  `~/.pacewright/escalations/*.json` (+ optional `claude -p` when `PACEWRIGHT_CLAUDE_NOTIFY=1`). Read
  it via `pcw escalations [--drain]`, the MCP `escalations` tool, or the `digest`'s `waiting_on_human`.
- **`claude_cli` adapter (R1 + R4)** — `claude_cli/run` shells a real `claude -p` (prompt/`prompt_file`,
  `model`, `add_dir[]`, `cap_secs`) with a daemon-owned wall-clock cap and retry-on-fast-fail. This is
  the "headless-claude round" the LinkedIn/X/book scripts hand-rolled with `caffeinate`/watchdog; now
  it's a capped, escalating pacewright task. (`agent/ask` stays the single-turn API completer.)
- **`pipeline` launcher** — built-in `pipeline/start` starts a fresh dated run (`<prefix>-YYYYMMDD`) so
  a scan→act pipeline recurs on the declarative schedule. `schedule::partition` now accepts the
  built-in adapters (`dummy`/`agent`/`claude`/`claude_cli`/`pipeline`).

### The migrated fleet (deployed to `~/.pacewright`, canonical copies in `packaging/`)
`schedules/outreach.toml` + `config.toml` (fleet caps) + `recipes/matchmaker-scan.kdl` +
`recipes/pipelines/outreach-matchmaker.kdl`:
- **matchmaker** → deterministic `outreach/matchmaker` pipeline (scan 5 categories → fan-out
  `matchmaker/pitch`, cap 100/day, ledger dedup). The R5 showcase.
- **linkedin-comment / x-engage / book-promo** → `claude_cli/run` schedules (the scripts already were
  `claude -p` rounds; now daemon-capped + escalating instead of 8 launchd agents).
- **linkedin-invitations** → scheduled `linkedin/invitations` recipe.
- **x-harvest** → NOT migrated (deferred): a CSV-accumulating crawl needing a recipe result-sink
  pacewright doesn't have; lowest spec priority; still runs as its launchd crawl.

### Cutover (NOT done — your go; it starts real outreach)
`packaging/cutover-outreach.sh` is reversible (`--undo`). It verifies the new daemon is up,
`schedule apply`s, then unloads + archives the 6 migrated launchd plists (keeps `pacewright-chrome`,
`caddy`, `paperclipai.server`, `linkedin-post-daily`, `x-harvest`). Run it deliberately once you
restart the daemon on the new binary. Verified live in an isolated daemon: `pipeline/start`→dated run,
terminal failure→escalation outbox+`pcw escalations`+digest; deterministic tests cover fan-out+ledger.
**Live browser/`claude -p` runs were NOT exercised here** (need the logged-in Chrome + live sites).


Last updated: 2026-07-17 (**single attached Chrome, one tab per site** — Phases 1–5 done + auth
gate PASSED + recipes de-generated; uncommitted). Written for the next agent.

## ⛔ START HERE — single attached Chrome (2026-07-16, IN PROGRESS, UNCOMMITTED)

Plan + full findings: **`docs/plans/2026-07-16-single-chrome-attach.md`**. Read it before touching
the browser layer — the section below is only the resume state.

**What changed.** pacewright no longer lets chrome-agent *launch* a browser. It **attaches**
(`--connect`) to one always-on, non-headless Google Chrome, and separates sites by named **tabs**
instead of per-account browser profiles. Launching was the root cause of the auth bugs: a
CDP-launched browser is a bot signal, so a social network walled the profile and revoked `li_at`, and Google
refused sign-in outright.

- account → `--page <account>` (was `--browser <account>`), one shared `--browser pacewright`
- `--headed` and `--copy-cookies` **deleted** — they only existed to make a launched throwaway
  Chromium resemble a real signed-in Chrome
- `auth login` no longer launches; it `--activate`s the account's tab for the human to sign in
- browser reaper **deleted** (it was also silently broken — see plan, Phase 5)
- `[browser] idle_timeout` → `[browser] connect` in `config.toml` (old key ignored, not fatal)
- new `foreground #true` recipe flag → `--activate`; on `globex/render_clips` only
- `RunOpts { account, foreground }` replaces `run(.., auth, account)` — `auth` was dead

**✅ Recipes are now static (was: generated).** Per Federico's directive, the globex recipes are
hand-maintained static `.kdl` files parameterized by `{{ url }}`/`{{ guest }}`, no generator. Root
cause of the old generation: a compiled chrome-agent export verb that no longer
exists (`recipe check` → unknown step verb), so the JS was inlined. That inlined form is canonical
now. Headers rewritten, repo staging synced (it had held the dead-verb form), generator retired to
`gen_recipes.py.RETIRED` + README. All six globex recipes pass `chrome-agent recipe check`.
⚠️ The external **awesome-recipes** repo (the real distribution channel per `.gitignore`) is not
reachable from here and still needs the same static versions, or `pcw recipe add` reinstalls the
dead verb form.

**No foreground mutex — deliberately.** The dispatch loop already awaits each task before claiming
the next, so tasks never overlap and cannot contend for the one foreground tab. That property is
pinned by `foreground_serialization_is_load_bearing` (`daemon/tests/e2e.rs`), which fails loudly if
anyone parallelizes the loop. Read the failure as: `foreground` now needs real serialization.

**State:** 178 tests green, clippy clean. **Nothing is committed.** The working tree already held
unrelated in-flight work (Cargo.lock, `crates/cli/*`, `registry.rs` — OAuth-ish) before this
started, so committing would sweep that up. Separate the two before committing.

**The Chrome is installed and running.** `packaging/com.paperclip.pacewright-chrome.plist` is
loaded as a LaunchAgent (port 9222, profile `~/.pacewright/chrome-profile`). KeepAlive revival and
attach self-heal across restarts are both verified live.

**✅ AUTH GATE PASSED (2026-07-17).** Federico signed into Google + a social network by hand; driven through
`--connect`, the social network survived a profile load with `li_at`
intact (feed reload after still signed in). Google accepts sign-in on a `--remote-debugging-port`
Chrome, and attach does NOT burn the social network — the two things the whole refactor bet on. The media
platform was not re-checked this session but is the same attach path.

**✅ Stale page-target auto-recovery (2026-07-17).** Fixed: `core::browser::is_stale_page_target`
+ `prune_stale_page`, wired into both `CliBrowser::run` and `CliRecipeRunner::run` — on a stale
target they prune that one page from `sessions.json` and retry once, so chrome-agent opens a fresh
tab. Bounded to a single retry. Pure logic unit-tested (`CHROME_AGENT_HOME`-scoped temp store);
wiring proven live by closing a tab mid-flight and watching `CliBrowser` recover on its own. A
closed tab is no longer a silent task failure.

**Next, in order:** (1) push the static globex recipes to the external awesome-recipes repo (not
reachable from here); (2) handle the stale-page-target error class in the daemon (recreate + retry
once); (3) `pcw chrome status` (must NOT trust `sessions.json`'s `headless` field — it reads `true`
for attached sessions); (4) finish Phase 6 — retire the three launched profiles from
`sessions.json` (move aside, don't delete), drop the "Google can't use auth_login" caveat from the
pacewright skill; (5) untangle + commit (see below).

---

## Recipe auth & login (2026-07-10) — SUPERSEDED IN PART

⚠️ The **browser model** below (per-account launched profiles, `--headed`, `--copy-cookies`, page
`main`) is superseded by the 2026-07-16 plan above. The rest — account recipes as KDL, the auth
status cache, `pcw auth`, the Accounts panes — still stands.

## Recipe auth & login — account recipes + persistent sessions (2026-07-10)

Running `globex/generate_magic_clips` through pacewright 403'd: `chrome-agent --copy-cookies`
copies a **static snapshot** of the everyday Chrome profile, and the media platform's access token has a
**~9.5-minute TTL** — the snapshot is stale by the time the recipe runs (Google: signed out
entirely). Fix: recipes reuse a **durable, self-refreshing per-account session**, and the operator
can **establish + inspect** those sessions. Spec: `docs/specs/2026-07-10-recipe-auth-login.md`.

**The model — a login IS a recipe.** Login procedures live under
`~/.pacewright/recipes/accounts/<account>.kdl` (account name = file stem). An account recipe is a
**normal recipe whose steps ARE the signed-in check**, plus a flat `login-url` node. A normal recipe
references its account by evolving the `auth` flag: `auth account="prevetted-globex"` (was `auth
#true`). Recipes sharing an account share one session. chrome-agent needs **no changes** — `auth
account=…` and `login-url` are unknown nodes it already ignores; account recipes run as ordinary
recipes; `--headed` + persistent `--browser <name>` already exist.

- **`crates/adapter-recipe/src/registry.rs`** — `RecipeMeta` gains `account: Option<String>` (from
  `auth account="…"`) + `login_url` (account recipes). `is_account()` = name prefix `accounts/`;
  `accounts()` / `account(name)` / `recipes_for_account(name)`. `adapters()` **excludes** accounts,
  so an account recipe is never a runnable task adapter (routed to auth instead).
- **`crates/adapter-recipe/src/runner.rs`** — `RecipeRunner::run(path, vars, auth, account)`. When
  `account=Some(x)`, run in that **persistent profile** (`--browser x`, **no `--copy-cookies`**) — the
  staleness fix. `auth #true` (no account) keeps the shared `pacewright` profile + copy.
- **`crates/adapter-recipe/src/auth.rs`** (NEW) — `AuthManager` owns the per-account **status cache**
  + login orchestration. `recheck` runs the account recipe headless in its profile (`Ok`→signed-in,
  `Terminal`→signed-out, `Retryable`→unknown). `login` opens a **headed** window via the
  `LoginLauncher` seam (`CliLoginLauncher` = `chrome-agent --browser <account> --headed goto
  <login-url>`) and marks `logging_in`; `poll_until_signed_in` rechecks (4s × 75 ≈ 5 min) then clears.
- **`proto` + `daemon/server.rs`** — four RPCs: `AuthList` (cached, cheap — rides the 1s web
  snapshot), `AuthRecheck{account?}`, `AuthLogin{account}`, `AuthLoginAll`. Handled in an **async**
  `handle_auth` BEFORE the engine-locked synchronous dispatch (they spawn subprocesses + detached
  polls). `Server` gains `auth: Arc<AuthManager>`; `main.rs` builds it from the registry + runner +
  `CliLoginLauncher`. `web.rs::snapshot()` includes `accounts`.
- **Clients (thin):** `pcw auth status/login [--all]/recheck` (`cli/src/auth_cmd.rs`, friendly
  table/messages); web **Accounts** pane (4th tab, Log in / Recheck / Log in all); TUI **Accounts**
  pane (`tab` cycles Feed/Schedule/Limits/Accounts, `l`=login `r`=recheck on the selected row).

**Verified live:** an `accounts/prevetted-globex` recipe + a `globex/generate_magic_clips`
referencing it → daemon registered only the `globex` task adapter (account excluded) → `pcw auth
status` printed the account, `unknown`/never, with the recipe using it → `pcw auth login nope` errored
cleanly, then a real interactive login on `prevetted-globex` flipped it to **signed in** and the
session persisted across a daemon restart.

**Two gotchas the live run caught (do not relearn):**
1. **`expect` is a TRIPWIRE** — it FAILS when its condition is TRUE (see `chrome-agent`
   `src/recipe/engine.rs`: `if condition_holds → return Err(on_fail)`). A signed-in check must trip on
   the signed-OUT signal (`settled-url-matches #"globex\.com/login"#`), NOT assert the signed-in URL.
   My first account recipe had it inverted (tripped on `/dashboard`), so signed-in read as signed-out.
2. **Regex in a locator/URL match must be a KDL raw string** (`#"globex\.com/login"#`) — a plain
   `"…\.…"` is an invalid KDL escape and the whole recipe fails to load.

Deferred (spec §6): password-manager fill (`login-field`) — the account profile is an isolated
Chromium profile that does NOT inherit the everyday Chrome's saved passwords, so login is interactive
once (then it persists); credentials never pass through pacewright.

## Web dashboard — the control plane in a browser (2026-07-09)

The scheduler's control plane, now also in a browser. Spec: `docs/specs/2026-07-09-web-dashboard.md`.

- **`crates/daemon/src/web.rs`** — an axum server bound to `127.0.0.1:7878` (override
  `PACEWRIGHT_WEB_ADDR`, empty disables), spawned by `main.rs` alongside the socket + tick loop,
  sharing the one `Arc<Server>`. Adds **no** backend logic:
  - `POST /api` — a `proto::Request` in, a `proto::Response` out, straight through the existing
    `server::handle_request` (the exact socket dispatch). This is the whole control plane.
  - `GET /ws` — pushes a full `snapshot()` (`status`+`tasks`+`schedules`+`limits`, each from the
    read RPCs) once a second. The page is pure WS-consumer + `POST /api`; no polling, no change-hub.
  - `GET /` — one self-contained `web/index.html` (inlined CSS+JS, no build), `include_str!`'d in.
- **Panes** mirror the TUI: **Feed** (run-now/cancel/pause-all), **Schedule** (toggle switches =
  enable/disable, apply+prune — the centerpiece), **Limits** (spend + inline `set_limit` editor).
- Test helpers moved to `server::test_support` (shared by `server.rs` + `web.rs` tests). Verified
  live: all three panes render over WS, and enable/disable/set_limit/pause round-trip through `/api`.
- **Gotcha:** the page is compiled in via `include_str!` — edit `web/index.html` then **rebuild** the
  daemon. (An early `nav`-as-implicit-global `ReferenceError` silently killed WS init; watch the
  browser console when the conn dot stays red.)

## Recipe cookie-auth is per-recipe now — the `auth` flag (2026-07-09)

Proving the HN digest live surfaced a real defect: `CliRecipeRunner` passed `--copy-cookies` on
**every** run, so a public no-auth recipe failed (`Chrome cookies file not found`). Now a recipe
declares `auth #true` in its `recipe { … }` block when it needs the operator's logged-in session;
`RecipeRegistry` parses it into `RecipeMeta.auth`, the adapter threads it to
`RecipeRunner::run(path, vars, auth)`, and `--copy-cookies` is added only then. Public recipes
(`news/hackernews`) omit it and navigate cold. chrome-agent ignores the unknown `auth` node
(forward-compatible) — no chrome-agent change. `CliRecipeRunner.copy_cookies` stays as a global
force-on override (default off).

**Live proof:** `news/hackernews` on a schedule → daemon boot-reconcile → tick → chrome-agent →
**live** Hacker News → `hn.json` + `hn-digest.md` in the vault → task `succeeded`. Note: Chrome
launch needs a real `$HOME` (its `~/Library` app-support/crashpad state); a throwaway `HOME=/tmp/…`
hangs Chrome launch — unrelated to pacewright. Run the daemon under the real `$HOME`.

## Declarative scheduler — enable-able recurrent tasks (2026-07-09)

The seo-os-style model: a **catalog of recurrent tasks you toggle on/off**. Spec:
`docs/specs/2026-07-09-declarative-scheduler.md`.

- **recipe = how (shared) · task = which params (your case) · schedule = when (yours).** The
  schedule is NOT in the recipe file — it's `~/.pacewright/schedules/*.toml`, a list of `[[task]]`
  entries (`id`, `recipe`, `params`, `every`/`at`, `enabled`). See `packaging/schedule.example.toml`.
- **`crates/adapter-recipe/src/schedule.rs`** — parse (`load_dir`/`parse_file`), `validate`/
  `partition` (against the `RecipeRegistry`), and `reconcile` (desired-state: file → queue, keyed
  on `dedup:schedule:<id>`, queues only effectively-enabled entries, `--prune` cancels removed).
  Lives in `adapter-recipe` (not `core`) because validation needs the registry.
- **Enable/disable is a runtime toggle**, persisted in `core`'s new `schedule_state` table (wins
  over the file default) — same pattern as `set_limit` → `limit_overrides`. Both tables + a fix to
  preserve `dedup_key` across recurrence firings are in `crates/core/src/store.rs`/`runner.rs`.
- **RPCs** (`proto` + `daemon/server.rs`, now behind a `Server{engine,registry,schedules_dir}` ctx):
  `ScheduleApply{prune}`, `ScheduleList`, `ScheduleEnable/Disable{id}`, `SetLimit{key,config}`.
  The daemon reconciles on boot and merges `limit_overrides` over `config.toml`.
- **CLI**: `pcw schedule check` (offline) / `list`/`apply [--prune]`/`enable`/`disable`.
- **TUI**: three panes — **Feed / Schedule / Limits**, `tab` cycles, in Schedule `↑/↓` select,
  `space` enable/disable, `a` apply.

The **web dashboard** consuming these RPCs is now built (see the top section). Still deferred
(documented): the **daemon jobs-sweep**/`schedules/` file-watch (reconcile is boot + explicit
`apply` + on-toggle), and the automated Claude repair loop.

## Recipe engine — the big shift (2026-07-09)

Browser automation is moving from **hand-written Rust adapters** to **declarative KDL
recipes** executed by the forked `chrome-agent` (`~/work/chrome-agent`, branch
`feat/recipe-engine`, held local — do NOT push / open the upstream PR without Federico's
go-ahead). A recipe is inert data: `goto`/`extract`/`expect`/`wait`/`screenshot`, write
verbs (`click`/`fill`/`select`/`upload`), and cookie-authenticated `request` (in-page
`fetch` with `credentials:'include'`), with Playwright-style locators resolved by a
shipped `locators.js`. Spec: `docs/specs/2026-07-08-recipe-engine-in-chrome-agent.md`.

pacewright consumes it via `crates/adapter-recipe`:
- **`RecipeRegistry`** enumerates `~/.pacewright/recipes/*.kdl` (populated by `pcw recipe
  add`) and reads each recipe's routing/pacing metadata (name → `(adapter, action)`,
  `limit-key`s, `var`s with `from` aliases) via a shallow KDL walk.
- **`RecipeAdapter`** — one per `<adapter>` prefix — routes an `(adapter, action)` to its
  recipe and runs it via **`CliRecipeRunner`** (`chrome-agent recipe run <file> --vars-json
  …`, whole recipe in one process so `locators.js` persists; pinned to the `pacewright`
  browser/page). The child's `[Terminal]/[Retryable]/[RateLimited]` tag is recovered into
  the pacewright error class. Pacing is unchanged (recipe declares `limit-key`s).
- **Vault job-runner** — `pcw recipe job <note.md>` reads a note's YAML frontmatter
  (`recipe: acme/scrape_profile` + var fields), binds frontmatter→vars (honoring `from`
  aliases + `vault`/`out_dir`/`slug`/`note` context), and enqueues a **paced** task deduped
  on the note path. The recipe's `output` blocks write the md/JSON note back into the vault.

To run the acme testbed end to end: `pcw recipe add <repo>` (or drop the testbed
`.kdl` under `~/.pacewright/recipes/`), then `pcw add acme scrape_profile --params
'{"url":"…"}'` or `pcw recipe job <note>`. Needs the `chrome-agent` binary from the fork on
PATH (build it in `~/work/chrome-agent`).

Deferred follow-ups: the **daemon jobs sweep** (a configured glob of job notes swept on the
tick, vs. today's one-shot `pcw recipe job`) and the **automated Claude repair loop**
(subsystem E — the `--repair` context bundle already ships; closing the loop does not).

## What this is

`pacewright` is a Rust workspace: a browser task queue/scheduler/runner that replaces
a pile of ad-hoc daemon scripts (and eventually Postiz). One background daemon
(`pacewrightd`) owns a durable SQLite-backed queue, a scheduler, a pacing/limits
engine, and a runner that drives per-platform **adapters**. You talk to it from a CLI
(`pacewright` / `pcw`), a live TUI, and later an MCP server + Tauri GUI.

Full context: `README.md` (architecture, CLI, config, roadmap). Design spec:
`docs/specs/2026-07-07-core-engine-design.md`. M1 build plan:
`docs/plans/2026-07-07-m1-core-engine.md`.

## Repo layout

7 crates under `crates/`:
- `core` — engine, scheduler, runner, store (SQLite), limits, config, clock, rng, adapter trait,
  `BrowserHandle` trait + test doubles. The heart. Stays browser-free (trait only, no impl).
- `proto` — JSON-RPC wire types shared by daemon + clients.
- `daemon` — `pacewrightd`: Unix-socket JSON-RPC server + 1s tick loop.
- `cli` — `pacewright`/`pcw`: client subcommands (incl. `recipe add/list/job`) + ratatui TUI.
- `adapter-dummy` — reference `Adapter` (`echo`/`slow`/`flaky`/`always_fail`/`rate_heavy`/`panic`) for testing without a browser.
- `browser` — `CliBrowser`: the real `BrowserHandle`, drives the `chrome-agent` CLI (per-verb).
- `adapter-recipe` — `RecipeAdapter` + `RecipeRegistry` + `RecipeRunner`: runs declarative KDL
  recipes as paced tasks by shelling `chrome-agent recipe run` (whole recipe in one process).
  **Replaced the hand-written `adapter-acme`** — site logic is now a `.kdl` recipe (the
  `acme/scrape_profile` testbed recipe lives gitignored under `/recipes/`, never committed).

## Non-negotiable design invariants (do not break these)

- **Determinism.** All time flows through an injected `Clock` (`crates/core/src/clock.rs`);
  all randomness through an injected `Rng` (`crates/core/src/rng.rs`). Core code must
  NEVER call wall-clock (`SystemTime::now`, `chrono::Utc::now`) or the `rand` crate
  directly — tests rely on `TestClock`/`TestRng` for reproducibility. The daemon wires
  in `SystemClock` + a seeded `SeededRng` at the edge (`crates/daemon/src/main.rs`).
- **Error classes** (`AdapterError` in `crates/core/src/model.rs`): `Retryable`
  (backoff + requeue, capped by `max_attempts`), `Terminal` (fail immediately),
  `RateLimited { retry_after }` (defer to a time). Adapters return these; the runner
  (`crates/core/src/runner.rs`) enforces the semantics. A panic inside an adapter's
  `execute()` is now caught at the runner boundary and folded into `Terminal` — see below.
- **Adapters declare limit keys; the engine enforces limits.** An adapter's `ActionSpec`
  lists `limit_keys`; the pacing engine (`crates/core/src/limits.rs`) applies daily caps,
  min-gap, active-hours windows, and jitter per key from `config.toml`.
- **`Response::Ok(Value)` is internally tagged** — every Ok handler must return a JSON
  object (`json!({...})`), never a bare string/array/number/bool, or serialization breaks.

## Toolchain / how to build (IMPORTANT: Rust is not on PATH)

Start every shell with: `source ~/.cargo/env` (Rust 1.96, pinned via `rust-toolchain.toml`).
`rustfmt`/`clippy` are installed for the 1.96.1 toolchain.

Gates that must stay green before any commit:
- `cargo test --workspace` — currently **86 tests, all passing** (+3 `#[ignore]`d live browser
  tests: `cargo test -p pacewright-browser -- --ignored`, needs `chrome-agent` + Chrome;
  +1 `#[ignore]`d live-git test: `cargo test -p pacewright-cli -- --ignored live_clone`).
- `cargo clippy --workspace --all-targets -- -D warnings` — clean, zero warnings.
- `cargo fmt` before committing your own changes.

Note: the repo currently has pre-existing whole-repo `cargo fmt --check` drift (~116
locations, present since M1 — the code was hand-formatted, not rustfmt'd). It is NOT a
CI gate today. Do not do a blanket `cargo fmt` on the whole tree in a feature commit —
that would bury your real change under a huge unrelated reformat. If you want to adopt
rustfmt, do it as its own dedicated commit and flag it. Keep your OWN new code
rustfmt-clean and matching the surrounding hand-formatted style (dense one-line structs,
etc.).

## Workflow (solo developer — no branches)

Federico works solo and merges locally. Commit directly to `main` (or a short-lived
branch merged `--no-ff` immediately — either is fine). **Do NOT push to origin/remote and
do NOT force-push** — Federico pushes manually. Local commits/branches/merges are all yours.

Commit-message trailer convention used in this repo:
`Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>` (add it if you commit).

## State of the tree (git log, newest first)

```
7f8372a docs: refresh HANDOFF — browser seam, first real acme task, chrome-agent gotchas
c4f26c7 Merge m2-browser-seam: BrowserHandle seam + CliBrowser + acme adapter
cb6ad99 fix(browser,acme): isolate chrome-agent page; check settled state after goto
382218a feat: CliBrowser + acme profile adapter, wired into the daemon
3d0e143 feat(core): BrowserHandle seam — trait in RunCtx, threaded through the runner
d3164f7 docs: add implementation plan for chrome-agent fork (M2 workstream 1)
4a218ad docs: add chrome-agent fork spec (M2 workstream 1 — lib API + §9 extensions)
e97abe4 Merge m1-hardening: single-source terminal set, dedup index, pause/resume RPC test
```

`main` is the integration branch. All old feature branches are fully merged into `main`
and deleted — history is preserved by the `--no-ff` merge commits above.

## What just landed (this session)

1. **`BrowserHandle` seam (M2a).** `crates/core/src/browser.rs` defines the trait
   (`goto`/`eval`/`screenshot`) plus `NullBrowser` (fails `Terminal`, the default) and
   `FakeBrowser` (scriptable, records calls). `RunCtx` finally carries
   `browser: Arc<dyn BrowserHandle>`, filling the M1 placeholder; `run_task` threads it in;
   `Engine::with_browser` attaches one without disturbing browser-free callers. Core owns
   only the trait — no impl — so it stays deterministic and browser-free.
   `BrowserError → AdapterError`: navigation/io are `Retryable`, unavailable/eval `Terminal`.

2. **`CliBrowser` + `AcmeAdapter`.** `scrape_profile` runs against the real site today
   (see "Running a real acme task"). The adapter declares `acme.profile_scrape`, so
   pacing is enforced by the engine, not the adapter. URLs are validated to `acme.com`
   over https so a queued task can't repoint the adapter at an arbitrary host.

3. **Two bugs found only by actually running it** — both now regression-tested, and both
   written up under "chrome-agent gotchas": the machine-global `default` page collision
   (the media-platform tooling navigated our page mid-task), and `goto` echoing the requested
   URL so auth-wall detection read a stale URL.

4. **`recipe add`/`list` (recipe distribution).** `crates/cli/src/recipe_install.rs`.
   `pacewright recipe add owner/repo[@ref][#subdir]` shallow-clones the repo, discovers
   and validates every `.kdl` (skipping non-recipe / invalid files), copies the valid ones
   under `~/.pacewright/recipes/<owner>__<repo>/`, and records provenance (URL, ref, pinned
   HEAD SHA, recipe names) in a `.sources.toml` manifest — reproducible + listable via
   `recipe list`. Pure local FS op, no daemon round-trip. Post-clone logic is factored into
   `install_from_dir` so discover/validate/copy/record is unit-tested against a local dir;
   the live git path has an `#[ignore]`d smoke test. NOTE: this installs recipes as *data*;
   the engine that *executes* KDL recipes is not built yet (needs the chrome-agent fork — see
   roadmap). acme recipes live in `/recipes/` (gitignored) as a local testbed only.

Progress ledger with full detail: `.superpowers/sdd/progress.md` (gitignored, local only).

## Running a real acme task (works today)

```bash
cargo build --release
./target/release/pacewrightd &                       # or restart the existing one
./target/release/pacewright adapters                 # acme/scrape_profile should be listed
./target/release/pacewright add acme scrape_profile \
    --params '{"url":"https://www.example.com/in/me/"}'
./target/release/pacewright get <id>                 # result = scraped profile JSON
./target/release/pacewright limits                   # acme.profile_scrape counter incremented
```

`/in/me/` resolves to your own profile — it proves the authenticated path **without**
sending a profile-view notification to a third party. Viewing someone *else's* profile
while logged in does notify them; keep that in mind before pointing this at leads.

### chrome-agent gotchas (both cost real debugging time — do not relearn them)

1. **`--copy-cookies` only fires when chrome-agent launches a *fresh* browser.**
   `copy_chrome_cookies` is called inside `launch_browser`; if a session already exists,
   chrome-agent reuses it and silently skips the copy, so you stay logged out and hit the
   auth wall. Fix: `chrome-agent --browser pacewright close --purge`, then retry.
2. **chrome-agent's browsers and pages are *named and global to the machine*.** At the
   defaults every consumer shares one browser and one page called `default`. The
   media-platform/podcast tooling on this box drives that page. Because `goto` and `eval` are
   separate subprocesses, a concurrent consumer can navigate the page between them — we
   observed an eval intended for an acme profile return `globex.com`. `CliBrowser`
   therefore pins `--browser pacewright --page pacewright` on **every** verb. Never let
   pacewright touch the `default` page.

Related: `goto` echoes the **requested** URL, not the post-redirect one. Always read the
settled `location.href`/`document.title` back via `eval` before deciding anything (this is
how `AcmeAdapter` detects auth walls). And the site ships build-hashed class names
(`e6590096 _3293afb7 …`) — class-based selectors rot instantly; anchor on the `<main>`
heading and stable text patterns instead.

## Roadmap — what to build next

M1 (core engine) is DONE. **M2's in-repo browser seam is DONE**, and **M3's first slice
(`acme/scrape_profile`) runs against the real site.**

| M | Scope | State |
|---|---|---|
| M2a | **Browser handle in `RunCtx`** (`BrowserHandle` trait + `CliBrowser` + `FakeBrowser`) | ✅ done |
| M2b | Fork chrome-agent → **KDL recipe engine** (superseded the lib-facade plan) | ✅ engine + write/HTTP verbs done on `feat/recipe-engine` (held); upstream PR held |
| M2c | pacewright `adapter-recipe` (RecipeAdapter/Registry/Runner) + vault job-runner | ✅ done (this session) |
| M3 | **profile** adapter | ✅ now a `acme/scrape_profile` **recipe** (Rust `adapter-acme` deleted); avatar capture not started |
| M4 | **post / edit-mentions / reply-comments** + **pages** adapter | not started |
| M5 | globex adapter (extract raw, export magic clips → Spotify → unlisted upload) | not started |
| M6 | Content-platform upload adapter + daily limits | not started |
| M7 | MCP server + Claude skill | not started |
| M8 | Tauri desktop GUI (Postiz replacement) + migrate off Postiz | not started |

### The deliberate detour on M2

Federico's approved design (`docs/specs/2026-07-07-chrome-agent-fork-lib.md` + the plan in
`docs/plans/`) is to **fork `sderosiaux/chrome-agent` into a Rust library** — it is
published to crates.io but is **binary-only** (`[[bin]]`, `autolib = false`, no `lib.rs`),
so `cargo add chrome-agent` gives you nothing callable. That fork is still the intended
substrate.

To get a *working* acme task without blocking on the fork, `BrowserHandle`'s first impl
(`CliBrowser`) shells out to the chrome-agent **CLI**. The trait's methods deliberately
mirror the fork's planned `Page` API (`goto`/`eval`/`screenshot`), so a native
`chrome_agent::Session`-backed impl drops in behind the same trait with **zero adapter
changes**. Nothing about the fork plan is invalidated; it just isn't on the critical path.

**Recommended next step:** either (a) build the fork per the existing plan and swap in a
`NativeBrowser`, or (b) extend the acme adapter (avatar capture needs `screenshot` +
the viewport fix, which is exactly what the fork's §5.1 delivers — so (a) unblocks it).

## Remaining M1 "known gaps" (deferred, not bugs — see README)

- `set_limit` RPC — limits set via `config.toml` + restart, no runtime RPC.
- `run_now --force` — re-queues now but does not override an active limit defer.
- `subscribe` push — TUI polls `list` + `status` each second instead.
- Single-instance guard — best-effort `AddrInUse` refusal (no lock file); launchd
  enforces one instance in practice.

## Open ideas / smaller follow-ups (author's notes, none blocking)

- Pause/resume state is in-memory; a `pause_scopes` table (or a row in an existing meta
  table) would make it survive daemon restarts if operators expect that.
- `paused_scopes()` returns an unordered `Vec` from a `HashSet` — fine for JSON/TUI, but
  sort it if you ever add a snapshot-style test. (The new `test_pause_resume_via_dispatch`
  in `crates/daemon/src/server.rs` avoids order-dependence with `contains`.)
- `flaky_state` map in `DummyAdapter` is unbounded (test-only adapter, so harmless).
- **The tick loop holds the engine lock for the whole task.** `serve()` does
  `let e = engine.lock().await; e.tick().await`, and `handle_request` also locks the engine.
  M1's tasks were microseconds; a browser task is seconds-to-90s, so a long
  `acme/scrape_profile` will block every RPC (`status`, `list`, the TUI's 1s poll) until
  it finishes. Worth fixing before browser tasks get common: run tasks outside the lock, or
  hold the lock only for store/registry reads.
- **`CliBrowser` has one page.** Concurrent browser tasks in the same tick would interleave
  `goto`/`eval` on the same chrome-agent page and corrupt each other, the same way the
  media-platform tooling corrupted us. Today the engine runs tasks sequentially, so this is
  latent — but any move to parallel task execution must give each task its own `--page`.
- `AcmeAdapter`'s `headline`/`location` are positional guesses over `top_card`. The raw
  `top_card` array is returned precisely so this can be remapped without a redeploy. Avatar
  capture (M3's other half) needs `screenshot` + a real viewport (chrome-agent's default
  caps around ~469px) — i.e. it needs the fork.

**Resolved in the M1-hardening pass (2026-07-07), no longer open:**
- ✅ Daemon-level RPC round-trip test for `Pause`/`Resume`/`Status` — added
  `test_pause_resume_via_dispatch` (`crates/daemon/src/server.rs`).
- ✅ Terminal-status drift — `store.rs` no longer hardcodes the terminal set in SQL;
  `find_active_by_dedup` derives it from `TaskStatus::terminal_strs()` (single source of
  truth over `is_terminal`), guarded by an exhaustive-match test in `model.rs`. Added a
  partial index `idx_tasks_dedup` on `tasks(dedup_key)`.
- ✅ DST `.single().unwrap()` — was *already* resolved in production code before this pass:
  `limits.rs` uses non-panicking `resolve_local`/`local_from_millis`. The only remaining
  `.single().unwrap()` is a test helper (`noon_ms()`) on a fixed non-DST date. The old
  note here was stale.

## First moves for the next agent

1. `source ~/.cargo/env && cd /Users/agente/work/pacewright`
2. `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings` —
   confirm the 72-test / clippy-clean baseline before changing anything.
3. Read `docs/specs/2026-07-07-core-engine-design.md`, then `crates/core/src/browser.rs`
   (the `BrowserHandle` seam) and `crates/adapter-acme/src/lib.rs`.
4. Prove the stack still works end-to-end: run a real acme task (see the section above).
   If it hits an auth wall, re-read the two chrome-agent gotchas — it is almost always the
   fresh-launch cookie copy.
5. Then pick up the chrome-agent fork (`docs/plans/2026-07-07-chrome-agent-fork-lib.md`)
   — or ask Federico which milestone he wants next.
