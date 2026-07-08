# Recipe format + engine — declarative browser automation as data

Status: **approved** (2026-07-08).
Depends on: the chrome-agent fork (`docs/specs/2026-07-07-chrome-agent-fork-lib.md`) — this
spec adds one small requirement to it (§8). Blocks: recipe repo, recipe testing, and
LLM-repair specs (each its own cycle — see §12).

This is **subsystem A+B** of the recipe redesign: the KDL **format** and the generic
**engine** that runs it. The recipe repo (C), known-state test harness (D), and LLM repair
loop (E) are deferred to their own specs.

---

## 1. Context — why this redesign

`LinkedInAdapter` (`crates/adapter-linkedin/src/lib.rs`) proved the browser seam works, but it
also proved the anti-pattern: its extraction logic is **hand-written Rust + an embedded JS
blob** (`PROFILE_JS`). When LinkedIn changed its markup, the fix required editing Rust,
recompiling, and redeploying a binary. Worse, the first version keyed off CSS classes
(`.text-body-medium`) and, because LinkedIn ships build-hashed classes (`e6590096 _3293afb7 …`),
**silently returned nulls** — a wrong answer that still reported `succeeded`.

The redesign: **a recipe is a data file, not code.** A single generic engine interprets it.
Platform knowledge (how to read a LinkedIn profile) lives in a versioned, testable,
Claude-authorable **KDL** file; the engine knows nothing about any platform. This is the
substrate the later subsystems need — a recipe repo (C) can only distribute *data* safely, a
test harness (D) can only pin *a recipe's* behavior, and LLM repair (E) can only rewrite
*a data file*, not recompile Rust.

### Why KDL, why locators (decisions already made)

- **Format: KDL** (`kdl.dev`). Chosen over YAML/TOML/HCL/NestedText after research
  (`scratchpad/format-comparison.md`): typed-by-construction (no YAML "Norway problem" /
  coercion class — the exact silent-corruption failure mode above), a maintained
  format-preserving Rust crate (`kdl` v6, Apache-2.0), and no arbitrary-tag security surface
  for shared recipes. Format-preserving parsing is a direct enabler of subsystem E (surgical
  single-step repair without reflowing the file).
- **Extraction primitive: Playwright-style locators**, not CSS/XPath and not raw JS. The
  resilient extraction that actually works on LinkedIn is *semantic* (`getByRole("heading")`,
  `getByText(/followers$/)`) — which is exactly what I hand-reinvented badly in `PROFILE_JS`.
  Locators are resolved by a **runtime that ships with the engine** (`locators.js`), so
  recipes stay pure declarative data and never carry executable JS.

### Authentication is not a recipe concern (decided)

A recipe never logs in. Authentication is a property of how the **session** is established, not
a step in the flow. The daemon owns one long-lived session whose Chrome cookies are inherited
from the operator's real, already-logged-in profile (the browser layer's copy-cookies — the
fork's `Session { copy_cookies: true }`). Every recipe runs *inside* that authenticated context.

Rejected alternatives, and why:
- **Conditional login** ("if logged out, log in") needs conditionals we excluded (§7) *and*
  means scripting LinkedIn's login form — exactly the fresh, unproven session LinkedIn's
  anti-bot keys on. The core thesis (core-engine spec §9) is that the safe substrate is the
  *already-trusted* profile, not a scripted login.
- **A `linkedin/login` recipe dependency** has the same login-automation problem plus needs
  recipe composition we don't have.
- **Cookie inheritance** ✅ is what already works in this codebase (chrome-agent `--copy-cookies`
  copies the Cookies DB + the `Local State` decryption key). It is a **session-lifecycle**
  responsibility — including the "cookies only copy on a *fresh* launch" footgun found this
  session — and lives entirely in the browser/fork layer, never in a recipe.

A recipe's *only* interaction with auth is the `expect` tripwire that detects an auth wall and
fails `Terminal` with a clear message. It detects; it never fixes.

---

## 2. Goal / non-goals

**Goal.**
1. A KDL **recipe schema** (§4) — declarative, non-Turing-complete, human- and Claude-authorable.
2. A generic **`RecipeEngine`** (§6) that runs a recipe over `BrowserHandle` with Playwright-style
   locator resolution (§5) and real CDP actions.
3. A **`RecipeAdapter`** that plugs recipes into the existing `Adapter`/pacing/runner machinery,
   preserving the `pcw add <adapter> <action> --params …` UX.
4. Port `linkedin/scrape_profile` to a recipe and **delete** the hand-written adapter (§9).

**Non-goals (deferred, each its own spec).**
- The "awesome recipes" **repo** (subsystem C). Recipes ship in-repo under `recipes/` for now.
- The known-state **fixture + golden test harness** (subsystem D). This spec ships only
  engine-level unit tests over `FakeBrowser`.
- The **LLM repair** loop (subsystem E). The recipe model is designed to *allow* it (typed model
  + format-preserving AST retained), but the loop is not built here.
- The chrome-agent fork itself — separate, already specced. This spec only names the
  `BrowserHandle` surface it must provide (§8).

---

## 3. Architecture

New crate **`pacewright-recipe`**, depending on `pacewright-core` (for `BrowserHandle` +
`AdapterError`), `kdl`, and `knuffel`:

```
crates/recipe/
  src/
    model.rs     # typed Recipe/Step/Locator (knuffel-derived from KDL)
    engine.rs    # RecipeEngine: runs a Recipe over &dyn BrowserHandle
    locator.rs   # Rust side of locator specs -> JSON passed to the injected runtime
    adapter.rs   # RecipeAdapter: impl Adapter, routes a recipe name to a run
    runtime/
      locators.js  # the injected locator runtime (bundled via include_str!)
```

- **`model.rs`** parses KDL into a typed `Recipe` using `knuffel` (derive-based KDL→struct). The
  raw `kdl::KdlDocument` AST is retained alongside the typed model so subsystem E can later
  rewrite a single node format-preservingly. (This spec doesn't build the rewrite path, only
  keeps the door open by not discarding the AST.)
- **`locators.js`** is injected once per session as a CDP init-script (§8), defining a global
  `__pw` with `resolve(spec) -> {found, backendNodeId|null, text|null, count}` and
  `extract(spec, many) -> value`. It reimplements the semantic locators
  (`role`/`name`/`text`/`label`/`css`/`level`/`nth`/`within`/`fallback`). It is engine code,
  reviewed and versioned with the engine — never supplied by a recipe.
- **`RecipeAdapter`** implements the existing `pacewright_core::adapter::Adapter`. A
  `RecipeRegistry` loads every `*.kdl` under a **configured recipes directory** at daemon start
  (default `~/.pacewright/recipes/`, overridable via config / `$PACEWRIGHT_RECIPES`). Recipes
  are **distributed data, not repo source** — they are *not* committed into this engine repo;
  they come from the operator's local dir or the awesome-recipes repo (subsystem C). Each
  recipe's `name` ("linkedin/scrape_profile") maps to `(adapter="linkedin", action="scrape_profile")`
  so the RPC/CLI surface is unchanged. `limit_keys_for` reads the recipe's `limit-key`, so
  pacing stays declared in data.

---

## 4. The recipe schema (KDL)

```kdl
recipe "linkedin/scrape_profile" {
    description "Navigate to a LinkedIn profile and extract the top card."
    limit-key "linkedin.profile_scrape"

    // Variables interpolated into string values as {{ name }}.
    var "url" required=#true doc="https://www.linkedin.com/in/<slug>/"

    step {
        goto "{{ url }}"
    }

    // A tripwire: if the listed condition holds, abort the run with `on-fail`'s error class.
    step {
        expect on-fail="terminal" message="auth wall — Chrome not logged into LinkedIn" {
            // `settled-*` reads the post-redirect location, not goto's echoed URL.
            settled-url-matches "/authwall|/login/"
        }
    }

    step {
        extract "name" {
            locator role="heading" level=1 {
                fallback role="heading" level=2   // self-view uses h2
            }
        }
    }

    step {
        extract "followers" {
            locator text=#"/followers$/"#         // lexical anchor, not markup
        }
    }
}
```

### Schema reference

- `recipe "<name>"` — the top node; `name` is `"<adapter>/<action>"`.
  - `description "<text>"` — one line.
  - `limit-key "<key>"` — 0+; the pacing keys this recipe spends (maps to `ActionSpec.limit_keys`).
  - `var "<name>" required=#bool doc="<text>" default="<value>"` — 0+ declared inputs.
  - `step { <verb> … }` — ordered; executed top to bottom, each auto-waits (§6).

- **Step verbs (v1):**
  | verb | form | meaning |
  |------|------|---------|
  | `goto` | `goto "<url>"` | navigate and settle |
  | `click` | `click { locator … }` | real CDP click on the resolved target |
  | `fill` | `fill "<value>" { locator … }` | real CDP typing into the resolved target |
  | `extract` | `extract "<key>" many=#bool { locator … }` | read text (or list) into the result under `<key>` |
  | `expect` | `expect on-fail="<class>" message="…" { <condition> }` | tripwire; abort if the condition holds (see below) |
  | `wait` | `wait { locator … timeout-ms=<n> }` | block until the locator is present (auto-wait is implicit on other verbs; this is an explicit barrier) |
  | `screenshot` | `screenshot "<key>"` | capture PNG bytes into the result under `<key>` (base64) |

- **Locator** (`locator <props> { <child locators> }`): the match fields are KDL **properties** —
  `role`, `name`, `text` (string or `/regex/` via a raw string `#"…"#`), `label`, `tag`, `css`,
  `level` (heading level), `nth`. Several fields are **child nodes**, not properties, because KDL
  property values are scalars and these are themselves locators: `fallback <props>` (tried when the
  primary resolves nothing), `within <props>` (scopes the search to a subtree), and the
  **relative anchors** `after <props>` / `near <props>` (§4a). Validated against the `kdl` crate —
  `within={…}` / `after={…}` as a *property* is a parse error; they must be child nodes. Locators
  are how *every* targeting verb (`click`/`fill`/`extract`/`wait`) names an element.

### 4a. The robustness ladder (relative locators)

Some data has no semantic handle of its own — e.g. a LinkedIn `headline`/`location` is just "the
paragraph under the name," a bare `<p>` with a build-hashed class. Rather than pin it with a
brittle absolute position, anchor it to a *semantic* element with a relative locator. `after <loc>`
resolves the first element (matching the outer locator's props) that follows the anchor in document
order; `near <loc>` the nearest. Authors — and the LLM-repair loop (subsystem E) — should prefer
the highest tier that resolves:

| Tier | Form | Survives | Example |
|---|---|---|---|
| 1 semantic | `role`/`text`/`label` | class + layout churn | `locator role="heading"` |
| 2 relative | `after`/`near` an anchor | class churn, most layout | `locator tag="p" nth=0 { after role="heading" }` |
| 3 positional | `nth` + `within` | class churn only | `locator css="main p" nth=1` |
| 4 raw | `css` / xpath | nothing | `locator css=".pv-text-details__left"` |

Tier-2 makes fields *pinnable*; it does not make them immortal — a structural reshuffle still
breaks them, which is what subsystem D (golden tests on a frozen page) catches and subsystem E
(re-derive the locator, climbing the ladder) heals.

- **`expect` is a tripwire, not an assertion.** It names a *bad state*; if that state holds, the
  step aborts the run with the `on-fail` error class. Conditions: `settled-url-matches "<regex>"`
  (post-redirect URL), `visible { locator … }`, `text-matches "<regex>" { locator … }`.
  `on-fail` ∈ `terminal | retryable | rate_limited` → the `AdapterError` the runner receives.
  (There is deliberately no "assert present" form: `wait`/`extract` already auto-wait for an
  element and fail `Retryable` on timeout, which covers the positive case.)

- **Interpolation:** `{{ var }}` inside any string value is replaced from bound vars before the
  step runs. Unknown var ⇒ load-time error. No expressions — substitution only.

---

## 5. Locator resolution (the Playwright part)

1. On session start the engine injects `locators.js` (§8) so `__pw` exists on every document,
   including after navigations (CDP `Page.addScriptToEvaluateOnNewDocument`).
2. To resolve a locator the engine serializes the locator spec to JSON and calls
   `__pw.resolve(spec)` via `browser.eval`. The runtime implements semantic matching:
   `role` → ARIA role, `text`/`/regex/` → visible-text match, `level` → heading level,
   `tag` → element name, `within` → scoped subtree, `after`/`near` → resolve the anchor first
   then take the following/nearest match (§4a), `nth` → index, `fallback` → second attempt. It
   returns `{found, backendNodeId, text, count}`.
3. **Auto-wait:** targeting verbs poll `__pw.resolve` every ~250ms until `found` (or a
   per-step `timeout-ms`, default e.g. 10s). Timeout ⇒ `AdapterError::Retryable` (the page may
   just be slow; the runner backs off and requeues).
4. **Actions use real CDP input**, never JS synthetic events: `click`/`fill` take the resolved
   `backendNodeId`, ask the browser for its box model, and dispatch `Input.dispatch*`. Synthetic
   events are detectable and break LinkedIn `@`-mention autocomplete (design spec §9). This is
   why the engine needs the fork's real-input `BrowserHandle` methods (§8), not just `eval`.
5. **`extract`** calls `__pw.extract(spec, many)` — for `many=#false` the resolved element's
   trimmed text; for `many=#true` the array of matches' texts. Result is written under the
   step's key.

`locators.js` shipping *with the engine* is the load-bearing security property: a downloaded
recipe (subsystem C) is inert data; it can only name locators, never run code.

---

## 6. Engine execution model

```
RecipeEngine::run(recipe, vars, &dyn BrowserHandle) -> Result<Value, AdapterError>
```

1. **Bind + validate:** check required vars present, apply defaults, fail load-time on unknown
   `{{ … }}` references.
2. **Inject** `locators.js` (idempotent per session).
3. **Per step, in order:**
   - `goto` → `browser.goto`, then settle.
   - `click`/`fill`/`wait` → resolve locator with auto-wait, perform the CDP action.
   - `extract` → resolve + read, write into the result map under its key.
   - `expect` → evaluate the tripwire condition against *settled* state; if it holds, return the
     mapped `AdapterError` (`on-fail` class) with `message` and stop the run.
   - `screenshot` → `browser.screenshot`, base64 into the result.
4. **Return** the accumulated result map as `Ok(Value)` — exactly the shape adapters return
   today, so the runner/store/audit-trail are unchanged.

Error mapping is the integration seam with `crates/core/src/runner.rs`: guards choose their
class explicitly; locator timeouts are `Retryable`; a missing browser or malformed recipe is
`Terminal`. The runner's existing backoff/requeue/defer semantics then apply for free.

---

## 7. Language scope — the YAGNI line

v1 recipes are: **ordered steps, each auto-waits; `extract` one-or-many; `expect` guards that
fail with a pacewright error class.** No conditionals, no loops, no expressions, no arithmetic —
**non-Turing-complete by construction.** Rationale: the smaller and dumber the format, the
better subsystems D (testable) and E (Claude repairs it reliably) work; every browser-DSL in
history slid into being a bad programming language, and a self-healing system multiplies the
cost of that surface.

Two observations keep this sufficient: list extraction is a *property of a step* (`many=#true`),
not control flow; and "if X stop" is a *guard*, not an `if`. **Rule:** the first real recipe that
provably cannot be written within this scope forces a design conversation (a documented,
reviewed extension), not an inline hack.

---

## 8. What this requires from `BrowserHandle` / the fork

Today `BrowserHandle` (`crates/core/src/browser.rs`) has `goto`/`eval`/`screenshot`. The engine
needs it to grow, backed by the fork's persistent `Session` + real-input layer:

- `inject_init_script(js: &str)` — register `locators.js` to run on every new document
  (`Page.addScriptToEvaluateOnNewDocument`). **This is the one addition to the approved fork
  spec** (`2026-07-07-chrome-agent-fork-lib.md` §4): add an init-script hook to the `Page`
  facade. Small; noted there as a follow-up.
- `click(target)` / `fill(target, value)` — real CDP `Input.dispatch*` on a resolved
  `backendNodeId` (the fork's `element.rs` already dispatches these; the facade exposes them).
- `wait_for(locator_json, timeout)` may live engine-side (poll `eval`) rather than as a trait
  method — decided in the plan; the trait minimally needs `inject_init_script` + `click` +
  `fill` beyond today's three.

Until the fork lands, the engine cannot run for real (the CLI `CliBrowser` re-spawns per verb and
loses the injected runtime between calls — see the sequencing note). Per the agreed build order,
**the fork is built first**; this engine targets the fork's `BrowserHandle` impl.

---

## 9. Migration — delete the hand-written adapter

- `crates/adapter-linkedin` is **removed**. Its behavior (auth-wall tripwire, settled-URL check,
  heading/follower extraction) is reproduced by `linkedin/scrape_profile.kdl` — authored and
  KDL-validated this session (`recipes/linkedin/`, **gitignored**; see below).
- **LinkedIn recipes are a testbed, not a committed artifact.** Recipes are distributed data
  (§3): the LinkedIn ones live in the operator's local recipes dir now and the awesome-recipes
  repo (subsystem C) later — never in this engine repo. `recipes/` is gitignored precisely so it
  can be a stable local testbed without becoming repo source. What this migration *commits* is the
  engine and the deletion of the Rust adapter, not any LinkedIn `.kdl`.
- The daemon registers one `RecipeAdapter` (backed by a `RecipeRegistry` over the configured
  recipes dir) in place of `LinkedInAdapter`. With a LinkedIn recipe present, `pcw adapters` lists
  `linkedin/scrape_profile` and `pcw add linkedin scrape_profile --params '{"url":…}'` runs it.
- Regression bar (validated against the local testbed recipe, not committed): run against the same
  live profile, the recipe must return the same `name`/`followers`/`landed_url` the Rust adapter
  produced this session. (`headline`/`location` were positional even in Rust; the recipe returns
  the raw `top_card` list for the caller to map — see the recipe's own note.)

---

## 10. Determinism

The engine is a **new crate, not `core`** — core's `Clock`/`Rng` determinism invariant is
untouched. Auto-wait polling uses real time at the browser edge, consistent with the existing
rule that browser I/O is the non-deterministic edge (like network). Engine *logic* (var binding,
step ordering, result assembly, error mapping) is deterministic given browser responses, which is
what makes it unit-testable over `FakeBrowser`.

---

## 11. Testing (this spec)

- **Engine unit tests over `FakeBrowser`:** script `eval` to return canned `__pw.resolve`
  responses and assert step behavior — var interpolation, extract accumulation, guard→error-class
  mapping, auto-wait timeout→`Retryable`, fallback-locator use.
- **Model tests:** parse small example KDL recipes (committed as test fixtures under the crate,
  *non-LinkedIn* — a synthetic `example/*` recipe) into the typed model; assert load-time errors
  for missing required vars / unknown `{{ … }}` / bad `on-fail` class. Committed test recipes are
  deliberately generic; LinkedIn recipes stay in the gitignored testbed.
- The **real known-state fixture + golden-output harness is subsystem D** — deferred. This spec
  deliberately does not freeze a LinkedIn page; it proves the *engine*, not a *recipe*.

---

## 12. Risks / open questions

- **Locator runtime fidelity.** `locators.js` reimplementing a slice of Playwright's locator
  semantics is the technical heart; getting `getByRole`/visible-text right across real pages is
  non-trivial. Mitigation: keep the v1 locator set small (role/text/level/nth/within/fallback +
  css escape) and grow it against real recipes.
- **Fork dependency.** The engine is unrunnable-for-real until the fork provides a persistent
  session + init-script injection + real input. This spec is buildable and unit-testable before
  the fork lands, but end-to-end validation waits on it.
- **Recipe/adapter routing.** One `RecipeAdapter` fronting many recipes vs. one adapter per
  namespace — settled in the plan; must preserve today's `adapter/action` RPC shape.
- **Format-preserving repair (E) coupling.** Retaining the `kdl` AST alongside the typed model
  costs a little memory now to keep E cheap later; if that proves awkward, E can re-parse.
- **`fill`/secrets.** Typing values (e.g. into a login form) will eventually mean secrets in
  recipes/vars — out of scope here, but flagged for C/E: vars may need a secret class.

---

## 13. Sequencing within this spec

1. `pacewright-recipe` crate skeleton; typed `Recipe` model via `knuffel`; parse the §4 recipe.
2. Model validation (required vars, `{{ … }}` resolution, `on-fail` classes) + tests.
3. `locators.js` v1 (role/text/level/nth/within/fallback/css) + a JS-level test harness.
4. `RecipeEngine` over `BrowserHandle`: goto/extract/expect first (read-only path), unit-tested
   on `FakeBrowser`.
5. `click`/`fill`/`wait` with auto-wait once the fork's real-input `BrowserHandle` methods exist.
6. `RecipeAdapter` + `RecipeRegistry`; wire into the daemon behind the existing RPC shape.
7. Port `linkedin/scrape_profile` to KDL; delete `crates/adapter-linkedin`; verify the
   regression bar (§9) end-to-end against the fork.
