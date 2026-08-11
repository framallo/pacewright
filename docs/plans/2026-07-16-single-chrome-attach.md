# Plan: one attached Chrome, one tab per site

**Status:** Phases 1–5 implemented (178 tests green, clippy clean). Phase 0 mechanical half
verified live; **auth half blocked on Federico at the keyboard**. Phase 6 not started.
**Not committed** — the working tree already held unrelated in-flight work before this started.
**Date:** 2026-07-16
**Supersedes the browser model in:** `docs/specs/2026-07-10-recipe-auth-login.md`

## Problem

pacewright gives every *account* its own chrome-agent-**launched** browser
(`runner.rs:153`, `auth.rs:CliLoginLauncher`). Launching Chrome over CDP is a bot signal:

- **acme** walls a launched profile on profile scrapes and revokes its session cookie,
  logging the session out. This is the bug that motivates the refactor.
- **Some identity providers refuse sign-in on a launched browser** outright ("this browser or app
  may not be secure"), which is why `pcw auth login initech-account` hangs at
  "logging in…" and why the CDP-**attach** workaround exists outside the auth system.

The `--headed` patch at `runner.rs:177` treats the symptom (headless is *also* a bot
signal) but not the cause: the browser is still launched, still a fresh Chromium, still
not the operator's real Chrome.

## Target model

One real Google Chrome, non-headless, always running, launched by launchd (never by
chrome-agent), with a dedicated profile. pacewright **attaches** to it. Sites are
separated by **named tabs**, not by browser profiles.

| | today | target |
|---|---|---|
| browser | one launched Chromium **per account** | one attached Chrome, **shared** |
| isolation unit | `--browser <account>` (profile) | `--page <site>` (tab) |
| how it starts | chrome-agent launches on demand | launchd, `--remote-debugging-port=9222` |
| `--headed` | per-account patch | gone (the real Chrome is visible by definition) |
| `--copy-cookies` | snapshot into a throwaway profile | gone (the profile *is* the session) |
| `auth login` | launches a headed window | navigates the site's tab, `--activate` |
| bot surface | launched Chromium | a real Chrome a human signs into |

This is not speculative. `~/.chrome-agent/sessions.json` already contains a working
precedent — the `initech-attach` entry, `pid: null` with a ws endpoint on 9222 and named
pages. The refactor generalizes that one entry into the only path.

## Verified assumptions

**Phase 0 mechanical half: RUN 2026-07-16, PASSED.** Live against Chrome 150 launched with
`--user-data-dir=~/.pacewright/chrome-profile --remote-debugging-port=9222`:

- endpoint live; UA reports `Chrome/150.0.0.0`, **not** `HeadlessChrome` — the
  fingerprint that matters. ✅
- `--connect http://127.0.0.1:9222` attaches. ✅
- **three named pages (`acme`/`initech`/`globex`) coexist as three real tabs in one
  browser, and navigating one does not clobber another** — re-reading the `acme` tab
  after driving the other two still returned `Example Domain`. This is the isolation
  property the whole refactor rests on. ✅
- `gc` reaped the dead launched `acme-account` and left the attached session alive
  (`remaining: 3`). The "external `--connect` sessions are never touched" claim is now
  **verified, not just documented**. ✅

Behaviours probed live because the whole design depends on them:

- **`--connect` does NOT silently fall back to launching.** With no cached session and a dead
  endpoint it fails in-band (`{"ok":false,"error":"Could not resolve CDP WebSocket…"}`) on a
  **zero exit**. This was the scariest possible failure mode — a silent launch would have
  reintroduced the acme bug invisibly whenever Chrome was down. It does not happen. ✅
- **A cached session record beats the `--connect` flag.** Pointing `--connect` at a dead port
  while a live record existed for that browser name silently reused the cached endpoint. So
  changing `browser.connect` does not take effect while a stale record exists — a config-doesn't-
  apply trap worth knowing.
- **Attach self-heals across a Chrome restart.** launchd `KeepAlive` mints a new browser GUID, so
  the cached `wsEndpoint` goes stale. chrome-agent re-resolves from the `--connect` URL and
  rewrites the record, still attached (`pid: null`). **Verified end to end**: killed Chrome →
  launchd revived it → the next verb worked with no intervention. This is why `--connect` belongs
  in `global_args` (every verb), not just on `goto`. ✅

Two structural findings from the spike:

1. **`--browser` must still be pinned when attached.** Omitting it filed our three pages
   under the browser key `default` — precisely the shared-`default` hijack hazard
   `browser/src/lib.rs` already warns about. Under `--connect` the name is only the
   `sessions.json` bookkeeping key (the endpoint identifies the browser), but it still
   must not be `default`.
2. **`sessions.json` records `headless: true` for attached sessions** even though the
   Chrome is visibly headed (`initech-attach` shows the same). Cosmetic chrome-agent
   bookkeeping — `pcw chrome status` must not trust that field.

**Auth half: VERIFIED 2026-07-17 — the last risk is closed.** Federico signed into both
providers by hand in the attached Chrome; then, driven through `--connect`:

- **initech's studio dashboard** loaded signed in. The identity provider **accepts**
  sign-in on a `--remote-debugging-port` Chrome — the single biggest risk in the whole plan. ✅
- **acme** loaded the feed signed in, then loaded a **profile** (the historically
  session-burning op), and a feed reload right after was **still signed in** —
  the session cookie survived. This is the exact failure the refactor exists to fix, and attach fixes it. ✅

Nothing about the design is unproven now. The one operational wrinkle found (below) is about tab
bookkeeping, not auth.

**Operational finding — stale page targets don't self-heal.** The browser-level GUID self-heals
across a Chrome restart (verified 2026-07-16), but a *page*-level stale target does not: when the
`acme` tab's cached `targetId` was dead (tab closed since it was cached), chrome-agent errored
`Target … not found in /json/list` instead of recreating the tab. Pruning the page from
`sessions.json` fixed it. **This will bite in production** — if the operator closes a site's tab,
the next scheduled task for it fails until the record is pruned. The daemon should treat that error
class as "recreate the page and retry once" (or `pcw chrome status` should offer a prune). Not
blocking, but it turns a closed tab into a silent task failure.

## Accepted trade-offs

**Foreground contention — smaller than it looked.** One Chrome means one foreground tab, and
Chrome throttles background tabs, which is what stalls a globex render. But pacewright's
dispatch loop is already strictly serial (verified, and now pinned by
`foreground_serialization_is_load_bearing`), so no two recipes can contend for the foreground and
**no mutex is needed**. All that was required was `foreground #true` → `--activate` on the one
recipe that must be watched. The regression is real only if the dispatch loop is ever made
concurrent — which the pin catches.

**One account per site, permanently.** Two acme accounts is possible today (two
profiles) and becomes impossible. Federico runs one account per site, so this is
accepted, not overlooked.

**Shared blast radius.** A Chrome crash or profile corruption takes every site down at
once, where today it would take one. Mitigated by the dedicated profile (no human drives
it) and launchd `KeepAlive`.

## Phases

Each phase is independently landable and leaves the tree green. The codebase tests
argument *shape* as pure functions (`runner::args`, `CliBrowser::global_args`,
`CliLoginLauncher::login_args`), so every behavioral change here has a natural failing
test to write first. That is the TDD entry point: **rewrite the arg-shape test to encode
the new model, watch it fail, then change the builder.**

### Phase 0 — Spike: prove the attach path ✅ FULLY PASSED

Both halves done. Mechanical half 2026-07-16 (attach works, named tabs isolate, `gc` is safe);
auth half 2026-07-17 (initech signed in, acme profile load with the session cookie surviving — see
Verified assumptions). The gate is met, so Phase 6 is unblocked.

### Phase 1 — Chrome supervision + preflight ✅ (except `pcw chrome status`)

- ✅ launchd plist `packaging/com.paperclip.pacewright-chrome.plist` — `KeepAlive`, `RunAtLoad`,
  port 9222, dedicated `--user-data-dir`, `ProcessType Interactive` (**not** `Background`: macOS
  throttles Background processes and Chrome already throttles non-foreground tabs — together they
  would stall exactly the globex renders Phase 4 is about). **Installed and loaded**; KeepAlive
  revival verified.
- ✅ `Config.browser_connect` from `[browser] connect` (`core/src/config.rs`), replacing
  `browser_idle_timeout_ms`. The retired `idle_timeout` key is ignored rather than fatal, so an
  operator upgrading with it still in `config.toml` doesn't get a boot failure — covered by
  `test_retired_idle_timeout_key_is_ignored_not_fatal`.
- ✅ Endpoint threaded from config to **all three** chrome-agent callers in `daemon/main.rs`
  (`CliBrowser`, `CliRecipeRunner`, `CliLoginLauncher`) so they cannot drift apart.
- ✅ `core::browser::explain_connect_failure` rewrites chrome-agent's opaque "Could not resolve
  CDP WebSocket" into the cause + the `launchctl load` fix, and reclassifies it `Unavailable`
  (it arrives as in-band `ok:false` on a zero exit, so it would otherwise read as "eval failed").
  Wired into `CliBrowser::check_ok` and `runner::classify`; the test pins the message chrome-agent
  actually emits, captured live.
- ⬜ `pcw chrome status` (up/down + endpoint). Must not trust `sessions.json`'s `headless` field —
  it reads `true` for attached sessions even when the Chrome is visibly headed.

### Phase 2 — `CliBrowser` and `CliRecipeRunner` attach ✅

- add `connect: Option<String>`; when set, emit `--connect <endpoint>` and **drop**
  `--copy-cookies` (there is nothing to copy into an attached real profile) and
  `--headed` (meaningless when attached).
- `browser_name` collapses to one shared name.
- **account → page**, replacing `runner.rs:153`'s account → browser:
  ```rust
  let page = account.unwrap_or(&self.page_name);   // was: browser
  ```
- tests to rewrite first (they currently encode the old model and **must** fail):
  - `every_verb_is_pinned_to_a_named_browser_and_page` → pins the *shared* browser and
    the *per-site* page.
  - `goto_carries_session_flags_but_eval_does_not` → attach carries neither
    `--copy-cookies` nor `--headed`.
  - `login_window_is_headed_activated_on_the_main_tab` → see Phase 3.
  - add: attached args never contain `--headed`/`--copy-cookies`; account `foo` maps to
    `--page foo` under one `--browser`.

### Phase 3 — `auth login` stops launching ✅

`CliLoginLauncher::open` no longer spawns a browser. It navigates the account's tab in
the attached Chrome with `--activate` and leaves the window up for the human. The
`LoginLauncher` trait and `AuthManager` (status cache, `logging_in`, recheck-clears-flag)
are unchanged — only the launcher's args change, so `auth.rs`'s existing tests keep
their shape.

This is the fix. acme login stops burning its own session because nothing launches a
browser anymore; the identity-provider special-case workaround folds back into `auth login` and the
skill's "⚠ some providers can't use `auth_login`" caveat is retired.

### Phase 4 — Foreground ✅ (no mutex — the premise was wrong)

**The proposed mutex was cut: pacewright already runs tasks strictly serially.** The daemon's tick
loop `handle.await`s each task before claiming the next (`server.rs`); its `tokio::spawn` is panic
isolation, not concurrency. So two recipes cannot fight over the foreground tab, and a lock would
have been redundant machinery guarding a property that already holds.

What was actually needed is the other half — even serialized, a render needs its tab *in front*,
and the last-activated tab may belong to another site:

- ✅ `foreground #true` recipe flag → `RecipeMeta.foreground` (`registry.rs`), default **off**
  (raising a window steals focus on the operator's Mac; scrapes and API polls must not).
- ✅ `CliRecipeRunner` passes `--activate` only for foreground recipes.
- ✅ Applied to `globex/render_clips` — the one recipe that needs it. Verified by parsing the
  real installed recipe: `render_clips foreground=true`, `publish_clips`/`share_spotify` false.
  ⚠️ That `.kdl` is **generated**, and its generator lives only in ephemeral job scratch
  (`~/.claude/jobs/f4ebd47e/tmp/gen_recipes.py`). Both were updated, but the generator will not
  survive — **move it into the repo** or a regen silently drops the flag and renders stall again.
- ✅ `RunOpts { account, foreground }` replaces the old `(auth: bool, account: Option<&str>)`
  positional pair. `auth` had become genuinely dead: it only ever chose whether to copy cookies
  into the throwaway profile that attaching removed.
- ✅ **`foreground_serialization_is_load_bearing`** (`daemon/tests/e2e.rs`) pins the serial
  property this decision rests on: a probe adapter records max concurrent `execute` calls, and
  asserts it never exceeds 1. Confirmed to actually bite — sabotaging the loop to dispatch
  concurrently makes it fail with `left: 3, right: 1` and a message telling the reader that
  `foreground` now needs real serialization. **If someone parallelizes the dispatch loop, this
  test fails first and Phase 4's mutex becomes real work.**

### Phase 5 — Retire the reaper ✅

Removed `spawn_browser_reaper`, `DEFAULT_BROWSER_IDLE_TIMEOUT_MS`, `BROWSER_REAP_EVERY`, and
`browser.idle_timeout`. Against an attached session the reaper is a guaranteed no-op (verified:
`gc` reaped the dead launched `acme-account` and left the attached session alive), and the
Chrome is now supervised by launchd, so reaping was not pacewright's job any more.

**It was also silently broken.** It passed `--json` *after* the `gc` subcommand, but `--json` is a
global flag — chrome-agent exits with a usage error, which landed in the `Ok(out) =>
tracing::debug!(...)` arm. It had never reaped anything since it was written (commit `9f0961e`,
the HEAD commit) and said nothing about it. Deleting dead code, not working code.

### Phase 6 — Migrate recipes + docs 🔶 IN PROGRESS

- ✅ **Recipes de-generated → static with parameters** (Federico's directive 2026-07-17). The
  globex recipes were emitted by `gen_recipes.py`, which lived only in ephemeral job scratch —
  a regen would silently drop `foreground #true` and stall renders. Root cause: `media_export`
  was a compiled chrome-agent step verb that **no longer exists** (`recipe check` →
  `unknown step verb`), so the generator inlined the JS. That inlined form is now the canonical
  **static, hand-maintained** file. Done: rewrote the "edit the generator, not this file" headers
  to name the two `{{ url }}`/`{{ guest }}` parameters; synced repo staging
  (`recipes/globex/{render,publish}_clips.kdl`, which still held the dead `media_export`
  verb) to the working form; retired the generator to `*.RETIRED` with a README so a stray run
  can't clobber. All six globex recipes pass `chrome-agent recipe check`; `render_clips` parses
  `foreground=true`, the others false.
  ⚠️ The true distribution channel is the external awesome-recipes repo (`.gitignore` calls
  `recipes/` "distributed data"); it is **not reachable from here** and still needs the same
  static versions pushed, or a `pcw recipe add` would reinstall the dead verb form.
- ⬜ account recipes keep their names; `accounts/*` still needs a daemon restart (`server.rs:24`).
- ⬜ `pcw auth recheck` for all three accounts against the attached Chrome, end to end.
- ⬜ retire the three launched profiles from `sessions.json` (move aside, don't delete).
- ⬜ update the `pacewright` skill: drop the identity-provider `auth_login` caveat, document the single
  Chrome + tab-per-site model and the launchd job.

## Open question for Phase 1

Whether the launchd Chrome should be `LaunchAgents` (per-user, needs a GUI session — the
right call given it must be non-headless and visible) or something that survives logout.
Non-headless implies a logged-in GUI session, so `LaunchAgents` is the default answer
unless the box is ever driven headless-over-SSH.
