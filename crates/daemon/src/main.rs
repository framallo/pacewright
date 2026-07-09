use anyhow::Result;
use pacewright_daemon::server;
use pacewright_core::adapter::AdapterRegistry;
use pacewright_core::clock::SystemClock;
use pacewright_core::config::Config;
use pacewright_core::engine::Engine;
use pacewright_core::rng::SeededRng;
use pacewright_core::store::Store;
use pacewright_adapter_dummy::DummyAdapter;
use pacewright_adapter_recipe::{schedule, CliRecipeRunner, RecipeAdapter, RecipeRegistry};
use pacewright_browser::CliBrowser;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

fn home_dir() -> PathBuf { PathBuf::from(std::env::var("HOME").unwrap()) }

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
    let mut reg = AdapterRegistry::new();
    reg.register(Arc::new(DummyAdapter::new()));

    // Recipe-backed adapters: one `RecipeAdapter` per distinct `<adapter>` prefix among the
    // installed `.kdl` recipes (populated by `pcw recipe add`). The site logic that used to
    // live in `adapter-linkedin` is now a gitignored testbed recipe `linkedin/scrape_profile`.
    let recipe_registry = Arc::new(RecipeRegistry::load_dir(&recipes_dir()));
    let recipe_runner = Arc::new(CliRecipeRunner::new());
    for adapter_name in recipe_registry.adapters() {
        tracing::info!("registering recipe-backed adapter `{adapter_name}`");
        reg.register(Arc::new(RecipeAdapter::new(
            adapter_name,
            recipe_registry.clone(),
            recipe_runner.clone(),
        )));
    }
    if recipe_registry.is_empty() {
        tracing::info!(
            "no recipes installed in {} — only browser-free adapters are available (add with `pcw recipe add`)",
            recipes_dir().display()
        );
    }

    // Lazy: no Chrome process is touched until a task actually drives the browser, so a
    // daemon on a machine without `chrome-agent` still boots and runs browser-free
    // adapters. Browser tasks then fail Terminal with a clear message.
    let browser = Arc::new(CliBrowser::new());

    let engine = Engine::new(store, reg, cfg, Arc::new(SystemClock), Arc::new(SeededRng::new(rand_seed()))).with_browser(browser);
    engine.recover_on_boot()?;

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
                r.created.len(), r.updated.len(), r.canceled.len(), valid.len()
            ),
            Err(e) => tracing::error!("schedule reconcile failed: {e}"),
        }
    }

    let engine = Arc::new(Mutex::new(engine));
    let srv = Arc::new(server::Server {
        engine,
        registry: recipe_registry,
        schedules_dir: schedules_dir(),
    });

    tracing::info!("pacewrightd listening on {}", sock_path.display());
    server::serve(srv, &sock_path).await
}

// A boot-time seed derived from the pid + start; randomness only affects pacing jitter.
fn rand_seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0) ^ (std::process::id() as u64)
}
