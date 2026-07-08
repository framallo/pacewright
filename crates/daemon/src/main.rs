use anyhow::Result;
use pacewright_daemon::server;
use pacewright_core::adapter::AdapterRegistry;
use pacewright_core::clock::SystemClock;
use pacewright_core::config::Config;
use pacewright_core::engine::Engine;
use pacewright_core::rng::SeededRng;
use pacewright_core::store::Store;
use pacewright_adapter_dummy::DummyAdapter;
use pacewright_adapter_linkedin::LinkedInAdapter;
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let dir = pw_dir();
    let db_path = dir.join("pacewright.db");
    let sock_path = dir.join("pw.sock");
    let cfg_path = dir.join("config.toml");

    let cfg = match std::fs::read_to_string(&cfg_path) {
        Ok(s) => Config::from_toml(&s).unwrap_or_default(),
        Err(_) => Config::default(),
    };

    let store = Arc::new(Store::open(db_path.to_str().unwrap())?);
    let mut reg = AdapterRegistry::new();
    reg.register(Arc::new(DummyAdapter::new()));
    reg.register(Arc::new(LinkedInAdapter::new()));
    // M5+: register riverside/youtube adapters here.

    // Lazy: no Chrome process is touched until a task actually drives the browser, so a
    // daemon on a machine without `chrome-agent` still boots and runs browser-free
    // adapters. Browser tasks then fail Terminal with a clear message.
    let browser = Arc::new(CliBrowser::new());

    let engine = Engine::new(store, reg, cfg, Arc::new(SystemClock), Arc::new(SeededRng::new(rand_seed()))).with_browser(browser);
    engine.recover_on_boot()?;
    let engine = Arc::new(Mutex::new(engine));

    tracing::info!("pacewrightd listening on {}", sock_path.display());
    server::serve(engine, &sock_path).await
}

// A boot-time seed derived from the pid + start; randomness only affects pacing jitter.
fn rand_seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0) ^ (std::process::id() as u64)
}
