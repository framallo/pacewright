# Pacewright ↔ PreVetted Riverside publishing pipeline — plan

Goal (Federico, 2026-07-14): drive the whole Riverside publishing pipeline from pacewright, wired into the
podcast pipeline checklist (`prevetted_podcast/_templates/pipeline.md`). Steps:
export all magic shorts → shorts to YouTube Unlisted → long to YouTube Unlisted → to Spotify → update wiki video pages.

## Current state (verified 2026-07-14)
- `prevetted-riverside` account **holds a signed-in session headless** (auth recheck = "signed in"), so headless
  browser recipes against Riverside DO work (unlike LinkedIn, which bounces to /login).
- Recipes live as KDL in `~/.pacewright/recipes/riverside-*.kdl`; two mechanisms:
  - declarative `request` steps (REST API) — e.g. `generate_magic_clips` POSTs `/api/v4/projects/<id>/ai-clips`.
  - the compiled **`youtube_export`** verb — drives Riverside's Share→YouTube modal (WebSocket publish); modes
    `render` (Export video) and `publish` (Share→YouTube Unlisted, title prefix). **Clip-focused**: iterates
    Made-for-You clips of a project.

## Architecture change (2026-07-14): no more compiled UI verbs — declarative KDL only
Per Federico: **move `youtube_export` out of Rust into KDL; expand the engine to support tabs.** Done. The
chrome-agent recipe engine gained two primitives, so imperative UI flows are now recipes, not compiled verbs:
- **`eval` step** — `step { eval "<key>" js=#"…async in-page routine…"# retry-if-positive="<path>" }`. Runs an
  async in-page JS routine (CDP `Runtime.evaluate` awaitPromise), captures its JSON under `<key>`. If a dotted
  path into the result is a number > 0 the step fails **retryable** (the daemon polls again) — this is how the
  YouTube flow keeps re-running until `pending == 0`.
- **`tab` step** — `step { tab "follow" url-contains="…" }` / `tab "back"` / `tab "close"`. Follows a
  newly-opened tab (by `opener_id`, else url substring, else the lone new page), injects the `__pw` runtime, and
  routes subsequent steps there. **This is what makes the Spotify flow (opens a new tab) a plain recipe.**
- The compiled `youtube_export` verb is **removed**; `riverside/publish_clips` + `riverside/render_clips` are now
  pure KDL (goto + eval). Source of truth: `tmp/gen_recipes.py` in the paperclip job dir → generates the two
  `.kdl` (KDL v6.7.1 rejects literal newlines in strings, so the JS is embedded via `json.dumps`).

## Step → recipe map
| Pipeline step | Recipe | Status |
|---|---|---|
| Export all magic shorts | `riverside/generate_magic_clips` (REST) → `riverside/render_clips` (KDL: goto + eval) | ✅ done (KDL) |
| Shorts → YouTube Unlisted | `riverside/publish_clips` (KDL: goto + eval, localStorage idempotency, retry-if pending) | ✅ done (KDL) |
| Long → YouTube Unlisted | `riverside/publish_long` — clone of publish_clips targeting the long Magic-episode export tab (verify the Share control + title field are the same on the long export; likely a one-line selector tweak) | 🔨 KDL (small) |
| Long → Spotify (scheduled) | `riverside/share_spotify` — KDL recipe: eval opens Magic-ep Share → `click role=button "See all"` → `click role=button "Spotify"` (opens new tab) → `tab "follow" url-contains="spotify.com"` → eval-drive the Creators wizard | ✅ authored + registered 2026-07-15. Riverside side VERIFIED live; Creators wizard eval gated (`do_schedule=false`) + UNVERIFIED, needs one live gated run to finalize calendar/time selectors. Popup fixed via `--disable-popup-blocking`. |
| Shorts → Spotify | `riverside/share_shorts_spotify` — same pattern, clip mode | 🔨 KDL |
| Update wiki video pages | pipeline step: agent writes `videos/` notes with public URLs (not a pacewright recipe) | 🔨 agent |

## Build order (remaining)
1. **`riverside/publish_long`** — likely publish_clips with the Share/title selectors pointed at the long export.
   Fast once we confirm the long-export Share modal matches the clip one.
2. **`riverside/share_spotify`** (the real remaining work, now a KDL recipe not a Rust verb): walk the live
   Share → See all → Spotify → Creators "Publish from Riverside" flow ONCE (non-destructively, stopping before
   Schedule) to capture the wizard selectors, then author the recipe: `click` Share/See-all/Spotify →
   `tab "follow"` → `eval` the Details step (toggle HTML desc, paste title+desc from the episode note) → set
   thumbnail → Next → Review → **Schedule to the publish date** (gated behind explicit go per episode).
3. Wire `pd_publish_due`/pipeline to `pacewright add riverside <action>` at the right step.

## Risks / notes
- Spotify Creators wizard is fragile (calendar gotchas, HTML-toggle-or-it-mangles-text) — the `eval` routine must
  mirror the proven modal-dance style used in the YouTube JS. Real test run needed, gated before Schedule.
- Live-publish steps are outward-facing — gate behind explicit go per episode until proven.
- Titles/descriptions come from the episode note (EP134 SEO template); pass as recipe vars.
- Deploy: `chrome-agent` release binary is symlinked at `/opt/homebrew/bin/chrome-agent` → `cargo build --release`
  deploys it; then `pacewright recipe reload`. Recipe steps `eval`/`tab` require this new binary.
