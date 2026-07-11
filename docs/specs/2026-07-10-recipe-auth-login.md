# Recipe auth & login — account-recipes, persistent sessions, `pcw auth`

Status: **design** (2026-07-10). Builds on the recipe engine (`2026-07-08`), the declarative
scheduler (`2026-07-09`), and the web dashboard (`2026-07-09`). Motivated by a concrete failure:
running `riverside/generate_magic_clips` through pacewright 403'd because `chrome-agent
--copy-cookies` copies a **static snapshot** of the everyday Chrome profile, and Riverside's access
token has a **~9.5-minute TTL** — the snapshot is stale by the time the recipe runs. Google/YouTube
was worse: signed out entirely. Recipes need a **durable, self-refreshing logged-in session** they
can reuse, plus a way for the operator to **establish and inspect** those sessions.

---

## 1. The model — a login *is* a recipe

Login procedures live as their own recipes under **`~/.pacewright/recipes/accounts/<account>.kdl`**.
Each is the login for exactly one **account** (the account name = the file stem, e.g.
`accounts/prevetted-riverside.kdl` → account `prevetted-riverside`). An account recipe declares:

An account recipe is a **normal recipe whose steps ARE the signed-in check**, plus flat
pacewright-consumed nodes (`login-url`, optional `login-field`). chrome-agent runs it like any
recipe and **ignores** the extra nodes (forward-compat, `_ => {}`), so **chrome-agent needs no
changes** — the whole feature is pacewright-side.

```kdl
recipe "accounts/prevetted-riverside" {
    description "Login for the prevetted.fm Riverside account."
    login-url "https://riverside.com/login"        // where the human signs in (pacewright-consumed)

    // The recipe's steps ARE the signed-in check, run headless in the account profile. NOTE:
    // chrome-agent `expect` is a TRIPWIRE — it FAILS (on-fail class) when its condition is TRUE.
    // So the check trips on the SIGNED-OUT signal: visiting /dashboard while signed out redirects to
    // /login; signed in it stays on /dashboard, /login never matches, expect passes → recipe succeeds.
    step { goto "https://riverside.com/dashboard" }
    step {
        expect on-fail="terminal" message="signed out" {
            settled-url-matches #"riverside\.com/login"#
        }
    }

    // Optional password-manager / credential-tool fill (values never pass through the agent):
    // login-field "username" { locator role="textbox" name="Email" }
    // login-field "password" secret=#true { locator role="textbox" name="Password" }
}
```

The account name is the recipe name after the `accounts/` prefix (`accounts/prevetted-riverside` →
account `prevetted-riverside`, profile `prevetted-riverside`). Account recipes are **not** registered
as runnable task adapters — the registry routes them to the auth subsystem instead.

A normal recipe **references** its account by evolving the existing `auth` flag:

```kdl
recipe "riverside/generate_magic_clips" {
    auth account="prevetted-riverside"   // was: auth #true
    ...
}
```

- `auth account="X"` → this recipe needs the session established by `accounts/X`.
- `auth #true` (no account) stays valid = "needs a session, use the default `pacewright` profile"
  (back-compat; today's behavior).
- No `auth` → public recipe (unchanged).

Recipes sharing an account share one session. This keeps login recipe-native ("expand the recipes
to allow a login procedure") and lets one login serve many recipes.

## 2. Session storage — a persistent per-account Chrome profile

Each account maps to a **persistent chrome-agent browser profile named after it**
(`--browser prevetted-riverside`; cookies in `~/.chrome-agent/browsers/prevetted-riverside/`).

- `pcw auth login <account>` opens that profile **headed** at `login.url`; the operator signs in by
  hand; cookies persist in the profile.
- An authed recipe (`auth account="X"`) runs in that **same** profile with `--browser X` and **no
  `--copy-cookies`**. Because it is the same profile the human logged into, the site refreshes its
  own short-lived tokens when the profile runs. This is the staleness fix: no snapshot, no copy.

This replaces `--copy-cookies` for account-bound recipes. `auth #true` (no account) keeps using the
shared `pacewright` profile; `--copy-cookies` remains only as the explicit global override.

## 3. The daemon owns auth; clients are thin

Auth state and orchestration live in the **daemon** (mirrors schedules/limits). CLI, TUI, and web
are thin clients over new RPCs on the existing `handle_request` dispatch.

**State.** The daemon loads `accounts/` via the `RecipeRegistry`, maps each normal recipe's
`auth account` to its account, and keeps a **cached status** per account (`signed_in:
true|false|unknown`, `last_checked_ms`). Status is **not** recomputed on every snapshot (a headless
check per account per second is far too expensive) — it is refreshed on demand and while a login is
in progress.

**RPCs** (additive to `proto::Request`, each returns the usual `Response::Ok(Value)`):

- `AuthList` → `[{ account, login_url, recipes: [names…], signed_in, last_checked, has_check }]`.
  `has_check=false` → `signed_in` is `unknown` (we can't tell without a declared `check`).
- `AuthRecheck { account }` → run the account's `login.check` headless in its profile; update the
  cache; echo the new status. (`account` omitted → recheck all.)
- `AuthLogin { account }` → the daemon spawns a **headed** `chrome-agent --browser <account>
  --headed goto <login.url>` (a real window on the operator's machine), marks the account
  `logging_in`, and returns immediately. While `logging_in`, the daemon polls `login.check` every
  few seconds (bounded, e.g. 5 min); when it passes, status flips to `signed_in` and polling stops.
- `AuthLoginAll` → open, one at a time, each account that is `signed_in=false|unknown`.

The 1 s web snapshot already in place carries the **cached** auth status, so the Accounts pane
updates live during a login without any new push channel.

## 4. Clients

**CLI** (`pcw auth …`, thin over the RPCs):
- `pcw auth` / `pcw auth status` — table: account · signed-in/out/unknown · recipes using it · last
  checked.
- `pcw auth login [account]` — trigger `AuthLogin`; print "opened a login window for <account> — sign
  in, then it'll go green". `--all` → `AuthLoginAll`.
- `pcw auth recheck [account]` — force a status refresh.

**Web dashboard** — a 4th pane **Accounts** (Feed / Schedule / Limits / **Accounts**):
- One row per account: a signed-in/out/unknown **pill**, the recipes using it, `last checked`.
- **Log in** button → `AuthLogin`; the daemon pops the headed Chrome on the machine; the row shows
  "logging in…" and flips to green when the check passes (via the live snapshot).
- **Recheck** per row; **Log in all** header action for the signed-out ones.

**TUI** — an Accounts pane at parity (tab-cycle Feed/Schedule/Limits/Accounts; `l` = login on the
selected row, `r` = recheck). Required in v1 alongside web + CLI.

## 5. Signed-in detection — the `login.check`

"Which ones are signed out (if we know)" = the account recipe's steps. The daemon runs them
**headless in the account profile**: a `goto` + an `expect`. Because chrome-agent's `expect` is a
**tripwire** (it fails when its condition holds), the check trips on the *signed-out* signal — a
`settled-url-matches` on the **login** URL (you got bounced to `/login`), or a `visible` locator that
only appears on the login page. Recipe succeeds (no trip) → `signed_in=true`; the classed `expect`
failure → `false`. An account recipe with **no** check → `signed_in=unknown` (surfaced honestly; the
operator can still `Log in`).

## 6. Password-manager fill (optional follow-on)

`login.field` entries name the username/password inputs by locator and mark secrets. During `pcw
auth login`, a credential-request tool / the OS password manager fills them **directly** in the
headed window — values never pass through the agent, honoring the credential-safety rule. Pure
interactive login works with no `field` declared; PM-fill is additive and ships after the
interactive path.

## 7. Changes

**chrome-agent** (`~/work/chrome-agent`, branch `feat/recipe-engine`): **no changes.** `auth
account="…"`, `login-url`, and `login-field` are unknown nodes it already ignores; the account
recipe's steps run as an ordinary recipe; `--headed` and persistent `--browser <name>` already exist.

**pacewright**:
- `adapter-recipe`: `RecipeMeta` gains `account: Option<String>` (from `auth account="…"`) and, for
  account recipes (name prefix `accounts/`), the parsed `login_url` + `login_fields`. Registry
  indexes `accounts/` separately and does **not** register an `accounts` task adapter. The
  runner/adapter runs an authed recipe with `--browser <account>` (no copy) instead of the shared
  profile.
- `core`/`daemon`: an `auth` module owning the status cache + login orchestration; `proto` gains the
  four RPCs; `server.rs` dispatches them; the daemon loads `accounts/` at boot.
- `cli`: `pcw auth` subcommands. `web/index.html`: the Accounts pane + `snapshot()` includes cached
  auth status. (Optional) `tui.rs`: the Accounts pane.

## 8. Testing

- **model/parse:** `auth account="…"` parses; `login { url; check }` parses/validates; account name
  derives from the path; a normal recipe resolves to its account.
- **status cache:** unknown-without-check; check pass/fail maps to signed_in true/false; recheck
  updates `last_checked`.
- **orchestration:** `AuthLogin` marks `logging_in` and spawns the headed command (mocked runner);
  the bounded poll flips to signed_in on a passing check and gives up after the deadline.
- **dispatch:** the four RPCs over an in-memory server (mirrors the schedule/limit dispatch tests).
- **snapshot:** the web snapshot includes `accounts` with cached status.
- Gates unchanged: `cargo test --workspace`, `clippy -D warnings`; only new files rustfmt-clean (no
  blanket-fmt of hand-formatted files).

## 9. Sequencing

1. **pacewright adapter-recipe**: `RecipeMeta.account` (from `auth account="…"`) + `login_url`/
   `login_fields`; registry indexes `accounts/` and skips the `accounts` task adapter; run authed
   recipes with `--browser <account>` (no copy). (chrome-agent unchanged.)
3. **daemon + proto**: the auth status cache + orchestration + the four RPCs + boot load.
4. **CLI**: `pcw auth status/login/recheck`.
5. **Web**: the Accounts pane + snapshot wiring. **TUI**: the Accounts pane (tab-cycle + `l`/`r`).
6. Gate + docs (README auth section, HANDOFF), branch, merge `--no-ff`.

## 10. Non-goals / deferred

- Storing credentials in pacewright. Interactive + PM-fill only; no plaintext secrets near recipes.
- Auto-re-login when a session lapses mid-run (the recipe fails `Retryable`; the operator re-logs
  in). A future "on auth-fail, prompt login" is a later nicety.
- Non-cookie auth transfer between the everyday Chrome and the account profile. The operator logs
  into the account profile directly; we don't try to import the everyday session.
