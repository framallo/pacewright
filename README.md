# pacewright

A **wright** (craftsman) of **pace** — a Rust tool that **queues, schedules, and runs browser-automation tasks** with human-like pacing and per-platform daily limits.

pacewright replaces a pile of ad-hoc daemon scripts (and eventually [Postiz](https://postiz.com)) with one background service you drive from a CLI, a live TUI, an MCP server, or a desktop app. It is built to automate LinkedIn, Riverside, and YouTube through their UIs without tripping bot-detection — the safety comes from driving a real, logged-in browser over CDP and from a scheduler that enforces daily caps, minimum gaps, active-hours windows, and jitter.

> **Status: Milestone 1 (core engine) complete.** The engine — queue, scheduler, pacing/limits, runner, durable tracking — is done, tested (47 tests, `clippy -D warnings` clean), and proven end-to-end against a browser-free **DummyAdapter**. Real platform adapters, the MCP server, and the Tauri GUI are later milestones.

---

## Why it exists

Automating an authenticated LinkedIn/YouTube session safely is mostly about *not looking like a bot*: consistent fingerprint, a real session, and — critically — **human pacing** (bounded volume, spacing between actions, working hours, randomness). pacewright puts that pacing in the engine, declared per action, so every adapter inherits it for free. See [`docs/specs/`](docs/specs) for the full design and the anti-detection research behind it.

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
   pacewright  pcw tui   Tauri app   MCP server   (thin clients)
     (CLI)    (dashboard)  (later)     (later)
```

The engine is **platform-agnostic**. Adapters implement one trait (`execute(action, params) -> Result`) and *declare* which daily-limit keys each action spends; the engine enforces the limits, persists everything, and never needs to know what LinkedIn is. That boundary is what lets the whole engine be tested with a fake adapter and zero browser.

### Workspace

| Crate | Responsibility |
|---|---|
| `pacewright-core` | Task model, SQLite store, injectable `Clock`/`Rng`, limits engine (cap · min-gap · active-hours · jitter), scheduler, runner (retry/backoff/recurrence/deps), engine (boot-recovery + tick) |
| `pacewright-proto` | JSON-RPC request/response wire types shared by daemon + clients |
| `pacewright-daemon` | `pacewrightd` — Unix-socket server + 1s tick loop |
| `pacewright-cli` | `pacewright` (alias `pcw`) — client subcommands + live ratatui TUI |
| `pacewright-adapter-dummy` | reference `Adapter` (`echo`/`slow`/`flaky`/`always_fail`/`rate_heavy`/`panic`) for testing the engine |

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
| `tui` | live dashboard |

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

## Install as a background service (macOS launchd)

```bash
cp packaging/config.example.toml ~/.pacewright/config.toml
sed "s#__HOME__#$HOME#g" packaging/com.paperclip.pacewrightd.plist > ~/Library/LaunchAgents/com.paperclip.pacewrightd.plist
launchctl load ~/Library/LaunchAgents/com.paperclip.pacewrightd.plist
```

## Roadmap

| Milestone | Scope |
|---|---|
| **M1 ✅** | Core engine + DummyAdapter + CLI + TUI + launchd |
| M2 | Extend chrome-agent (larger viewport, real CDP input, human mouse movement, `Runtime.enable` audit) + browser handle in `RunCtx` |
| M3 | LinkedIn **profile** adapter (scrape + avatar) — port `linkedin_scraper` to Rust |
| M4 | LinkedIn **post / edit-mentions / reply-comments** + **pages** adapter |
| M5 | Riverside adapter (extract raw, export magic clips, → Spotify, → YouTube unlisted) |
| M6 | YouTube adapter + daily limits |
| M7 | MCP server + Claude skill |
| M8 | Tauri desktop GUI (Postiz replacement) + migrate off Postiz |

## Known gaps (M1)

Intentionally deferred to later milestones, not oversights:

- **`set_limit` RPC** — limits are set via `config.toml` + restart; no runtime RPC yet.
- **`run_now --force`** — resets `scheduled_for`/clears `next_eligible_at` (re-queues now) but does not yet override an active limit defer.
- **`subscribe` push** — the TUI polls `list` + `status` each second instead.
- **Single-instance guard** is best-effort (`AddrInUse` refusal, no lock file); launchd enforces one instance in practice.

## Design docs

- Spec: [`docs/specs/2026-07-07-core-engine-design.md`](docs/specs/2026-07-07-core-engine-design.md)
- M1 implementation plan: [`docs/plans/2026-07-07-m1-core-engine.md`](docs/plans/2026-07-07-m1-core-engine.md)

## License

MIT
