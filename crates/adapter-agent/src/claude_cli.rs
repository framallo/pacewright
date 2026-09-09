//! A `claude -p` agent-step adapter — the "headless-claude round" execution kind (R1), with the
//! daemon-owned wall-clock cap + retry-on-fast-fail (R4) that every launchd A-script hand-rolled
//! with `caffeinate`/`sleep CAP; kill`.
//!
//! This is distinct from [`crate::AgentAdapter`] (`agent/ask`), which is a single-turn Anthropic
//! Messages API call — no filesystem, no tools. `claude_cli/run` shells the real `claude` CLI, so it
//! can read a working directory and write drafts, or drive a paced content round — but now as a
//! scheduled, capped, escalating pacewright task instead of a launchd agent reimplementing the
//! watchdog.
//!
//! The CLI invocation is behind a [`ClaudeRunner`] trait so the adapter's param-shaping + failure
//! classification are unit-testable without spawning a process.
use std::collections::BTreeMap;

use async_trait::async_trait;
use pacewright_core::adapter::{Adapter, RunCtx};
use pacewright_core::model::{ActionSpec, AdapterError};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Default wall-clock cap: 2100s (35m) — enough for a full paced round (a 25m cap was observed
/// killing rounds mid-batch).
pub const DEFAULT_CAP_SECS: u64 = 2100;
/// A failure faster than this is treated as a startup/API error, not a finished round, so it is
/// retryable (the engine backs off and retries) rather than a terminal give-up (R4).
const FAST_FAIL_SECS: u64 = 90;

/// One headless-claude round.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeRun {
    pub prompt: String,
    pub model: Option<String>,
    pub add_dirs: Vec<String>,
    pub cap_secs: u64,
    /// Extra environment for the child. **Never comes from the wire** — see
    /// [`build_run`] — because task params are persisted in the daemon's DB and
    /// a credential does not belong there. The adapter fills this at dispatch
    /// from the secret store.
    pub env: BTreeMap<String, String>,
}

/// The outcome of a round: stdout plus how it ended, so the adapter can classify the error class.
#[derive(Debug, Clone)]
pub struct ClaudeOutcome {
    pub stdout: String,
    pub code: Option<i32>,
    pub timed_out: bool,
    pub elapsed_secs: u64,
}

/// The seam: run one round. A spawn failure (no `claude` on PATH) is the only `Err`; a cap hit or
/// nonzero exit come back as an `Ok(ClaudeOutcome)` the adapter classifies.
#[async_trait]
pub trait ClaudeRunner: Send + Sync {
    async fn run(&self, spec: &ClaudeRun) -> Result<ClaudeOutcome, AdapterError>;
}

/// The real transport: shells `claude -p …`, applying the wall-clock cap via `tokio::time::timeout`
/// and killing the child on cap (kill-on-drop). `--dangerously-skip-permissions` mirrors the
/// scripts (an unattended round can't answer permission prompts).
pub struct ClaudeCliRunner {
    bin: String,
}

impl Default for ClaudeCliRunner {
    fn default() -> Self {
        ClaudeCliRunner {
            bin: "claude".to_string(),
        }
    }
}

impl ClaudeCliRunner {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn bin(mut self, b: impl Into<String>) -> Self {
        self.bin = b.into();
        self
    }
}

/// A model provider (gateway/proxy) is configured when `ANTHROPIC_BASE_URL` is set and non-empty.
/// In that mode `claude -p` keeps `ANTHROPIC_API_KEY`/`ANTHROPIC_AUTH_TOKEN` so it bills through the
/// provider; otherwise both are stripped and the round rides the `claude` CLI's own Claude Code
/// subscription login (the default). Pure so the decision is unit-tested without spawning.
pub(crate) fn use_model_provider(base_url: Option<&str>) -> bool {
    matches!(base_url, Some(v) if !v.trim().is_empty())
}

#[async_trait]
impl ClaudeRunner for ClaudeCliRunner {
    async fn run(&self, spec: &ClaudeRun) -> Result<ClaudeOutcome, AdapterError> {
        use tokio::process::Command;
        let mut cmd = Command::new(&self.bin);
        cmd.arg("-p")
            .arg(&spec.prompt)
            .arg("--dangerously-skip-permissions");
        if let Some(m) = &spec.model {
            cmd.arg("--model").arg(m);
        }
        for d in &spec.add_dirs {
            cmd.arg("--add-dir").arg(d);
        }
        cmd.stdin(std::process::Stdio::null());
        cmd.kill_on_drop(true);
        // Model provider vs subscription: with a provider configured (`ANTHROPIC_BASE_URL`), keep
        // the API key so `claude -p` bills through the provider. Without one, strip the key and the
        // raw auth token so the round uses the `claude` CLI's own Claude Code login (the default —
        // otherwise a daemon-env `ANTHROPIC_API_KEY` would silently bill the pay-per-token API).
        if !use_model_provider(std::env::var("ANTHROPIC_BASE_URL").ok().as_deref()) {
            cmd.env_remove("ANTHROPIC_API_KEY");
            cmd.env_remove("ANTHROPIC_AUTH_TOKEN");
        }
        // After the strip, never before: whatever the adapter resolved for this
        // run has to survive it. Today that is `CLAUDE_CODE_OAUTH_TOKEN`, which
        // is not in the list above — but relying on that would make the order a
        // silent trap for whoever adds the next name to it.
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }

        let start = Instant::now();
        let output = cmd.output();
        let result = if spec.cap_secs == 0 {
            output.await.map(Some)
        } else {
            match tokio::time::timeout(Duration::from_secs(spec.cap_secs), output).await {
                Ok(r) => r.map(Some),
                Err(_) => Ok(None), // timed out; child killed on drop
            }
        };
        let elapsed_secs = start.elapsed().as_secs();
        match result {
            Ok(Some(out)) => Ok(ClaudeOutcome {
                stdout: String::from_utf8_lossy(&out.stdout).to_string(),
                code: out.status.code(),
                timed_out: false,
                elapsed_secs,
            }),
            Ok(None) => Ok(ClaudeOutcome {
                stdout: String::new(),
                code: None,
                timed_out: true,
                elapsed_secs,
            }),
            Err(e) => Err(AdapterError::Terminal(format!(
                "could not spawn `{}`: {e}",
                self.bin
            ))),
        }
    }
}

/// The adapter. `name` is the registered prefix (`claude_cli`).
pub struct ClaudeCliAdapter {
    name: String,
    runner: Arc<dyn ClaudeRunner>,
    /// Where the setup token lives. Read at dispatch, not at construction, so
    /// rotating it takes effect on the next round without restarting the daemon.
    secrets_path: std::path::PathBuf,
}

impl ClaudeCliAdapter {
    pub fn new(name: impl Into<String>) -> Self {
        ClaudeCliAdapter {
            name: name.into(),
            runner: Arc::new(ClaudeCliRunner::new()),
            secrets_path: pacewright_core::run::home_dir().join("secrets.json"),
        }
    }
    pub fn with_runner(name: impl Into<String>, runner: Arc<dyn ClaudeRunner>) -> Self {
        ClaudeCliAdapter {
            name: name.into(),
            runner,
            secrets_path: pacewright_core::run::home_dir().join("secrets.json"),
        }
    }

    /// Point the adapter at another secret store. For tests.
    pub fn with_secrets_path(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.secrets_path = path.into();
        self
    }

    /// The environment this round should carry. Empty when there is no usable
    /// credential — and empty is the right answer there, not an empty-string
    /// variable, which would shadow whatever the daemon's own environment has.
    ///
    /// Two sources, in order:
    ///
    /// 1. A token pasted from `claude setup-token`, stored under `claude_code`.
    /// 2. **The `anthropic` OAuth login itself.** `pcw anthropic login` already
    ///    runs the Claude Code OAuth client, and what it stores is an
    ///    `sk-ant-oat01-…` with the `user:sessions:claude_code` scope — the same
    ///    thing `claude setup-token` prints, because that command is this flow
    ///    with the result shown on screen. Verified against the real CLI: with
    ///    a stale one it answers "OAuth access token has expired", not "invalid
    ///    token", so the shape is accepted and only freshness was missing.
    ///
    /// Going through `resolve_access_token` matters for (2): it refreshes and
    /// persists the rotation first, so a round never leaves with the expired
    /// token that is otherwise sitting in `secrets.json` most of the time.
    async fn env_for_run(&self) -> BTreeMap<String, String> {
        use pacewright_core::secrets::{SecretStore, CLAUDE_CODE};
        let mut env = BTreeMap::new();

        if let Ok(store) = SecretStore::load(&self.secrets_path) {
            if let Some(t) = store.static_token(CLAUDE_CODE) {
                env.insert("CLAUDE_CODE_OAUTH_TOKEN".to_string(), t.to_string());
                return env;
            }
        }

        let ahora = crate::now_ms();
        let http = crate::anthropic_oauth::ReqwestTokenHttp::default();
        match crate::anthropic_oauth::resolve_access_token(&self.secrets_path, &http, ahora).await {
            Ok(Some(t)) => {
                env.insert("CLAUDE_CODE_OAUTH_TOKEN".to_string(), t);
            }
            Ok(None) => {}
            // Se dice y se sigue sin credencial: la ronda va a fallar igual,
            // pero con el motivo escrito acá y no sólo en la salida de `claude`.
            Err(e) => eprintln!("claude_cli: no hay credencial utilizable: {e}"),
        }
        env
    }
}

/// Expand a leading `~/` to `$HOME`, so schedules can use `~/Sync/...` paths.
fn expand_home(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    p.to_string()
}

/// Assemble the [`ClaudeRun`] from task params. `prompt` (inline) or `prompt_file` (a path, `~`
/// expanded) is required. `add_dir` may be a string or an array of strings. Pure, so the shaping is
/// unit-tested without spawning.
pub fn build_run(params: &Value) -> Result<ClaudeRun, AdapterError> {
    let prompt = match params.get("prompt").and_then(Value::as_str) {
        Some(p) => p.to_string(),
        None => {
            let file = params
                .get("prompt_file")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    AdapterError::Terminal(
                        "claude_cli/run needs `prompt` or `prompt_file`".to_string(),
                    )
                })?;
            let path = expand_home(file);
            std::fs::read_to_string(&path).map_err(|e| {
                AdapterError::Terminal(format!("cannot read prompt_file {path}: {e}"))
            })?
        }
    };
    let model = params
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string);
    let add_dirs = match params.get("add_dir") {
        Some(Value::String(s)) => vec![expand_home(s)],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(expand_home)
            .collect(),
        _ => Vec::new(),
    };
    let cap_secs = params
        .get("cap_secs")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_CAP_SECS);
    Ok(ClaudeRun {
        prompt,
        model,
        add_dirs,
        cap_secs,
        // Deliberately not read from `params`: these are stored in the DB, so a
        // secret arriving this way would be persisted in plaintext. The adapter
        // injects the environment at dispatch instead.
        env: BTreeMap::new(),
    })
}

#[async_trait]
impl Adapter for ClaudeCliAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    fn actions(&self) -> Vec<ActionSpec> {
        vec![ActionSpec {
            name: "run".into(),
            limit_keys: vec![format!("{}.run", self.name)],
            params_schema: json!({
                "prompt?": "string (inline prompt)",
                "prompt_file?": "string (path to a prompt file, ~ expanded)",
                "model?": "string (e.g. claude-opus-4-8)",
                "add_dir?": "string | array of dirs claude may read/write",
                "cap_secs?": "int wall-clock cap; default 2100"
            }),
            description: "Run a headless `claude -p` round with a daemon-owned wall-clock cap. Retryable if it dies fast (startup error); terminal if it exhausts the cap.".into(),
        }]
    }

    async fn execute(
        &self,
        _ctx: &RunCtx,
        action: &str,
        params: Value,
    ) -> Result<Value, AdapterError> {
        if action != "run" {
            return Err(AdapterError::Terminal(format!(
                "claude_cli: unknown action `{action}` (expected `run`)"
            )));
        }
        let mut spec = build_run(&params)?;
        spec.env = self.env_for_run().await;
        let cap = spec.cap_secs;
        let outcome = self.runner.run(&spec).await?;
        if outcome.timed_out {
            // A cap hit is a hang, not a startup blip: terminal, and (if flagged) it escalates.
            return Err(AdapterError::Terminal(format!(
                "claude -p exceeded the {cap}s wall-clock cap and was killed"
            )));
        }
        match outcome.code {
            Some(0) => {
                let preview: String = outcome.stdout.chars().take(200).collect();
                Ok(json!({
                    "chars": outcome.stdout.len(),
                    "elapsed_secs": outcome.elapsed_secs,
                    "preview": preview,
                }))
            }
            other => {
                let msg = format!(
                    "claude -p exited {} after {}s",
                    other
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "signal".into()),
                    outcome.elapsed_secs
                );
                // Retry-on-fast-fail (R4): a quick death is a startup/API error worth one retry;
                // a slow one is a real failure the engine should give up on.
                if outcome.elapsed_secs < FAST_FAIL_SECS {
                    Err(AdapterError::Retryable(msg))
                } else {
                    Err(AdapterError::Terminal(msg))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    struct FakeRunner {
        last: Mutex<Option<ClaudeRun>>,
        outcome: ClaudeOutcome,
    }
    #[async_trait]
    impl ClaudeRunner for FakeRunner {
        async fn run(&self, spec: &ClaudeRun) -> Result<ClaudeOutcome, AdapterError> {
            *self.last.lock() = Some(spec.clone());
            Ok(self.outcome.clone())
        }
    }
    fn ok_outcome() -> ClaudeOutcome {
        ClaudeOutcome {
            stdout: "done".into(),
            code: Some(0),
            timed_out: false,
            elapsed_secs: 5,
        }
    }

    #[test]
    fn build_run_reads_inline_prompt_and_add_dir_array() {
        let run = build_run(&json!({
            "prompt": "hello",
            "model": "claude-opus-4-8",
            "add_dir": ["/a", "/b"],
            "cap_secs": 60
        }))
        .unwrap();
        assert_eq!(run.prompt, "hello");
        assert_eq!(run.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(run.add_dirs, vec!["/a".to_string(), "/b".to_string()]);
        assert_eq!(run.cap_secs, 60);
    }

    #[test]
    fn build_run_reads_a_prompt_file() {
        let dir = std::env::temp_dir().join(format!("pw-claude-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("p.md");
        std::fs::write(&f, "from a file").unwrap();
        let run = build_run(&json!({ "prompt_file": f.to_string_lossy() })).unwrap();
        assert_eq!(run.prompt, "from a file");
        assert_eq!(run.cap_secs, DEFAULT_CAP_SECS);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn build_run_requires_a_prompt_source() {
        assert!(build_run(&json!({ "model": "x" })).is_err());
    }

    fn ctx() -> RunCtx {
        RunCtx {
            task_id: "t".into(),
            browser: Arc::new(pacewright_core::browser::NullBrowser),
        }
    }

    /// Un directorio de secretos propio del test, para no tocar el del usuario.
    fn tmp_secrets(nombre: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pw-claude-sec-{}-{nombre}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("secrets.json")
    }

    const OAT: &str = "sk-ant-oat01-0123456789abcdef0123456789abcdef";

    #[test]
    fn build_run_ignores_env_from_the_wire() {
        // Es la garantía que importa: los params se guardan en la base, así que
        // un secreto que llegue por ahí quedaría en claro. Que el llamador lo
        // mande no alcanza para que se use.
        let run = build_run(&json!({
            "prompt": "hola",
            "env": { "CLAUDE_CODE_OAUTH_TOKEN": OAT, "PATH": "/evil" }
        }))
        .unwrap();
        assert!(run.env.is_empty(), "el env del cable no se usa: {:?}", run.env);
    }

    #[tokio::test]
    async fn el_token_guardado_llega_al_hijo() {
        use pacewright_core::secrets::{SecretStore, CLAUDE_CODE};
        let path = tmp_secrets("con");
        let mut store = SecretStore::load(&path).unwrap();
        store.set_static_token(CLAUDE_CODE, OAT);
        store.save().unwrap();

        let fake = Arc::new(FakeRunner {
            last: Mutex::new(None),
            outcome: ok_outcome(),
        });
        let a = ClaudeCliAdapter::with_runner("claude_cli", fake.clone())
            .with_secrets_path(&path);
        a.execute(&ctx(), "run", json!({ "prompt": "hola" }))
            .await
            .unwrap();
        let spec = fake.last.lock().clone().unwrap();
        assert_eq!(
            spec.env.get("CLAUDE_CODE_OAUTH_TOKEN").map(String::as_str),
            Some(OAT)
        );
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn sin_token_guardado_no_va_la_variable() {
        // Vacío, no vacía: una variable en "" taparía la que el daemon pudiera
        // tener puesta por otro lado.
        let path = tmp_secrets("sin");
        std::fs::remove_file(&path).ok();
        let fake = Arc::new(FakeRunner {
            last: Mutex::new(None),
            outcome: ok_outcome(),
        });
        let a = ClaudeCliAdapter::with_runner("claude_cli", fake.clone())
            .with_secrets_path(&path);
        a.execute(&ctx(), "run", json!({ "prompt": "hola" }))
            .await
            .unwrap();
        let spec = fake.last.lock().clone().unwrap();
        assert!(!spec.env.contains_key("CLAUDE_CODE_OAUTH_TOKEN"), "{:?}", spec.env);
    }

    #[test]
    fn model_provider_is_the_base_url_presence() {
        assert!(use_model_provider(Some("https://gateway.example/v1")));
        assert!(!use_model_provider(None));
        assert!(!use_model_provider(Some("")));
        assert!(!use_model_provider(Some("   ")));
    }

    #[tokio::test]
    async fn a_timeout_is_terminal() {
        let a = ClaudeCliAdapter::with_runner(
            "claude_cli",
            Arc::new(FakeRunner {
                last: Mutex::new(None),
                outcome: ClaudeOutcome {
                    stdout: String::new(),
                    code: None,
                    timed_out: true,
                    elapsed_secs: 2100,
                },
            }),
        );
        let ctx = RunCtx {
            task_id: "t".into(),
            browser: Arc::new(pacewright_core::browser::NullBrowser),
        };
        let err = a
            .execute(&ctx, "run", json!({ "prompt": "x" }))
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::Terminal(m) if m.contains("cap")));
    }

    #[tokio::test]
    async fn a_fast_nonzero_exit_is_retryable_but_a_slow_one_is_terminal() {
        let ctx = RunCtx {
            task_id: "t".into(),
            browser: Arc::new(pacewright_core::browser::NullBrowser),
        };
        let fast = ClaudeCliAdapter::with_runner(
            "claude_cli",
            Arc::new(FakeRunner {
                last: Mutex::new(None),
                outcome: ClaudeOutcome {
                    stdout: String::new(),
                    code: Some(1),
                    timed_out: false,
                    elapsed_secs: 3,
                },
            }),
        );
        assert!(matches!(
            fast.execute(&ctx, "run", json!({ "prompt": "x" })).await,
            Err(AdapterError::Retryable(_))
        ));
        let slow = ClaudeCliAdapter::with_runner(
            "claude_cli",
            Arc::new(FakeRunner {
                last: Mutex::new(None),
                outcome: ClaudeOutcome {
                    stdout: String::new(),
                    code: Some(1),
                    timed_out: false,
                    elapsed_secs: 600,
                },
            }),
        );
        assert!(matches!(
            slow.execute(&ctx, "run", json!({ "prompt": "x" })).await,
            Err(AdapterError::Terminal(_))
        ));
    }

    #[tokio::test]
    async fn success_returns_a_preview_and_passes_the_spec_through() {
        let runner = Arc::new(FakeRunner {
            last: Mutex::new(None),
            outcome: ok_outcome(),
        });
        let a = ClaudeCliAdapter::with_runner("claude_cli", runner.clone());
        let ctx = RunCtx {
            task_id: "t".into(),
            browser: Arc::new(pacewright_core::browser::NullBrowser),
        };
        let out = a
            .execute(&ctx, "run", json!({ "prompt": "hi", "add_dir": "/x" }))
            .await
            .unwrap();
        assert_eq!(out["preview"], "done");
        assert_eq!(
            runner.last.lock().as_ref().unwrap().add_dirs,
            vec!["/x".to_string()]
        );
    }
}
