use anyhow::Result;
use pacewright_adapter_recipe::{
    schedule, AuthManager, CliLoginLauncher, CliRecipeRunner, RecipeRegistry, RecipeRunner,
};
use pacewright_browser::CliBrowser;
use pacewright_core::clock::SystemClock;
use pacewright_core::config::Config;
use pacewright_core::engine::Engine;
use pacewright_core::rng::SeededRng;
use pacewright_core::store::Store;
use pacewright_daemon::server::{self, build_adapter_registry};
use pacewright_daemon::web;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use tokio::sync::Mutex;

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap())
}

fn pw_dir() -> PathBuf {
    let d = home_dir().join(".pacewright");
    std::fs::create_dir_all(&d).ok();
    d
}

fn recipes_dir() -> PathBuf {
    pw_dir().join("recipes")
}

fn schedules_dir() -> PathBuf {
    pw_dir().join("schedules")
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let dir = pw_dir();
    let db_path = dir.join("pacewright.db");
    let sock_path = dir.join("pw.sock");
    let cfg_path = dir.join("config.toml");

    let mut cfg = match std::fs::read_to_string(&cfg_path) {
        Ok(s) => Config::from_toml(&s).unwrap_or_default(),
        Err(_) => Config::default(),
    };

    let store = Arc::new(Store::open(db_path.to_str().unwrap())?);

    // Layer runtime pacing overrides (from `set_limit`) over what config.toml declared.
    for (key, limit) in store.limit_overrides_all()? {
        cfg.set_limit(key, limit);
    }
    // Recipe-backed adapters: one `RecipeAdapter` per distinct `<adapter>` prefix among the
    // installed `.kdl` recipes (populated by `pcw recipe add`). The site logic that used to
    // live in `adapter-acme` is now a gitignored testbed recipe `acme/scrape_profile`.
    // `build_adapter_registry` (shared with `RecipeReload`) also registers the built-in DummyAdapter.
    let recipe_registry = Arc::new(RecipeRegistry::load_dir(&recipes_dir()));
    // All three chrome-agent callers (runner, login launcher, CliBrowser) must attach to the SAME
    // always-on Chrome, so `browser.connect` is threaded to each rather than defaulted per-caller.
    // Cloned up front because `cfg` moves into the engine before the launcher is built.
    let browser_connect = cfg.browser_connect.clone();
    let recipe_runner: Arc<dyn RecipeRunner> = Arc::new(match &cfg.browser_connect {
        Some(endpoint) => CliRecipeRunner::new().connect(endpoint),
        None => CliRecipeRunner::new(),
    });
    let clock: Arc<dyn pacewright_core::clock::Clock> = Arc::new(SystemClock);
    let reg = build_adapter_registry(&recipe_registry, &recipe_runner, &store, &clock);
    if recipe_registry.is_empty() {
        tracing::info!(
            "no recipes installed in {} — only browser-free adapters are available (add with `pcw recipe add`)",
            recipes_dir().display()
        );
    }

    // Lazy: nothing touches Chrome until a task actually drives the browser, so a daemon on a
    // machine without `chrome-agent` — or with the always-on Chrome down — still boots and runs
    // browser-free adapters. Browser tasks then fail Terminal with a clear message.
    //
    // Read the attach endpoint before `cfg` moves into the engine.
    let browser = Arc::new(match &cfg.browser_connect {
        Some(endpoint) => CliBrowser::new().connect(endpoint),
        None => CliBrowser::new(),
    });

    let engine = Engine::new(
        store,
        reg,
        cfg,
        clock,
        Arc::new(SeededRng::new(rand_seed())),
    )
    .with_browser(browser)
    .with_notifier(Arc::new(pacewright_daemon::notify::OutboxNotifier::new()));
    engine.recover_on_boot()?;

    // (No browser reaper. pacewright no longer launches browsers, so there is nothing to reap:
    // the one Chrome is supervised by launchd, and `chrome-agent gc` never touches an attached
    // session — verified 2026-07-16. The old reaper was also silently broken: it passed `--json`
    // *after* the `gc` subcommand, which is a usage error, and logged the non-zero exit at debug.)

    // Reconcile the declarative schedule files into the queue on boot, so recurring/scheduled
    // tasks come back after a restart. Invalid entries are logged and skipped, not fatal.
    {
        let (entries, load_errs) = schedule::load_dir(&schedules_dir());
        let (valid, validation_errs) = schedule::partition(&entries, &recipe_registry);
        for e in load_errs.iter().chain(validation_errs.iter()) {
            tracing::warn!("schedule: {e}");
        }
        match schedule::reconcile(&engine.store, &*engine.clock, &valid, false) {
            Ok(r) => tracing::info!(
                "schedule reconcile: {} created, {} updated, {} canceled ({} entries)",
                r.created.len(),
                r.updated.len(),
                r.canceled.len(),
                valid.len()
            ),
            Err(e) => tracing::error!("schedule reconcile failed: {e}"),
        }
    }

    // Auth: account recipes (`accounts/*`) establish + check the sessions authed recipes reuse.
    // The check runs the account recipe headless in its profile (same `recipe_runner`); login opens
    // a headed window via chrome-agent. Account recipes aren't task adapters, so they never entered
    // the `RecipeAdapter` loop above.
    let auth = Arc::new(AuthManager::new(
        recipe_registry.clone(),
        recipe_runner.clone(),
        Arc::new(match &browser_connect {
            Some(endpoint) => CliLoginLauncher::new().connect(endpoint),
            None => CliLoginLauncher::new(),
        }),
    ));

    let engine = Arc::new(Mutex::new(engine));
    let srv = Arc::new(server::Server {
        engine,
        registry: RwLock::new(recipe_registry),
        recipes_dir: recipes_dir(),
        recipe_runner,
        schedules_dir: schedules_dir(),
        auth,
    });

    // Local web dashboard (localhost only). `PACEWRIGHT_WEB_ADDR` overrides the bind address;
    // set it empty to disable the dashboard entirely. Runs alongside the socket + tick loop.
    match web_addr() {
        Some(addr) => {
            let srv = srv.clone();
            tokio::spawn(async move {
                if let Err(e) = web::serve_web(srv, addr).await {
                    tracing::error!("web dashboard failed: {e}");
                }
            });
        }
        None => tracing::info!("web dashboard disabled (PACEWRIGHT_WEB_ADDR is empty)"),
    }

    tracing::info!("pacewrightd listening on {}", sock_path.display());
    server::serve(srv, &sock_path).await
}

/// The dashboard bind address: `PACEWRIGHT_WEB_ADDR` (default `127.0.0.1:7878`), or `None` when the
/// var is set but empty (explicitly disabled) or unparseable (warned, then off).
fn web_addr() -> Option<std::net::SocketAddr> {
    let raw = std::env::var("PACEWRIGHT_WEB_ADDR").unwrap_or_else(|_| "127.0.0.1:7878".to_string());
    if raw.trim().is_empty() {
        return None;
    }
    match raw.parse() {
        Ok(a) => Some(a),
        Err(e) => {
            tracing::warn!(
                "PACEWRIGHT_WEB_ADDR `{raw}` is not a valid address ({e}) — dashboard off"
            );
            None
        }
    }
}

// A boot-time seed derived from the pid + start; randomness only affects pacing jitter.
fn rand_seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ (std::process::id() as u64)
}
