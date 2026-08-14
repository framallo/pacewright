# Writing pacewright recipes (KDL)

A **recipe** is a declarative [KDL](https://kdl.dev) file that names a browser (or HTTP) automation
as an ordered list of steps. pacewright runs it **in-process** against the always-on, logged-in
Chrome it attaches to (`pacewright-chrome`, the vendored engine) — no `chrome-agent` binary, no
hand-written per-site code. This is the reference for an agent (or human) authoring one.

- **Where they live:** `~/.pacewright/recipes/`. Install from a repo with
  `pcw recipe add <owner>/<repo>[@ref][#subdir]` (see [Installing](#installing-from-a-repo-private-ok)),
  or drop files in and `pcw recipe reload`.
- **Name = address:** a recipe named `recipe "linkedin/comment_post"` is invoked as adapter
  `linkedin`, action `comment_post` — `pcw add linkedin comment_post --params '{…}'`, an MCP
  `add_task`, a schedule, or a pipeline `fanout recipe=linkedin/comment_post`.
- **Run envelope:** on success a recipe yields `{ "ok": true, "result": { … }, "unexpected": [ … ] }`.
  `result` is the accumulated capture map; `unexpected` flags soft expectation misses (not failures).
- **Validate offline:** `pcw schedule check` (and the boot loader) parse every recipe through the same
  engine parser — a malformed recipe is reported, never silently half-run.

## Skeleton

```kdl
recipe "example/login" {                         // required: "<adapter>/<action>"
    description "Log in and capture the dashboard title."
    limit-key "example.login"                    // pacing bucket (cap/min-gap/jitter live in config.toml)
    auth account="example"                       // run in the "example" account's signed-in tab

    var "url"      required=#true doc="login page URL"
    var "username" required=#true
    var "password" required=#true

    step { goto "{{ url }}" }
    step { fill "{{ username }}" { locator css="#username" } }
    step { fill "{{ password }}" { locator css="#password" } }
    step { click { locator css="button[type=submit]" } }
    step { extract "title" { locator role="heading" level=1 } }
    step { expect on-fail="terminal" message="still on /login" { settled-url-matches "/login" } }

    output "json" path="~/example-login-{{ now }}.json"
}
```

## Top-level nodes

| Node | Form | Meaning |
|---|---|---|
| `recipe` | `recipe "<adapter>/<action>" { … }` | required; the name is the invocation address |
| `description` | `description "…"` | human blurb |
| `limit-key` | `limit-key "example.post"` | the pacing bucket this recipe spends (cap · min-gap · jitter · active-hours are set per-key in `config.toml`); repeatable |
| `auth` | `auth account="<name>"` \| `auth #true` \| `auth #false` | run in a signed-in account tab (`accounts/<name>` recipe establishes it); `#true` = shared profile; omit/`#false` = no session needed |
| `var` | `var "name" required=#true default="…" doc="…" from="…"` | a declared input; see below |
| `step` | `step { <verb> … }` | one ordered action; see [Steps](#steps) |
| `output` | `output "json"\|"markdown" path="…"` | render `result` and write it to a templated path; repeatable |
| `repair` | `repair { prompt "…" }` | prompt used when a run comes back `unexpected` (repair mode) |

`var` attributes: `required=#true` (fail fast if unbound), `default="…"`, `doc="…"`,
`from="…"` (source hint). Unknown top-level nodes are ignored (forward-compatible), which is how the
`auth` annotation coexists with the engine parser.

### Interpolation

Any string may contain `{{ name }}`. A reference resolves to, in scope: a declared `var`, an
`extract`/`request`/`api` capture key, a `solve` `key`, or the builtin **`now`** (epoch ms). Every
`{{ … }}` must resolve at parse time or the recipe is rejected.

## Steps

Targeting verbs take a required `locator { … }` child. Write verbs act on the resolved element.

| Verb | Form | Notes |
|---|---|---|
| `goto` | `goto "{{ url }}"` | navigate + settle |
| `reload` | `reload` | CDP reload (not a fresh nav) — for SPAs that paint blank on first goto |
| `click` | `click { locator … }` | real CDP input |
| `fill` | `fill "value" { locator … }` | native value setter (plain inputs/textareas) |
| `insert` | `insert "value" { locator … }` | trusted `insertText` for rich/`contenteditable` (Slate/React) editors `fill` can't reach |
| `select` | `select "value" { locator … }` | `<select>` only; matches option value then visible text |
| `upload` | `upload "path" { locator … }` | file input (often hidden — target with `css`) |
| `extract` | `extract "key" many=#true { locator … }` `expect-count=` `expect-min=` `expect-max=` | capture text/array into `result[key]`; cardinality misses flag `unexpected` |
| `wait` | `wait { locator … } timeout-ms=8000` | block until the locator resolves |
| `expect` | `expect on-fail="terminal"\|"retryable" message="…" { <condition> }` | tripwire: if the condition holds, abort with that error class |
| `screenshot` | `screenshot "key"` | capture PNG (base64) into `result[key]` |
| `eval` | `eval "key" js="…" retry-if-positive="dotted.path"` | run in-page JS, capture returned JSON; `retry-if-positive` polls (retryable) while a numeric path is > 0 |
| `tab` | `tab "follow"\|"back"\|"close" url-contains="…"` | multi-tab control |
| `download` | `download url="…" out="…" timeout=<secs>` | native auth-preserving download; same-origin 4xx = "not ready" → retryable |
| `request` | `request "GET" url="…" expect-status=200 { header "k" "v"; body #"…"#; capture "key" path="a.b" }` | in-page `fetch` (rides session cookies) |
| `api` | `api "POST" url="…" bearer="{{ token }}" { header …; body …; capture "key" path=… \| header=… }` | native TLS call, bearer auth injected by pacewright; a recipe built only of `api` steps runs with **no Chrome** |
| `solve` | `solve "prompt" key="answer" { locator … }` | **captcha / visual challenge** — screenshot the page, ask Claude vision to read it, type the answer into the locator (and capture it under `key`). Needs a solver wired (the daemon does); a bare run without one fails terminal |

### Locators

Match attributes on the `locator` node; nest relational locators as children.

```kdl
locator role="button" name="Submit"          // ARIA role + accessible name
locator css="#email"                          // CSS selector
locator text="Sign in"                        // visible text
locator role="heading" level=2                // heading level
locator css=".row" nth=3                       // the 3rd match
locator role="link" { within { locator css="nav" } }   // scope to an ancestor
```

Fields: `role`, `name`, `text`, `label`, `tag`, `css`, `level`, `nth`. Relational children:
`within`, `fallback` (try if the primary resolves nothing), `after`, `near`.

### `expect` conditions

```kdl
expect on-fail="terminal" message="auth wall"   { settled-url-matches "/login" }
expect on-fail="terminal" message="banned"      { visible { locator text="Account suspended" } }
expect on-fail="retryable" message="not ready"  { text-matches "Processing" { locator css=".status" } }
expect on-fail="terminal" message="empty"       { value "{{ found }}" non-empty=#true }   // or equals= / not-equals=
```

## Error classes & pacing

- A tripped `expect on-fail="terminal"` (or a bad var/recipe) → **Terminal**: the task fails and, if it
  spends a scope, pacewright pauses that scope and writes an **escalation** (the "call Claude on an
  issue" path).
- `on-fail="retryable"`, a locator timeout, or a transient HTTP error → **Retryable**: the task backs
  off and re-runs on the schedule.
- `limit-key` decides which daily cap / min-gap / jitter bucket the run spends; over-cap tasks are
  **deferred** to the next eligible slot, never dropped.

## How pacewright runs it

`NativeRecipeRunner` attaches to the always-on Chrome, resolves the recipe's account tab
(`auth account`), injects any OAuth secrets for `api` bearer steps, runs the steps in-process, and
returns the envelope. For a `solve` step it passes a Claude-vision `ClaudeSolver` (paid by the
`pcw anthropic login` Max/Pro subscription). None of this shells a `chrome-agent` binary.

## Installing from a repo (private OK)

`pcw recipe add` shells `git clone --depth 1 https://github.com/<owner>/<repo>` and pins the commit
SHA, so **private repos work through git's own auth** — no pacewright change needed. Use SSH via a
one-time rewrite:

```sh
git config --global url."git@github.com:".insteadOf "https://github.com/"
pcw recipe add <owner>/pacewright-recipes           # clones the private repo over your SSH key
pcw recipe add <owner>/pacewright-recipes#linkedin  # only the linkedin/ subdir
```

or configure a credential helper (`gh auth setup-git`). Layout the repo to mirror recipe names —
`linkedin/comment_post.kdl` → `recipe "linkedin/comment_post"`, `accounts/*.kdl` for login recipes,
`pipelines/*.kdl` for pipelines. `pcw recipe add` reloads a running daemon automatically; a new
daemon **binary** (or an `accounts/*` recipe) still needs a restart.
