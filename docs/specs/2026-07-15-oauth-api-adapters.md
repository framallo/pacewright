# Spec: OAuth + API adapters for pacewright (LinkedIn first, YouTube next)

**Date:** 2026-07-15 · **Status:** spec → plan · **Repos:** `pacewright` (auth/secrets/DSL glue) + `chrome-agent` (recipe engine DSL step)

## Goal

Let pacewright call **first-party HTTP APIs with OAuth bearer tokens** — not just drive a
browser session. First target: post to LinkedIn **with real @-mentions** via the official
Posts API. Same machinery then serves the YouTube Data API. Three capabilities:

1. **Auth** — 3-legged OAuth (authorization-code) per provider; one-time human consent.
2. **Key storage** — securely persist `client_id/secret` + `access/refresh` tokens; auto-refresh.
3. **Usage** — a recipe DSL step that makes an authenticated API request using a stored token.

## What Federico enabled on the LinkedIn app (id `246620286`)

| Product | Tier | Gives us |
|---|---|---|
| **Sign In with LinkedIn using OpenID Connect** | Standard | scopes `openid profile email`; `GET /v2/userinfo` → `sub` = Federico's member id → author URN `urn:li:person:{sub}` |
| **Share on LinkedIn** | Default | scope `w_member_social`; `POST /rest/posts` to publish on his behalf |
| Advertising API | Development | not needed for posting (ignore for now) |

**OAuth scopes to request:** `openid profile email w_member_social`.
**Author URN:** resolved once from `/v2/userinfo` `sub`, cached in the token record.
**Guest URNs (for mentions):** NOT available from the API (no public resolver). Resolved by
the **session profile scrape** (`linkedin/profile`, see below) — the `ACoAA…` id becomes
`urn:li:person:ACoAA…`. This is why the profile parser must return the URN.

## Architecture

Two auth systems coexist, cleanly separated by purpose:

| | **Session auth (exists)** | **OAuth token auth (new)** |
|---|---|---|
| For | browser recipes (scrape, UI actions) | API recipes (post via token) |
| Store | persistent Chrome profile per account | token record per provider |
| Human step | `auth login` (headed sign-in) | `oauth login` (consent redirect) |
| Used by | `goto`/`click`/`eval`/in-page `request` | new native `api` DSL step |

### 1. Secret storage (`pacewright-core`)
New module `core/src/secrets.rs`. File `~/.pacewright/secrets.json`, mode **0600**, one record
per provider:
```json
{ "linkedin": {
    "client_id": "...", "client_secret": "...",
    "access_token": "...", "refresh_token": "...",
    "expires_at": 1789999999, "scope": "openid profile email w_member_social",
    "author_urn": "urn:li:person:ACoAA..." } }
```
API: `SecretStore::load/save`, `get(provider)`, `set_app(provider, id, secret)`,
`set_tokens(provider, …)`, `access_token_fresh(provider)` (returns a valid token, refreshing
if `expires_at` is within a 5-min skew). Refresh = `POST /oauth/v2/accessToken`
(`grant_type=refresh_token`). Determinism: clock via the existing `Clock` trait, no wall-clock in core.

### 2. OAuth flow (`pacewright-cli` + daemon)
`pacewright oauth login linkedin` (and `oauth status`, `oauth logout`):
1. Read `client_id/secret` from the secret store (prompt/`--client-id/--client-secret` to set first).
2. Start a localhost callback server on a fixed port; open the browser to
   `https://www.linkedin.com/oauth/v2/authorization?response_type=code&client_id=…&redirect_uri=http://localhost:<port>/callback&scope=openid%20profile%20email%20w_member_social&state=<nonce>`.
   **Redirect URI must be registered on the app's Auth tab** (doc step for Federico).
3. Catch `?code=…`, exchange at `POST /oauth/v2/accessToken` → access+refresh tokens.
4. `GET /v2/userinfo` → store `author_urn = urn:li:person:{sub}`.
5. Persist via `SecretStore`. Human-in-the-loop, mirrors `auth login`.

### 3. New recipe DSL step — native `api` request (`chrome-agent`)
The engine already has an **in-page** `Request` (session cookies). Add a sibling that runs a
**native reqwest call (no browser)** with a bearer header, so a token-only recipe needs no Chrome:
```kdl
step {
    api "postId" method="POST" url="https://api.linkedin.com/rest/posts" \
        bearer="{{linkedin_token}}" \
        header "X-Restli-Protocol-Version" "2.0.0" \
        header "LinkedIn-Version" "202606" \
        body=#"{ … "commentary": "{{commentary}}" … }"# \
        expect-status=201 capture="header:x-restli-id"
}
```
- New `Step::Api(ApiRequest)` in `model.rs` (mirrors `Request` + `bearer` + native flag).
- Engine: if a recipe's steps are all non-browser (`api`/`eval`-free-of-DOM), the runner **skips
  the Chrome launch** entirely (big change in `adapter-recipe/runner.rs` + `chrome-agent` recipe
  runtime — gate: "does any step need a page?"). Native request via `reqwest`, templated
  url/headers/body, `{{vars}}` filled by pacewright, capture body JSON path or a response header.
- `bearer="{{linkedin_token}}"` is filled by the adapter from `SecretStore::access_token_fresh`
  — the recipe never sees the raw secret in its file; pacewright injects it at run time.

### 4. `linkedin/profile` returns the URN (`chrome-agent` recipe — DO FIRST, unblocked)
Extend the profile `eval` to also return `urn` (the `urn:li:fsd_profile:ACoAA…`, disambiguated
against the vanity in the URL so "people also viewed" URNs don't win). Callers persist it to the
guest note as `linkedin_urn`. This is the guest-URN source for mentions and needs no OAuth.

### 5. Mention posting recipe (`linkedin/post_with_mentions`)
Token-only, no browser. Vars: `commentary_template`, `mentions=[{name,urn}]`, `author_urn`.
Build the little-format commentary — replace each `{{name}}` with `@[Name](urn:li:person:ID)`
(bracket text must case-sensitively match a name part) — then `api` POST `/rest/posts`. Returns
the share URN. pacewright schedules it at go-live; a bearer call fires headless with **no auth
wall** (fixes the `publish_post` scheduling problem).

## Phased plan (TDD, mirrors M1 style)

- **P0 — `linkedin/profile` URN extraction** (chrome-agent recipe + live test). ✅ DONE 2026-07-15, verified on 3 profiles.
- **P1 — secret store** (`core/secrets.rs` + tests; `now_ms`-injected expiry). ✅ DONE 2026-07-15 — 6 tests (roundtrip, 0600 perms, expiry/skew, refresh-window, app-first guard), core 44/44 green, clippy clean. `SecretStore` at `~/.pacewright/secrets.json`.
- **P2 — OAuth CLI** (`oauth login/status/logout`; callback server; userinfo → author_urn). *Needs Federico's client_id/secret + a registered `http://localhost:8765/callback` redirect URI.*
  - ✅ **P2a pure helpers DONE 2026-07-15** — `core/src/oauth.rs`, 9 tests (RFC-3986 pct encode/decode, `authorize_url`, loopback `parse_callback` incl. OAuth-error + missing-code, `TokenResponse`→`expires_at_ms`, `person_urn`). Core 53/53 green, clippy clean.
  - ✅ **P2b IO glue DONE 2026-07-15** — `reqwest` added to workspace (rustls); `crates/cli/src/oauth_cmd.rs` = one-shot loopback listener + token exchange + `/v2/userinfo` → author URN → `SecretStore`; `oauth login/status/logout` wired in `main.rs` (local flow, never touches the daemon socket). Added `SecretStore::clear_tokens` (TDD, keeps app creds; core 55/55). cli+core clippy `-D warnings` clean.
    - **Redirect URI is provider-namespaced**: `{base}/{provider}/callback`, e.g. `http://localhost:8765/linkedin/callback` (`oauth::callback_uri` + `oauth::redirect_port`, both TDD, core 58/58). One loopback port can serve multiple providers. `base` defaults to `http://localhost:8765` and is overridable via `--callback-base`.
    - **Credentials resolve flag → stored → interactive prompt**: `--client-id`/`--client-secret` still work; when absent, `oauth login` prompts (client id visible, secret masked with `*` per char via a `crossterm` raw-mode reader so a paste is visibly registered). App creds are saved 0600 **before** the browser step (survive an aborted consent); when already stored, login shows the stored client id and asks whether to change them.
    - ✅ **VERIFIED LIVE 2026-07-15** — Federico ran `oauth login linkedin`; `oauth status` → `linkedin  app set · token valid · scope: email,openid,profile,w_member_social · urn: urn:li:person:n8jDls5vvd`. Posting scope + author URN both in hand. **P2 complete.**
- ✅ **P3 — native `api` DSL step DONE 2026-07-15** (chrome-agent). `Step::Api(ApiRequest)` in `model.rs`; engine dispatch folds `bearer=` into an `Authorization` header, checks `expect-status` (mismatch → retryable), captures a JSON body `path=` or a response `header=` (e.g. `x-restli-id`). `RecipeBrowser::api_request` via `ureq`+rustls, **feature-gated `api`** so default static-musl release builds stay pure-Rust. TDD: parse + engine tests, chrome-agent 172 unit tests green, clippy clean. Live-verified browser-less against the GitHub API (body + header capture).
- ✅ **P4 — browser-optional runner DONE 2026-07-15** (chrome-agent). `Recipe::needs_browser()` (false iff every step is `api`); `browser::NativeBrowser` (no-op inject, native `api_request`, every page verb errors); `recipe::run_recipe_native`; `run.rs` early-returns for api-only recipes → **no Chrome launch, no session**. TDD green. Live-verified: `recipe run` of an api-only recipe returns data with no browser.
- ✅ **P5 — `linkedin/post_with_mentions` DONE 2026-07-15**. Recipe at `~/.pacewright/recipes/linkedin-post-with-mentions.kdl` (token-only, api-only → browser-less; POSTs `/rest/posts`, commentary carries `@[Name](urn:li:person:…)`, captures the created share URN from `x-restli-id`). **Adapter token injection**: `adapter-recipe/runner.rs` `inject_oauth_vars` merges a valid `token`+`author_urn` from the 0600 secret store into the recipe's vars just before spawn, for any recipe declaring a `token` var (TDD, 39 adapter tests green). **End-to-end proven non-destructively**: `linkedin/whoami` (GET `/v2/userinfo`) via the daemon returned `sub=n8jDls5vvd` — token injection → api step → live LinkedIn all working. **The actual post is gated on Federico's explicit go** (outward-facing publish); commentary needs LinkedIn little-format escaping validated on a first real post.
- **P6 — YouTube**: same store/CLI/`api` step; add `youtube` provider (Google OAuth device/loopback), Data API `videos.update`/`insert` recipes (replaces the Python youtube-cli).

## What's needed from Federico
- App **client_id + client_secret** (LinkedIn app 246620286 → Auth tab). `oauth login` prompts for these (or pass `--client-id/--client-secret`); stored 0600 after first run.
- Register redirect URI `http://localhost:8765/linkedin/callback` on that Auth tab (provider-namespaced; override the base with `--callback-base` if you register a different one).
- One-time `oauth login linkedin` consent click (P2).

## Risk / notes
- Official API is **ToS-clean** (unlike the Voyager fallback). Mentions need the guest URN, which
  we self-resolve via P0 scrape — no restricted People-Typeahead needed.
- Secrets at rest: `~/.pacewright/secrets.json` 0600 on Federico's dedicated agent Mac. Good enough
  for single-user; note as a known limitation (no OS-keychain yet).
- Token lifetimes: LinkedIn access ~60d, refresh ~1y → refresh-on-use covers normal cadence.

See [[research/2026-07-15-linkedin-mentions]] for the mention-format research this builds on.
