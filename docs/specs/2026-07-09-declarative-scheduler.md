# Declarative scheduler — enable-able recurrent tasks

Status: **implemented** (2026-07-09). Builds on the recipe engine
(`2026-07-08-recipe-engine-in-chrome-agent.md`). This milestone is **Rust-only: schedule config +
reconciler + CLI + TUI**. The web dashboard (`2026-07-09` design discussion) is a documented
follow-up that consumes the same RPCs.

**Delivered:** the `schedule` module in `adapter-recipe` (not `core` — validation needs the
`RecipeRegistry`, which depends on `core`; `core` owns the `schedule_state`/`limit_overrides`
tables + the `dedup_key`-preservation fix); `Schedule{Apply,List,Enable,Disable}` + `SetLimit`
RPCs; boot reconcile + override merge in the daemon; `pcw schedule check/list/apply/enable/disable`;
and the TUI Feed / Schedule / Limits panes (`tab` to cycle, `space` to toggle, `a` to apply).

---

## 1. The model — recipe / task / schedule

Three separated concepts (the crux decision: the schedule does **not** live in the recipe file):

| Concept | What it is | Owned by | Lives in |
|---|---|---|---|
| **Recipe** | *how* to execute (steps, locators, `limit-key`s) | shared / distributed (`pcw recipe add`) | `~/.pacewright/recipes/*.kdl` |
| **Task** | a *specific case* — the params the recipe needs | the operator | a schedule file |
| **Schedule** | *when* / how often, and whether it's enabled | the operator | a schedule file |

A recipe is reusable and shareable — `acme/scrape_profile` runs for many leads on many
cadences. Baking a schedule into it would force your timing on everyone who installs it and stop
one recipe serving many cases. So **recipe = how (shared), task = which params (your case),
schedule = when (yours)**.

The UX centerpiece (seo-os-inspired): a **catalog of recurrent tasks you toggle on/off**.

---

## 2. Schedule files — `~/.pacewright/schedules/*.toml`

A directory of TOML files (one per pipeline, e.g. `podcast.toml`, `leads.toml`), each a list of
task entries. Same TOML idiom as `config.toml`; git-friendly.

```toml
# ~/.pacewright/schedules/daily.toml
[[task]]
id      = "hn-digest"                    # stable, globally-unique id — the reconcile key
recipe  = "news/hackernews"              # the "how" (must be an installed recipe)
every   = "0 9 * * *"                    # cron (croner 5- or 6-field) → recurring
params  = { url = "https://news.ycombinator.com/", out_dir = "~/vault/digests" }

[[task]]
id      = "scrape-jane"
recipe  = "acme/scrape_profile"
at      = "2026-07-10T09:00:00"          # RFC3339 → one-shot at a time
params  = { url = "https://www.acme.com/in/jane/" }

[[task]]
id      = "spotify-covers"
recipe  = "spotify/upload_cover"
params  = { episode = "e42" }            # no every/at → runs once when applied
enabled = false                          # declared but off by default
```

**Fields.** `id` (required, unique), `recipe` (required, `<adapter>/<action>`), `params` (table,
default `{}`). Timing — exactly one of `every` (cron) / `at` (RFC3339) / neither (one-shot on
apply). Optional `priority` (i64), `max_attempts` (i64), `enabled` (bool, default `true`) — the
**declared default** enabled state.

**Validation** (`schedule check`, offline): every file parses; `id`s are globally unique across all
files; each `recipe` resolves in the `RecipeRegistry`; the recipe's **required vars** are satisfied
by `params` (plus injected `~`-expansion, no `from`-aliases here — params are already var-keyed);
`every` parses as cron / `at` parses as RFC3339.

---

## 3. Enable / disable — declared default + runtime override

Enabling/disabling a recurrent task is a **first-class runtime action**, not a file edit — mirrors
how `set_limit` overrides `config.toml`.

- **Declared default:** the entry's `enabled` field.
- **Runtime override:** persisted in a new SQLite table `schedule_state(task_id PRIMARY KEY,
  enabled INTEGER, updated_at INTEGER)`. Set by `pcw schedule enable/disable <id>`, the TUI toggle,
  (later) the web switch.
- **Effective state:** `override if present, else the file default`. Only effectively-enabled
  entries are queued.

Toggling is idempotent and immediate: `disable` cancels the queued schedule task; `enable`
re-queues it (both via a reconcile pass).

---

## 4. The reconciler — file(s) → queue, desired-state

`reconcile()` makes the queue match the effectively-enabled schedule entries. Runs on **daemon
boot**, on **`schedule apply`**, and after **each enable/disable**.

For each entry, keyed by `dedup_key = "schedule:<id>"`:
- **effectively enabled** and no live task → enqueue a `Task` (recurrence = `every`;
  `scheduled_for` = `at` or now; params/priority/max_attempts from the entry).
- **enabled** and a live task exists → update it in place if `params`/`recurrence`/timing changed
  (compare, then `update_task`); otherwise leave it.
- **effectively disabled** (or entry deleted, under `--prune`) and a live task exists → cancel it.

"Live task" = a non-terminal task with that `dedup_key`. The engine's existing recurrence machinery
(`runner::next_occurrence_ms`) spawns the next occurrence on success; the **spawned occurrence must
preserve `dedup_key`** so reconcile keeps recognizing it (verify/fix `runner.rs` — today's spawn
copies `recurrence` but must also copy `dedup_key`). Consistency is eventual across an in-flight
run: a `disable` during a running instance is fully enforced at the next reconcile.

`apply` is **non-destructive by default**; `--prune` cancels tasks whose entries were removed from
the files (so stale schedule tasks don't linger).

---

## 5. New RPCs (`pacewright-proto`)

Additive to the existing `Request` enum; each returns the usual `Response::Ok(Value)` object.

- `ScheduleApply { prune: bool }` → `{ created, updated, canceled, errors }` counts + any
  per-entry validation errors.
- `ScheduleList` → `[{ id, recipe, params, every|at, enabled_default, enabled_effective,
  next_fire, live_status, last_error }]` (next_fire resolved via croner; live_status from the
  `schedule:<id>` task).
- `ScheduleEnable { id }` / `ScheduleDisable { id }` → upsert `schedule_state`, reconcile, echo new
  effective state.
- `SetLimit { key, config }` → persist a `limit_overrides` row + update in-memory `Config`
  (carried over from the web design; the schedule work needs runtime pacing edits too, and the CLI
  gets it for free). Boot merges overrides over `config.toml`.

The daemon reads `~/.pacewright/schedules/` and holds the `RecipeRegistry` (already built for the
recipe adapters), so it validates + resolves entries server-side. Schedule parsing/validation and
the reconcile diff live in a small **`schedule` module in `pacewright-core`** (pure, unit-testable
over an in-memory store); the daemon wires files + registry + store into it.

---

## 6. CLI surface (`pcw schedule …`)

- `pcw schedule check [dir]` — validate offline (no daemon): parse, unique ids, recipes resolve,
  required vars satisfied, cron/`at` parse. Exit 0/1. Uses the core `schedule` module + registry.
- `pcw schedule list` — the catalog: id · recipe · when · next-fire · effective on/off · live
  status. (daemon RPC.)
- `pcw schedule apply [--prune]` — reconcile the files into the queue. (daemon RPC.)
- `pcw schedule enable <id>` / `disable <id>` — toggle + reconcile. (daemon RPC.)

Existing `add`/`list`/`get`/`recipe …` are unchanged; `schedule` is a new subcommand group.

---

## 7. TUI — the catalog view

A new **Schedule** pane in the ratatui TUI (tab-switch with the existing feed), rendering the
seo-os-style catalog:

```
┌ Schedule ─────────────────────────────────────────────────┐
│ ◉ hn-digest        news/hackernews      next 09:00  ✓ ok   │
│ ◉ scrape-jane      acme/scrape…         once 07-10  ○ pend │
│ ○ spotify-covers   spotify/upload_cover  —          disabled│
└────────────────────────────────────────────────────────────┘
  [space] toggle   [a] apply   [tab] feed/schedule/limits   [q] quit
```

- Rows are the declared entries with effective on/off (`◉`/`○`), recipe, next-fire, last live
  status. `space` toggles the highlighted row (enable/disable RPC); `a` applies.
- A **Limits** pane too (per-key `count/cap`, min-gap, window) reusing the `limits` RPC; editing
  limits stays CLI/web for now (TUI is read-mostly for limits in this milestone).
- The existing live **Feed** pane (task rows) stays; `tab` cycles Feed / Schedule / Limits.

No new dependency — still `ratatui` + `crossterm`, polling the daemon each second (the push/WS work
belongs to the web milestone).

---

## 8. Testing

- **core `schedule` module:** parse valid/invalid TOML; unique-id + required-var + cron/`at`
  validation; the reconcile diff (create/update/cancel/prune) over an in-memory store and a fake
  registry; effective-enabled = override-over-default.
- **recurrence dedup fix:** a spawned next-occurrence preserves `dedup_key` (regression test in
  `runner.rs`).
- **daemon:** `ScheduleApply/List/Enable/Disable` and `SetLimit` handlers over an in-memory engine
  (mirrors the existing `server.rs` dispatch tests), incl. persistence of overrides across a store
  reload.
- **CLI:** `schedule check` over a temp `schedules/` dir (valid + each failure mode); the request
  builders for the daemon-round-trip subcommands.
- Gates unchanged: `cargo test --workspace`, `clippy -D warnings`, own new files rustfmt-clean and
  matching the repo's hand-formatted style (do **not** blanket-`cargo fmt` existing files).

---

## 9. Sequencing

1. **core `schedule` module** — `ScheduleEntry`/`ScheduleFile` types, TOML parse, validation
   (registry + cron/`at`), and the reconcile diff over the store; `schedule_state` +
   `limit_overrides` tables in `store.rs`; preserve `dedup_key` on recurrence spawn. Unit tests.
2. **proto + daemon** — the new `Request` variants + `server.rs` handlers; boot-time reconcile;
   `SetLimit` override load/merge. Dispatch tests.
3. **CLI** — `pcw schedule check/list/apply/enable/disable`; wire into `main.rs`. Tests.
4. **TUI** — Schedule + Limits panes, `tab` cycling, `space` toggle, `a` apply.
5. Gate + docs (README `schedule` section, HANDOFF), commit on a branch, merge `--no-ff`.

## 10. Deferred (documented, not built here)

- **Web dashboard** — the localhost axum + WS control plane; consumes the RPCs above (the catalog
  toggle becomes a browser switch). Its own milestone.
- **File-watch auto-apply** — today reconcile is boot + explicit `apply` + on-toggle; watching
  `schedules/` for live edits is a later nicety.
- **Vault job notes** stay the ad-hoc per-entity path (`pcw recipe job <note>`), complementary to
  the recurring schedule files.

## 11. The `auth` recipe flag — public recipes run without a signed-in Chrome

Wiring the `news/hackernews` recipe into a schedule and running it live surfaced a real integration
defect: `CliRecipeRunner` passed `--copy-cookies` on **every** run, so even a public, no-auth recipe
required a logged-in Chrome cookie DB and failed (`Chrome cookies file not found`) when one wasn't
present. Cookie-copying is now **per-recipe and declarative**:

- A recipe that needs the operator's logged-in session declares `auth #true` in its `recipe { … }`
  block (e.g. `acme/scrape_profile`). `RecipeRegistry` parses it into `RecipeMeta.auth`; the
  `RecipeAdapter` passes it to `RecipeRunner::run(path, vars, auth)`, which adds `--copy-cookies`
  only then. A public recipe (`news/hackernews`) omits it and navigates cold — chrome-agent ignores
  the unknown node (forward-compatible), so no chrome-agent change was needed.
- `CliRecipeRunner.copy_cookies` remains an optional **global force-on override** (default off) for
  operators who want every recipe to inherit the session regardless.

**Live proof (2026-07-09).** `schedules/daily.toml` → `news/hackernews` (on-apply) run through the
daemon end-to-end: boot reconcile created the task, the tick loop drove `chrome-agent` against the
**live** Hacker News front page, the recipe wrote `hn.json` + `hn-digest.md` into the vault, and the
task reached `succeeded` (attempts 0). `schedule list` / `enable` / `disable` RPCs verified against
the running daemon. (Chrome launch needs a real `$HOME` for its `~/Library` app-support state — a
throwaway `HOME=/tmp/...` hangs Chrome launch, unrelated to pacewright.)
