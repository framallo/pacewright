# pacewright `auth login` — status & pause note (2026-07-15)

**State: PAUSED.** Interactive `auth login` UX is fixed and shipped. The blocker that stopped us is
that a **chrome-agent-launched** browser trips acme's bot detection — see "Open blocker" below.

## Shipped this session (all TDD, clippy `-D warnings` clean)

Four fixes to make `pcw auth login <account>` sane and non-destructive:

1. **No tab-storm poll.** The daemon used to spawn a detached poll that re-ran the signed-in *check*
   every 4 s for 5 min against the profile being logged into — each run opened a new tab and
   navigated it, fighting the human mid-login. Removed. (`daemon/src/server.rs`: dropped
   `spawn_login_poll` + `LOGIN_POLL_*`; `adapter-recipe/src/auth.rs`: dropped `poll_until_signed_in`.)
2. **Login + check share one tab.** Login opened page `main`; the check ran on the runner default
   `pacewright` — a *second* tab that navigated away ("a new window popped up"). Account recipes now
   run on page `main`. (`adapter-recipe/src/runner.rs::args` → page `main` when `account.is_some()`.)
3. **Window raised to foreground.** A reused background profile window navigated invisibly
   ("nothing opened"). New chrome-agent global flag `--activate` → CDP `Page.bringToFront`, opt-in so
   background recipes don't steal focus. The login launcher passes it. (`chrome-agent/src/cli.rs`,
   `chrome-agent/src/run.rs`, `adapter-recipe/src/auth.rs::CliLoginLauncher::login_args`.)
4. **Login opens HOME, not the login form.** Opening `/login` shows a sign-in form even when already
   signed in. Login now opens the account's **home** — derived from the check's first `step { goto }`
   (`RecipeMeta.home_url`, falls back to `login-url`). Signed in → you see the app; signed out → the
   app redirects you to sign in. acme→`/feed`, globex→`/dashboard`, initech→`/studio`.
   (`adapter-recipe/src/registry.rs::first_goto_url`; `auth.rs::login` prefers `home_url`.)

Interactive flow (`cli/src/auth_cmd.rs`): `login` opens the window, prints "press Return once you're
signed in…", blocks on stdin, then does ONE `recheck` (which now also clears `logging_in`).

Binaries rebuilt (release); daemon restarted on each. chrome-agent 172 tests, pacewright adapter 43
tests green.

## Open blocker — why we paused

`auth login acme-account` now works and Federico signed in cleanly. But an `acme/profile`
scrape of a guest **hit acme's authwall and revoked the session cookie** — verified:
the profile's `Default/Cookies` had no session cookie afterward and recheck flipped to signed out. So the
**chrome-agent-LAUNCHED** browser is still flagged even when signed in + headed + `--stealth`. Same
root cause as an identity provider blocking sign-in on the automation browser for `initech-account`.

See memory `acme-scrape-burns-session.md`.

## Next step when resumed — CDP-attach (same as initech-account)

Drive a REAL Chrome instead of launching one:
1. Launch a normal Chrome with a dedicated `--user-data-dir` + `--remote-debugging-port=9222`.
2. Human signs in by hand.
3. Drive via `chrome-agent --connect auto` (ATTACH, not launch).

pacewright account recipes would need an attach/`--connect` path (a per-account CDP endpoint) instead
of the launched persistent profile. Design TBD.

**Federico's hypothesis to test first (cheap):** a **1st-degree connection**'s profile may not wall.
Re-login, then scrape ONE guest he's actually connected to and watch whether the session survives.

## Still queued behind this (unchanged)
- Delete the self-mention acme test post (`acme/delete_post` ready; needs the post URL from
  the feed — Posts API finder is 403).
- Re-scrape Anton's URN + connection degree (blocked by the same authwall).
- Track B: paced guest URN scrape (blocked).
