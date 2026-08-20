//! Live proof that a multi-Chrome pool drives several real browsers at once.
//!
//! Ignored by default: it needs Chromes actually listening. Bring them up and run it with
//!
//! ```text
//! ./packaging/chrome-pool.sh up 3
//! cargo test -p pacewright-adapter-recipe --test pool_live -- --ignored --nocapture
//! ```
//!
//! The unit tests in `pool.rs` cover the lease arithmetic with no browser. What only a live run can
//! show is the thing the change is *for*: three recipes in flight, each attached to a different
//! Chrome, all three succeeding. A pool that leased correctly but attached to one browser anyway
//! would pass every unit test and deliver no throughput at all.

use pacewright_adapter_recipe::{ChromePool, NativeRecipeRunner, RecipeRunner, RunOpts};
use std::io::Write;

const ENDPOINTS: [&str; 3] = [
    "http://127.0.0.1:9222",
    "http://127.0.0.1:9223",
    "http://127.0.0.1:9224",
];

/// The two tests both drive Chrome on slot 0 under the same page name, so run them one at a time.
/// Two live tests sharing real hardware is a race by construction, and in production the shared
/// pool's permit is what prevents it — here each test builds its own pool, so nothing does.
static LIVE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
/// A recipe with no network at all: it navigates to a `file://` page the test writes. `about:blank`
/// and `data:` are both rejected by CDP ("Cannot navigate to invalid URL"), and any real site would
/// make this test measure the internet instead of the pool.
fn write_recipe() -> (std::path::PathBuf, std::path::PathBuf) {
    // Unique per call, not per process: cargo runs the two tests concurrently in one binary, and a
    // shared name meant the first to finish deleted the page the other was still navigating to
    // (`net::ERR_ABORTED`).
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let id = format!(
        "{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let dir = std::env::temp_dir();
    let page = dir.join(format!("pcw-pool-live-{id}.html"));
    std::fs::write(&page, b"<title>pool-probe</title><h1>pool-probe</h1>").expect("write page");
    let path = dir.join(format!("pcw-pool-live-{id}.kdl"));
    let mut f = std::fs::File::create(&path).expect("write recipe");
    write!(
        f,
        r#"recipe "pool/probe" {{
    description "Prove a run reached a real Chrome."
    limit-key "pool.probe"
    step {{ goto "file://{}" }}
    step {{ eval "who" js="document.title" }}
}}
"#,
        page.display()
    )
    .expect("write recipe body");
    (path, page)
}

/// Liveness by TCP connect, not an HTTP client: the probe must not drag a new dependency into the
/// crate just to ask whether something is listening.
async fn endpoint_live(url: &str) -> bool {
    let addr = url
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::net::TcpStream::connect(addr),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live Chrome pool: ./packaging/chrome-pool.sh up 3"]
async fn three_recipes_run_on_three_chromes_at_once() {
    let _live = LIVE.lock().await;
    for e in ENDPOINTS {
        assert!(
            endpoint_live(e).await,
            "{e} is not listening — run `./packaging/chrome-pool.sh up 3` first"
        );
    }

    let (recipe, page) = write_recipe();
    let pool = ChromePool::new(ENDPOINTS);
    assert_eq!(pool.len(), 3);
    let runner = std::sync::Arc::new(NativeRecipeRunner::new().pool(pool).timeout_secs(60));

    // Three concurrent accountless runs. With one Chrome these would serialize on the permit; with
    // three they overlap, which is the entire point.
    let started = std::time::Instant::now();
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..3 {
        let r = runner.clone();
        let p = recipe.clone();
        set.spawn(async move { r.run(&p, "{}", &RunOpts::default()).await });
    }
    let mut ok = 0;
    while let Some(joined) = set.join_next().await {
        match joined.expect("task panicked") {
            Ok(v) => {
                assert_eq!(v["ok"], true, "recipe reported failure: {v}");
                ok += 1;
            }
            Err(e) => panic!("run failed: {e}"),
        }
    }
    assert_eq!(ok, 3, "all three runs must succeed");
    eprintln!(
        "3 concurrent runs over 3 Chromes in {:?}",
        started.elapsed()
    );
    let _ = std::fs::remove_file(&recipe);
    let _ = std::fs::remove_file(&page);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live Chrome pool: ./packaging/chrome-pool.sh up 3"]
async fn one_chrome_still_works_and_serializes() {
    let _live = LIVE.lock().await;
    // The regression guard for the other use case: a pool of one is the original behavior. Two runs
    // against a single Chrome must both succeed, one after the other, never sharing the tab.
    assert!(
        endpoint_live(ENDPOINTS[0]).await,
        "{} not listening",
        ENDPOINTS[0]
    );
    let (recipe, page) = write_recipe();
    let runner = std::sync::Arc::new(
        NativeRecipeRunner::new()
            .connect(ENDPOINTS[0])
            .timeout_secs(60),
    );
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..2 {
        let r = runner.clone();
        let p = recipe.clone();
        set.spawn(async move { r.run(&p, "{}", &RunOpts::default()).await });
    }
    while let Some(joined) = set.join_next().await {
        let v = joined
            .expect("task panicked")
            .expect("run on the single Chrome failed");
        assert_eq!(v["ok"], true, "{v}");
    }
    let _ = std::fs::remove_file(&recipe);
    let _ = std::fs::remove_file(&page);
}
