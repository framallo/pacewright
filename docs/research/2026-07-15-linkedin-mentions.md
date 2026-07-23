# Research: LinkedIn @-mentions for pacewright (URN, not name)

**Date:** 2026-07-15 · **Author:** agent (for Federico) · **Status:** research + implementation plan

## Problem

`linkedin/publish_post` posts body text as-is. `@Anton Pleshivtsev` renders as plain text,
not a real tag. The recipe's own header already documents why: LinkedIn's composer
`@`-typeahead only fires on **genuine hardware key events**; synthetic CDP keystrokes do
not open the dropdown (verified 2026-07-14, two techniques). So the visible-composer path
is a dead end for mentions.

Federico's insight is the key that unlocks it: **LinkedIn identifies a mention by the
member's entity URN, not by the typed name.** A mention is an *annotation* — an
`{offset, length} → URN` span over the commentary text. If we can (1) resolve a person to
their URN and (2) submit the post with annotations, we never touch the typeahead at all.

## How LinkedIn represents a mention

Two surfaces, same underlying model (a person id + a text span):

### 1. Official Posts API (`POST /rest/posts`) — "little" text format
```json
{
  "author": "urn:li:person:<AUTHOR_ID>",
  "commentary": "Thanks @[Anton Pleshivtsev](urn:li:person:ACoAAB1cD2ef)",
  "visibility": "PUBLIC",
  "distribution": { "feedDistribution": "MAIN_FEED", "targetEntities": [], "thirdPartyDistributionChannels": [] },
  "lifecycleState": "PUBLISHED",
  "isReshareDisabledByAuthor": false
}
```
- Mention syntax is inline: **`@[Display Name](urn:li:person:<ID>)`** (orgs:
  `@[Name](urn:li:organization:<numericId>)`). No separate offset array — LinkedIn parses
  the `@[...](...)` markup and matches the bracket text against the entity's real name.
- **Display-name match is mandatory and case-sensitive.** If the bracket text doesn't
  match the member/org name it silently degrades to plain text. Members may match on ANY
  one name part (first, last, or full); orgs must match the full name.
- `<ID>` is the opaque alphanumeric member id (e.g. `ACoAAB…`), **not** a short numeric id.

### 2. Internal Voyager API (what the web app itself sends)
The composer serializes to a commentary + an **attributes/annotations array**:
```
"commentary": {
  "text": "Thanks Anton Pleshivtsev for coming on",
  "attributes": [
    { "start": 7, "length": 17, "entity": "urn:li:fsd_profile:ACoAAB1cD2ef" }
  ]
}
```
(exact key names vary by endpoint version — must be captured live, see §Empirical step).
Note the **same opaque id** `ACoAAB…` appears here as `urn:li:fsd_profile:<ID>`; the public
API calls the same person `urn:li:person:<ID>`. Only the URN namespace differs — resolve
the id once, build either form.

## Resolving name/vanity → URN (the hard part)

| Path | Endpoint | Works for | Blocker |
|---|---|---|---|
| Official People Typeahead | `GET /rest/peopleTypeahead?q=organizationFollowers&keywords=…&organization=…` | only members who **follow a company page you admin** | permission `r_organization_followers` is **restricted / approved developers only**; org-scoped, not personal feed |
| Official Vanity finder | `GET /rest/vanityUrl?q=vanityUrlAsOrganization&vanityUrl=…&organization=…` | vanity → URN, but **org-scoped** | same restriction; can't resolve an arbitrary guest for a personal post |
| **Profile-page scrape (session)** | the profile page JSON already contains `urn:li:fsd_profile:ACoAA…` | **any profile you can view** while signed in | none beyond being signed in — we already scrape it in `linkedin/profile` |
| Voyager mention typeahead (session) | `GET /voyager/api/graphql?…mentionsTypeahead…` with session cookies | any member | internal API, ToS caveat (see below) |

**Takeaway:** the official API can *format* a mention but **cannot resolve an arbitrary
guest's URN for a personal-feed post** (People Typeahead is org-follower-only and
restricted). The session-based path resolves it trivially because the URN is already on
the profile page we scrape.

## Two implementation options

### Option A — session/Voyager path *(recommended; fits pacewright's architecture)*
pacewright already drives Federico's **real signed-in** LinkedIn via chrome-agent, and the
recipe engine's **`eval` step runs async JS in the page context** — which means it can make
**credentialed same-origin `fetch()` calls** to `/voyager/api/…` (cookies attach
automatically). No new engine primitive, no OAuth app, no keystrokes.

Flow (all inside `eval` steps, so it reuses the trusted session):
1. **Resolve URN** — reuse `linkedin/profile`: from the guest's profile page, extract the
   `urn:li:fsd_profile:ACoAA…` (already present in the page's embedded JSON). Cache it on
   the guest's vault note (e.g. `linkedin_urn:`) so we resolve once per guest.
2. **Compute offsets** — in JS, find each mention's `{start, length}` in the final body
   text (plain names, no `@`), pairing each with its URN. Deterministic string math.
3. **Create the share** — `fetch('/voyager/api/contentcreation/…', {method:'POST',
   headers:{'csrf-token': <JSESSIONID>}, body: JSON.stringify({commentary, attributes})})`.
   LinkedIn requires the `csrf-token` header = the `JSESSIONID` cookie value.

**csrf-token gotcha:** `JSESSIONID` may be httpOnly (JS can't read `document.cookie`). If
so, read it via chrome-agent's browser-level CDP `Network.getCookies` and inject it into
the `eval` scope, OR add a tiny engine affordance to pass a cookie value into a step. Flag
for the empirical step.

### Option B — official Posts API (`w_member_social`)
Clean and ToS-blessed, `@[Name](urn:li:person:ID)` commentary. **But** blocked by URN
resolution: no public way to get an arbitrary guest's URN for a personal post (People
Typeahead is org-follower-only + restricted). Only viable if the guest follows a Density
Labs page you admin. Also needs an OAuth app + `w_member_social` + token refresh plumbing.
Keep as a fallback for org-page posts where the guest is a follower.

## Recommendation

Build **Option A**. It matches Federico's "URN not name" framing exactly, reuses the
signed-in session and the existing `eval` primitive, and resolves the guest URN from data
we already scrape. Concretely:

1. Add `linkedin/resolve_urn` (or extend `linkedin/profile`) — `eval` that returns
   `urn:li:fsd_profile:<ID>` for a profile URL. Persist to the guest note as `linkedin_urn`.
2. Add mention support to `linkedin/publish_post`: accept a `mentions` var
   `[{name, urn}]`; in an `eval` step compute offsets over the body and POST the share via
   the Voyager create endpoint (annotations array). Drop the "post via visible composer"
   step for mention posts.
3. Keep the plain-text composer path for posts with no mentions (already works).

## Decision update (2026-07-15): Postiz is out; official API is viable after all

**Postiz cannot tag people.** It supports LinkedIn **company** mentions only; the
person-mention request ([issue #944](https://github.com/gitroomhq/postiz-app/issues/944))
was **closed as not planned** (Aug 2025). Since every guest tag is a person, Postiz can't
do what we need. (It stays fine as a generic scheduler for no-mention or company-only posts.)

**`w_member_social` is self-service, not partner-gated** — this flips the recommendation.
It's granted by adding the **"Share on LinkedIn"** product to an app in the Developer
Portal (plus "Sign In with LinkedIn using OpenID Connect"); it's available to verified apps
by default, no Community Management partnership. So Option B's OAuth barrier is low.

And the People-Typeahead wall doesn't actually block us: **the only gap in the official API
is URN resolution, which we solve ourselves by scraping the guest's profile** (the `ACoAA…`
id → `urn:li:person:ACoAA…`). We never need the restricted typeahead.

**Revised recommendation: connect via the official Posts API (bearer token), resolve URNs
by profile scrape, let pacewright schedule the API call.** Extra win: a bearer-token API
call fired at go-live time runs **headless with no browser and no auth wall** — which fixes
the exact problem that forced `publish_post` to use LinkedIn's native UI scheduler. This is
cleaner than both the UI recipe and the Voyager path, and it's ToS-clean.

- **Path (recommended):** official `POST /rest/posts`, `w_member_social`, `@[Name](urn:li:person:ID)` markup, URN scraped per guest + cached on the note, pacewright holds the OAuth token (access ~60d / refresh ~1y) and queues the post.
- **Fallback:** the session/Voyager path below, only if `w_member_social` is ever denied.

## Empirical step (only for the Voyager fallback)

The exact Voyager create-share endpoint + payload key names must be captured live:
1. Open LinkedIn signed-in with devtools → Network.
2. Compose a post with one `@`-mention, click Post.
3. Inspect the `POST` (likely `/voyager/api/contentcreation/normShares` or a
   `/voyager/api/graphql` mutation): copy the URL, the `csrf-token` header, and the JSON
   body's commentary+attributes shape. That payload becomes the `eval` template.

Do this against the attached `:9222` Chrome (the 2026-07-14 working path), not the headless
stealth session (LinkedIn bounces that to /login).

## ToS / risk note

Automating the **internal Voyager API** is against LinkedIn's User Agreement and carries
account-risk if done at volume. Mitigations already baked into pacewright's thesis: real
first-party session, human pacing (daily caps + jitter), low volume (a few posts/week).
This is the same risk posture pacewright already accepts for `publish_post`; mentions add
one more internal call (resolve URN) and change the create call from UI to API. The
official API (Option B) is the only zero-ToS-risk route but can't resolve guest URNs for
personal posts. Federico's call on the tradeoff.

## Sources
- [Posts API — Mentions & little text format](https://learn.microsoft.com/en-us/linkedin/marketing/community-management/shares/posts-api?view=li-lms-2026-06)
- [People Typeahead API (org-follower-only, restricted)](https://learn.microsoft.com/en-us/linkedin/marketing/community-management/shares/posts-atmention-search-api?view=li-lms-2025-11)
- [UGC Post API (legacy annotations)](https://learn.microsoft.com/en-us/linkedin/compliance/integrations/shares/ugc-post-api)
- [Publora — practical LinkedIn mentions guide](https://github.com/publora/publora-api-docs/blob/main/docs/guides/linkedin-mentions.md)
