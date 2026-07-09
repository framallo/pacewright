# KDL recipe engine — built into the chrome-agent fork, PR'd upstream

Status: **approved** (2026-07-08). **Implemented** (2026-07-09): steps 1–9 landed — the
chrome-agent `recipe` engine (+ write verbs & cookie-auth `request`, beyond the original
read-only v1) on `feat/recipe-engine` (held), and the pacewright `adapter-recipe` crate
(`RecipeAdapter`/`RecipeRegistry`/`CliRecipeRunner`) + `pcw recipe job` vault job-runner,
with `adapter-linkedin` deleted. Deferred: the daemon jobs sweep (§7a — only one-shot
`pcw recipe job` shipped) and the automated Claude repair loop (subsystem E).
Supersedes placement decisions in `2026-07-08-recipe-format-engine.md` (§3: the engine no
longer lives in a `pacewright-recipe` crate) and narrows `2026-07-07-chrome-agent-fork-lib.md`
(the full `Session`/`Page` lib facade is **not** required for this path).

This spec implements **subsystem A+B** (the KDL format + generic engine) **inside the
chrome-agent fork** as a first-class `recipe` subcommand, and consumes it from pacewright via
the CLI. The recipe feature is self-contained (no pacewright dependency), so it is prepared as
a **PR to upstream `sderosiaux/chrome-agent`**.

---

## 1. Why this placement (the delta from prior specs)

The recipe engine needs exactly one capability: **a browser to drive** (navigate, evaluate JS,
inject a script, screenshot). It needs *nothing* from pacewright — not pacing, not
`Clock`/`Rng`, not the `Adapter` trait. chrome-agent **is** that browser driver. So the engine
belongs in chrome-agent, offered upstream as a general capability, not buried in pacewright.

This also dissolves the blocking limitation the format spec flagged (§8): pacewright's
`CliBrowser` re-spawns chrome-agent per verb and **loses the injected locator runtime between
calls**. Running an entire recipe inside **one** `chrome-agent recipe run` process means
`locators.js` is injected once and persists for the whole recipe. Consequence: **pacewright
needs no lib facade** — it keeps calling chrome-agent through the CLI it already uses.

| Prior spec assumed | This spec |
|---|---|
| Engine in `pacewright-recipe` crate over `BrowserHandle` | Engine in `chrome-agent/src/recipe/`, driven by `&CdpClient` |
| `knuffel` for typed KDL deser | `kdl` crate v6 only; hand-written typed extraction (one fewer dep, no KDL-version coupling, AST retained for subsystem E) |
| Full `Session`/`Page` lib facade first | Not needed; a `recipe` CLI verb reuses chrome-agent's existing internals |
| pacewright links chrome-agent as a crate | pacewright shells `chrome-agent recipe run …` (existing `CliBrowser` pattern) |

Everything else from `2026-07-08-recipe-format-engine.md` stands: the KDL schema (§4), the
robustness ladder (§4a), locators-as-data with a shipped runtime (§5), the non-Turing language
scope (§7), authentication-is-not-a-recipe-concern (§1), and the migration that deletes the
hand-written adapter (§9).

---

## 2. Goal / non-goals

**Goal.**
1. A `recipe` subcommand in the chrome-agent fork: `recipe run <file.kdl> [--var k=v …] [--log]`
   and `recipe check <file.kdl>`.
2. A generic **engine** (`src/recipe/`) that runs a recipe over a small `RecipeBrowser` trait,
   with Playwright-style locator resolution via a shipped `locators.js`.
3. **Parameters, JSON output, and a run log** — the three properties called out for the
   example (§6): vars bound with `--var`, the result as JSON on **stdout**, a step-by-step
   trace to **stderr** (gated by `--log`).
4. An **optional, embedded Claude repair hook** (§5a): a recipe may carry a `repair { prompt … }`
   block and per-`extract` **expectation** hints; when a run doesn't produce the expected result,
   `recipe run --repair` assembles a repair context (failing step + page snapshot + the author's
   prompt) for Claude. This spec *defines the format + trigger + context*; the automated
   repair **loop** stays subsystem E (deferred).
5. **Output sinks — the engine writes files** (§5b): a recipe may declare `output "markdown"`
   / `output "json"` blocks that render the result (a minimal, non-Turing template) and write it
   to a templated path. Generic capability; the Obsidian vault path/template live in recipe+var
   *data*, so the engine stays site-agnostic and upstream-palatable.
6. A committable **Hacker News example recipe** + a `file://` fixture test proving the whole
   path deterministically.
7. In pacewright: a **vault job-runner** (§7) that treats an Obsidian note's YAML frontmatter as
   a recipe *job* — resolves the named recipe, binds frontmatter→vars, runs it **paced by the
   daemon**, and lets the engine write the markdown/JSON note into the vault. Plus the thin
   `RecipeAdapter` that maps result→`Value` and exit→`AdapterError`; **delete `crates/adapter-linkedin`**.
8. Prepare the fork's `recipe` feature as an upstream **PR** (branch + PR body; push/open gated
   on explicit go-ahead).

**Non-goals (unchanged from the format spec).** The awesome-recipes repo (C), the known-state
golden harness (D). The **automated** LLM repair loop (E) — call Claude, apply the fix, re-verify
— stays deferred; this spec ships only its *format hook + trigger + context bundle* (§5a), so a
recipe is repair-ready and an operator has a manual one-command path, without the closed loop.
Also out of scope here: `click`/`fill` write verbs — both real recipes (HN, LinkedIn scrape) are
**read-only**, so v1 ships `goto`/`extract`/`expect`/`wait`/`screenshot` and defers write verbs
to a follow-up (§10).

---

## 3. Architecture (in the fork)

```
chrome-agent/
  src/
    recipe/
      mod.rs        # pub fn run_cli(...) — CLI glue: parse args, drive engine, emit JSON/log
      model.rs      # typed Recipe/Step/Locator + parse from kdl::KdlDocument (hand-written)
      engine.rs     # Engine<B: RecipeBrowser>: runs a Recipe, accumulates the result map
      locator.rs    # Locator -> serde_json::Value (the spec passed to __pw.resolve/extract)
      browser.rs    # RecipeBrowser trait + CdpBrowser (real, wraps &CdpClient) + FakeBrowser (tests)
      runtime/
        locators.js # the injected locator runtime (include_str!)
    commands/mod.rs # + (nothing; recipe is its own module, not a commands/ leaf)
    cli.rs          # + Recipe { #[command(subcommand)] action: RecipeAction }
    run.rs          # + dispatch arm: connect page client, call recipe::run_cli
  tests/
    recipe_tests.rs # file:// fixture end-to-end (mirrors extract_tests.rs)
    fixtures/
      recipe_hn.html      # a frozen HN-like page (or reuse extract_hn_like.html)
  examples/
    recipes/
      hackernews.kdl      # the committable example (§6)
```

- **`RecipeBrowser`** is a tiny trait — the engine's only coupling to the browser. Native async
  fns (edition 2024 / Rust ≥1.75), used via a **generic bound** `Engine<B: RecipeBrowser>` so
  no `async-trait`/`dyn` and **no new dependency**:
  ```rust
  pub trait RecipeBrowser {
      async fn inject_init_script(&self, js: &str) -> Result<(), BoxError>;
      async fn goto(&self, url: &str) -> Result<NavInfo, BoxError>;   // {url,title}
      async fn eval(&self, js: &str) -> Result<serde_json::Value, BoxError>;
      async fn screenshot(&self) -> Result<Vec<u8>, BoxError>;
  }
  ```
  `CdpBrowser<'a>(&'a CdpClient)` implements it by delegating to the existing primitives:
  `inject_init_script` → `client.send("Page.addScriptToEvaluateOnNewDocument", {source})`
  (identical to `setup::apply_stealth`); `goto` → `commands::goto::run`; `eval` →
  `commands::eval::run_raw`; `screenshot` → the existing screenshot path (bytes). `FakeBrowser`
  is a scriptable double for unit tests (canned `eval` responses, records calls).
- **`model.rs`** parses `kdl::KdlDocument` into a typed `Recipe` by walking nodes (no derive
  macro). The raw `KdlDocument` is retained on `Recipe` so subsystem E can later rewrite a
  single node format-preservingly.
- **New dependency:** `kdl = "6"` (Apache-2.0, pure Rust → keeps the single-static-binary
  property). Justified in the PR body; it is the only added dep.

---

## 4. The `recipe` subcommand (CLI)

```
chrome-agent recipe run <FILE> [--var NAME=VALUE]... [--log] [--timeout <secs>]
chrome-agent recipe check <FILE>
```

- `run` — load+validate the recipe, bind vars, execute steps against the page, print the result
  **as JSON to stdout** (honoring the global `--json` shape `{"ok":true,"result":{…}}` for
  parity with other verbs; human mode pretty-prints the map).
- `check` — parse + validate only (required vars declared, `{{…}}` all resolvable given
  declared vars, `on-fail` classes valid, locators well-formed). No browser. Exit 0/1.
- `--var NAME=VALUE` — repeatable; binds a declared `var`. Unknown var name ⇒ error. Missing
  required var (no `--var`, no `default`) ⇒ error.
- `--log` — emit a **step-by-step trace to stderr** (see §5). stdout stays pure JSON so it
  pipes cleanly; the log never contaminates the machine-readable result.

`cli.rs` gains `Recipe { action: RecipeAction }` with `RecipeAction::{Run{…}, Check{…}}`
(nested subcommand, mirroring `Daemon`). `run.rs`: `recipe check` returns early (no browser,
like `Status`); `recipe run` is added to the `needs_existing` browser-bootstrap exemption
(alongside `Goto`) because it navigates, then dispatches through the page `client` to
`recipe::run_cli(&client, …)`.

---

## 5. Execution model, logging, and output

`Engine::run(recipe, vars, &browser) -> Result<serde_json::Value, RecipeError>`:

1. **Bind + validate** vars (required present, defaults applied, `{{…}}` all known) — load-time
   errors, before any navigation.
2. **Inject** `locators.js` once (`inject_init_script`) so `__pw` exists on every document,
   surviving navigations.
3. **Per step, in order**, each auto-waiting where it targets an element:
   - `goto "<url>"` → `browser.goto(interpolated_url)`.
   - `extract "<key>" many=#bool { locator … }` → poll `__pw.resolve(spec)` (auto-wait, ~250ms,
     default 10s) then `__pw.extract(spec, many)`; write under `<key>`. Timeout ⇒ `Retryable`.
   - `expect on-fail="<class>" message="…" { <condition> }` → evaluate the tripwire against
     **settled** state; if it holds, stop with the mapped error class + message.
   - `wait { locator … timeout-ms=<n> }` → explicit auto-wait barrier.
   - `screenshot "<key>"` → `browser.screenshot()`, base64 under `<key>`.
4. **Return** the accumulated result map as `Ok(Value)`.

**Logging (`--log`).** Each step emits one structured line to **stderr** before/after it runs,
e.g. `step 1 goto url=https://news.ycombinator.com/ → 200 "Hacker News"` /
`step 3 extract stories many=true → 30 items` / `step 2 expect settled-url !~ /login/ → ok`.
On failure the log shows the failing step and the error class. This is the operator's and (later)
subsystem E's window into *where* a recipe broke. Without `--log`, only the JSON result (run) or
nothing (check) is printed.

**Error classes.** `RecipeError` carries a class ∈ `terminal | retryable | rate_limited`
(from `on-fail`, or engine-assigned: locator timeout→retryable, malformed recipe/var→terminal).
The CLI maps class → process exit + a stable JSON error shape so pacewright's adapter can
recover the class from the child process (§7).

---

## 5a. The optional repair hook (Claude-assisted)

A recipe rots when a site changes its markup. The recipe format therefore carries **its own
instructions for fixing itself** — an optional, author-written natural-language prompt that
primes Claude to analyze the change and repair the recipe. This is the *format + trigger +
context* for subsystem E; the automated loop that calls Claude and re-verifies is E (deferred).
What ships here makes a recipe **repair-ready** and gives an operator a one-command manual path.

### What "expected result" means (the trigger)

To know a run "didn't get the expected result," the recipe must be able to declare what it
expects. Two mechanisms, both optional and both *declarative data* (no expressions):

- **Extraction cardinality** on `extract`: `expect-count=<n>` (exactly n), `expect-min=<n>`
  (at least n, default 1 for a non-`many` extract — an empty single extract is already
  unexpected), `expect-max=<n>`. A `many` extract with `expect-min=1` says "this list must not
  be empty." Violation ⇒ the run is flagged **unexpected** (distinct from a hard failure).
- **Hard failure**: a locator auto-wait timeout or an `expect` tripwire firing — already an
  `AdapterError`. These also trigger repair.

An "unexpected" run (cardinality violated) does **not** by itself fail the run — it returns the
(under-delivered) result *and* sets an `unexpected` marker in the JSON envelope
(`{"ok":true,"unexpected":["stories: expected ≥1, got 0"],"result":{…}}`), so callers can decide
whether to accept, retry, or repair. Under `--repair`, the marker triggers the repair path.

### The embedded prompt (schema)

One optional top-level node:

```kdl
repair {
    prompt #"""
    Prefer semantic locators (role/text) over css — this site build-hashes its class names.
    If a field is empty, climb the robustness ladder: semantic → relative (after/near an
    anchor) → positional (nth/within) → css. Never introduce executable JS; only change
    locator nodes. Keep every step's `key` and order intact.
    """#
}
```

The prompt is guidance *specific to this recipe/site*; it is concatenated with a fixed,
engine-owned system instruction (below), never replaces it. It is inert data — it can only be
*read* into a prompt, never executed.

### `recipe run --repair` (the optional step)

When `--repair` is set **and** a run is unexpected/failed, the CLI does **not** silently return.
It assembles and prints (to stderr, or to `--repair-out <file>`) a **repair context** for Claude:

1. the **failing recipe**, verbatim (the retained format-preserving KDL AST, so a fix is a
   surgical single-node edit);
2. **what went wrong**: the failing step index/verb, the field key, and `expected vs got`
   (e.g. `stories: expected ≥1, got 0`) or the error class + message;
3. a **page snapshot** at the failure point — chrome-agent's own `inspect` accessibility tree
   (+ the relevant DOM subtree) so Claude sees the *current* structure to re-anchor against;
4. the recipe's `repair.prompt` (if any);
5. a fixed **system instruction** (engine-owned, versioned): *"You repair declarative KDL
   browser recipes. Output only a corrected recipe. Change locator nodes only; preserve step
   keys, order, and the non-Turing schema; add no executable JS; prefer the highest robustness
   tier that resolves."*

In this spec `--repair` **emits** that context (making manual repair with Claude a one-liner) and
exits non-zero. Subsystem E later closes the loop: feed the context to Claude, apply the returned
recipe, re-run against the known-state fixture (subsystem D), and accept only if the expectation
now holds. The interface defined here — `repair.prompt`, the `expect-*` trigger, and the
context bundle — is exactly what E consumes, so E adds no new format surface.

---

## 5b. Output sinks — the engine writes files

By default `recipe run` prints the result JSON to stdout. A recipe may **additionally** declare
`output` blocks; when present, the engine renders and **writes files** after the steps complete.
This is a generic "render the result to a file" capability — the engine never knows about
Obsidian; the vault path and the note shape are recipe/var *data*.

```kdl
// After the extract steps:
output "json" path="{{ out_dir }}/{{ slug }}.json"

output "markdown" path="{{ vault }}/wiki/guests/{{ slug }}.md" {
    template #"""
    ---
    type: guest
    name: "{{ name }}"
    company: "{{ company }}"
    linkedin: "{{ url }}"
    updated: {{ now }}
    ---

    # {{ name }}
    {{ headline }} — {{ location }}

    ## Recent posts
    {{#posts}}- {{.}}
    {{/posts}}
    """#
}
```

- **`output "<format>" path="<templated path>"`** — `format` ∈ `json | markdown`. `path` is
  interpolated with vars **and** extracted result keys (`{{ slug }}`, `{{ name }}`), plus a
  built-in `{{ now }}` (RFC-3339, from the engine's real-time edge — this is browser-side I/O,
  outside pacewright's Clock invariant). 0+ blocks; e.g. write both a JSON record and a md note.
- **`json`** writes `serde_json::to_string_pretty(result)` — no template.
- **`markdown`** requires a `template` child (raw string). Rendering is a **minimal, non-Turing
  templater** (hand-rolled, no dep): `{{ key }}` scalar substitution over vars+result, and
  Mustache-style **sections** `{{#listkey}} … {{.}} … {{/listkey}}` to repeat a block over a
  `many` list. No conditionals/expressions — same YAGNI line as the recipe language (§7 of the
  format spec). Missing key ⇒ empty string (logged under `--log`).
- **Path safety:** the rendered `path` is written as-is (the operator controls the recipe and
  vars); parent dirs are created. Writing is the last phase, after a fully successful run — a
  failed/tripped run writes nothing (so a stale note is never half-overwritten). Under
  `--repair`, an *unexpected* run also skips writing.

Both real flows use this: the HN recipe (§6) writes a digest note; the LinkedIn testbed recipe
writes/updates a guest note in the vault.

---

## 6. The Hacker News example (committable)

`examples/recipes/hackernews.kdl` — public, no-auth, stable markup → the ideal committable
example and PR showcase. Demonstrates all three required properties: **parameters**, **JSON
output**, **a run log**.

```kdl
recipe "news/hackernews" {
    description "Read the Hacker News front page into a JSON list of stories."
    limit-key "news.hackernews"

    var "url" default="https://news.ycombinator.com/" doc="HN listing URL (news/newest/best)"
    var "limit" default="30" doc="max stories to return"

    step { goto "{{ url }}" }

    // Tripwire: HN is up and served the listing (not an error/empty shell).
    step {
        expect on-fail="retryable" message="HN listing not present — page may be rate-limited or down" {
            visible { locator css=".athing" }
        }
    }

    // Each story row: title, external link, points, author, comment count.
    // HN markup is table-based and stable, so tier-3/4 css locators are appropriate here.
    // expect-min makes "0 stories" an *unexpected* result that can trigger --repair.
    step {
        extract "stories" many=#true expect-min=1 {
            locator css=".athing .titleline > a"
        }
    }

    // Write a JSON record and a markdown digest note (into the vault when --var vault=… is set).
    output "json" path="{{ out_dir }}/hn.json"
    output "markdown" path="{{ out_dir }}/hn-digest.md" {
        template #"""
        ---
        type: digest
        source: hackernews
        updated: {{ now }}
        ---

        # Hacker News — front page

        {{#stories}}- {{.}}
        {{/stories}}
        """#
    }

    // Optional: how Claude should fix this recipe if the front page reshuffles.
    repair {
        prompt #"""
        HN's front-page rows are <tr class="athing"> with the title link at
        .titleline > a. If stories come back empty, the class/table structure changed:
        re-anchor the story locator on the visible title-link text or the row role,
        climbing the robustness ladder. Keep the "stories" key and the many=#true shape.
        """#
    }
}
```

Run:
```
chrome-agent recipe run examples/recipes/hackernews.kdl --var limit=10 --log
# stderr: step 1 goto … → 200 "Hacker News" ; step 2 expect … → ok ; step 3 extract stories → 30 items
# stdout: {"ok":true,"result":{"stories":["title one","title two", …]}}
```

(The v1 read-only locator set returns each story's text/href list. Richer per-row records —
points+author+comments joined per story — need either `many` over a row locator with sub-field
extraction or the write-verb follow-up; v1 keeps the example within the shipped primitives and
notes the limitation inline, per the format spec's YAGNI rule §7.)

The fixture test (`tests/recipe_tests.rs`) runs this recipe against a frozen local
`file://…/recipe_hn.html` and asserts the JSON has N story titles — deterministic, no network,
in chrome-agent's existing `run_cli` + `fixture_url` style.

---

## 7. pacewright integration (consume via CLI)

- New thin **`RecipeAdapter`** in pacewright (replacing `adapter-linkedin`). For an action it
  shells `chrome-agent --browser pacewright --page pacewright recipe run <path> --var …`
  (the exact `CliBrowser` invocation pattern already in `crates/browser`), reads stdout JSON as
  the returned `Value`, and maps the child's exit / JSON error shape → `AdapterError`
  (`terminal`/`retryable`/`rate_limited`). `limit_keys_for` reads the recipe's `limit-key` so
  pacing stays declared in data.
- A `RecipeRegistry` enumerates `*.kdl` under the configured recipes dir
  (`~/.pacewright/recipes/`, the dir `pcw recipe add` already populates) and maps each recipe
  `name` `"linkedin/scrape_profile"` → `(adapter="linkedin", action="scrape_profile")`, so the
  RPC/CLI surface (`pcw add linkedin scrape_profile --params …`) is unchanged.
- The daemon registers one `RecipeAdapter` in place of `LinkedInAdapter`.

Determinism is untouched: the child chrome-agent process is browser I/O at the edge, exactly
like today's `CliBrowser` calls. pacewright core's `Clock`/`Rng` invariant is unaffected.

### 7a. The vault job-runner (frontmatter jobs → files)

The unit of work is a **job note**: an Obsidian note whose YAML frontmatter both *names a
recipe* and *supplies its vars*. This is how an operator "creates a file with the parameters
that call the recipe" — the note is the job.

```markdown
--- (wiki/guests/jane-doe.md)
type: guest
recipe: linkedin/scrape_profile      # which recipe to run
linkedin: https://www.linkedin.com/in/jane/   # → bound to the recipe's `url` var (via a map)
slug: jane-doe
status: lead
---
```

`pcw recipe job <note.md>` (and a daemon sweep over a configured jobs glob) does:

1. **Parse frontmatter** (pacewright owns a small YAML/frontmatter reader — the engine stays
   YAML-free). Require a `recipe:` key.
2. **Resolve** the recipe by name via the `RecipeRegistry` (the `~/.pacewright/recipes/` dir).
3. **Bind vars** from frontmatter. A recipe may declare a `var` with a `from` alias
   (`var "url" from="linkedin"`) so a note's domain field maps to the recipe's var; unaliased
   vars match by name. Inject `vault`/`out_dir`/`slug` context vars.
4. **Enqueue a paced task** (`adapter="linkedin", action="scrape_profile"`, deduped on the note
   path) so LinkedIn scrapes obey the daily cap — the whole reason pacewright, not a raw script,
   runs this. The `RecipeAdapter` shells `chrome-agent recipe run <resolved.kdl> --vars-json '…'`.
5. The **engine writes the output file(s)** per the recipe's `output` blocks (§5b) — e.g. the
   guest note itself, or a JSON record — into the vault at the templated path.

So: **job note (params) → paced recipe run → engine writes the markdown/JSON note.** The engine
takes vars as `--vars-json` (no YAML dep); all Obsidian-specific knowledge (frontmatter, the
`from` aliases, vault paths) lives in pacewright + recipe data, never in chrome-agent. The
vault job-runner is a pacewright slice built **after** the engine + HN example prove the path
end-to-end (§11); it gets its own plan.

---

## 8. Migration — delete the hand-written adapter

- `crates/adapter-linkedin` is **removed**; its behavior (auth-wall tripwire, settled-URL check,
  heading/follower extraction) is reproduced by the gitignored testbed recipe
  `recipes/linkedin/scrape_profile.kdl`. **LinkedIn recipes are never committed** — testbed only.
- What this migration commits: the chrome-agent `recipe` engine + the **HN** example (public,
  committable), the pacewright `RecipeAdapter`/`RecipeRegistry`, the daemon rewiring, and the
  deletion of the Rust adapter. No LinkedIn `.kdl`.
- Regression bar (validated against the local LinkedIn testbed, not committed): same live
  profile → same `name`/`followers`/`landed_url` the Rust adapter produced.

---

## 9. Testing

**In the fork (chrome-agent):**
- **Pure unit tests** (no browser): KDL parse → typed model; var interpolation; missing/unknown
  var and bad `on-fail` load-time errors; locator→JSON serialization incl. `fallback`/`within`/
  `after`; `RecipeError` class mapping. Engine step-flow over `FakeBrowser` (canned `eval`):
  extract accumulation, expect-tripwire→class, auto-wait timeout→retryable, fallback use.
- **`locators.js`** gets a JS-level test (chrome-agent already runs `tests/js/*.test.js`).
- **End-to-end fixture test** (`recipe_tests.rs`, gated on `chrome_available()`): built binary
  `recipe run` against `file://…/recipe_hn.html`, assert JSON stories. Mirrors `extract_tests.rs`.
- Existing `cli_tests.rs` / `extract_tests.rs` stay green (regression guard).
- Gates: `cargo test`, `cargo clippy -- -D warnings` (pedantic+nursery, the crate's bar), `fmt`.

**In pacewright:** `RecipeAdapter` unit tests over a faked child-process boundary (canned
stdout/exit → `Value`/`AdapterError` class); registry enumeration test over a temp recipes dir.

---

## 10. Toolchain, deps, and upstream-PR risks

- **Toolchain.** chrome-agent pins **1.95.0**; pacewright pins 1.96.1. First implementation
  checkpoint: confirm `kdl = "6"` builds on 1.95.0. If its MSRV is higher, bump chrome-agent's
  `rust-toolchain.toml` (and the CI `dtolnay/rust-toolchain@` refs) to 1.96.1 — sanctioned by
  the fork spec §3 ("align to a channel ≥ what pacewright pins").
- **The added `kdl` dep.** chrome-agent markets "zero deps" (meaning zero *system* deps → static
  binary). `kdl` is pure Rust, so the static-binary property holds, but it *is* a new crate on a
  deliberately lean tree. The PR body must justify it; a fallback (hand-rolled KDL-subset parser,
  no dep) is noted if upstream objects.
- **Clippy bar.** The crate gates on `pedantic + nursery`. New code must clear that higher bar
  (stricter than pacewright's `-D warnings` default).
- **Upstream acceptance.** A KDL recipe engine is a sizable feature `sderosiaux` may decline or
  want reshaped. Mitigation: it is additive and self-contained (own module + subcommand, no
  changes to existing verbs), and **pacewright pins the fork by git-rev regardless**, so the
  merge is never on pacewright's critical path.
- **Write verbs deferred.** `click`/`fill` (real CDP input, needed for form-driving recipes)
  are a follow-up. The `RecipeBrowser` trait is shaped to grow them without churn.

---

## 11. Sequencing

1. **Fork groundwork:** confirm `kdl` builds on the pinned toolchain (bump if needed); add
   `src/recipe/` skeleton + `RecipeBrowser` trait + `FakeBrowser`; empty `recipe check` wired so
   the binary builds and `cli_tests` stay green.
2. **Model:** typed `Recipe`/`Step`/`Locator`/`Output` parsed from `kdl::KdlDocument`, incl. the
   optional `repair { prompt … }` node, `extract` `expect-count`/`expect-min`/`expect-max`,
   `var … from="…"` aliases, and `output` blocks; validation (vars/`{{…}}`/`on-fail`/expectation/
   output-format); `recipe check` real; unit tests.
3. **Locator runtime:** `locators.js` v1 (role/text/level/nth/within/fallback/after/css) +
   `locator.rs` serialization + JS-level tests.
4. **Engine (read path):** `goto`/`extract`/`expect`/`wait`/`screenshot` over `RecipeBrowser`;
   `--log`; expectation checking → the `unexpected` marker; unit tests over `FakeBrowser`.
5. **Output renderer:** the minimal templater (scalars + `{{#list}}…{{/list}}` sections) + `json`
   serialization; `output` blocks write files after a successful run; `--vars-json`; unit tests
   over template+result fixtures.
6. **CdpBrowser + `recipe run` dispatch:** wire real page client; `--repair` context assembly
   (recipe AST + failure + `inspect` snapshot + prompt + system instruction); the HN example
   (with `output` blocks) + `file://` fixture end-to-end test asserting both JSON and the written
   markdown digest.
7. **Full fork gate;** commit on a `feat/recipe-engine` branch; write the upstream PR body
   (hold push/open per the chosen PR scope).
8. **pacewright side (engine consumer):** `RecipeAdapter` + `RecipeRegistry`; daemon rewiring; delete
   `crates/adapter-linkedin`; validate the LinkedIn regression bar against the local testbed.
9. **Vault job-runner (own plan, §7a):** frontmatter reader + `var … from` binding +
   `pcw recipe job` + daemon jobs sweep, writing markdown/JSON notes into the podcast vault.
   Built after 1–8 prove the engine path end-to-end.
