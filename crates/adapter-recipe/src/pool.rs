//! The Chrome pool: which browser a run attaches to, and how many runs may go at once.
//!
//! pacewright attaches to already-running Chromes and never launches one. Until now there was
//! exactly one, so "which browser" was not a question. Two use cases pull apart:
//!
//! - **One shared Chrome** — the operator's real, logged-in browser. Safety comes from not looking
//!   like a bot: one consistent fingerprint, one real session, human pacing. A second browser would
//!   be a second fingerprint to keep straight, for nothing.
//! - **Many Chromes** — throughput. Filling merchant invoicing portals is not adversarial and the
//!   work is embarrassingly parallel, so N browsers is N times the invoices per hour.
//!
//! Both are the same mechanism at different sizes: **a pool of one is the original behavior**, so no
//! caller branches on a mode and the single-Chrome operator changes no configuration.
//!
//! Two rules make the pool safe, and both are load-bearing:
//!
//! 1. **An account is pinned to a slot.** A signed-in session lives in the one Chrome where the
//!    human signed in and cannot be moved, so `pcw auth login <account>` and every later run of
//!    that account resolve the same slot via [`pacewright_core::browser::slot_for_account`].
//! 2. **One run per Chrome at a time**, enforced by a permit per slot. That is not throttling: a
//!    named page is a real tab, and two runs in one browser would otherwise drive the *same* tab
//!    and clobber each other. The permit makes the collision unrepresentable, and as a side effect
//!    the pool size is the parallelism.

use pacewright_core::browser::{slot_for_account, DEFAULT_CHROME_CONNECT};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// A pool of attachable Chrome endpoints, each with a single permit.
#[derive(Clone)]
pub struct ChromePool {
    slots: Arc<Vec<Slot>>,
    /// Where the next accountless run starts looking. Only fairness, never correctness.
    next: Arc<AtomicUsize>,
}

struct Slot {
    endpoint: String,
    permit: Arc<Semaphore>,
}

/// A leased Chrome. Holding it is the right to drive that browser; dropping it releases the slot.
pub struct ChromeLease {
    endpoint: String,
    slot: usize,
    _permit: OwnedSemaphorePermit,
}

impl ChromeLease {
    /// The endpoint to attach to for the duration of this lease.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Which slot this is. Only needed to name the session bookkeeping — see
    /// [`ChromePool::browser_name`].
    pub fn slot(&self) -> usize {
        self.slot
    }

    /// The `--browser` name for this slot, given the caller's base name.
    pub fn browser_name(&self, base: &str) -> String {
        browser_name_for(base, self.slot)
    }
}

/// chrome-agent's `sessions.json` is keyed by `--browser <name>` + `--page <name>`, and it caches a
/// CDP *target id* per page. That cache is per-browser-name, not per-endpoint, so N Chromes sharing
/// one name share one record: a target opened in instance 0 is then looked up in instance 1, which
/// has never heard of it, and the run dies with `Target … not found in /json/list`.
///
/// Found by running it, not by reading it — every unit test passed with the names colliding.
///
/// Slot 0 keeps the bare base name, so a single-Chrome operator's existing `sessions.json` stays
/// valid and the one-Chrome case is unchanged down to the bookkeeping.
fn browser_name_for(base: &str, slot: usize) -> String {
    if slot == 0 {
        base.to_string()
    } else {
        format!("{base}-{slot}")
    }
}
impl Default for ChromePool {
    fn default() -> Self {
        Self::single(DEFAULT_CHROME_CONNECT)
    }
}

impl ChromePool {
    /// A pool from configured endpoints. Empty input falls back to a pool of one on the default
    /// endpoint, which is what an unconfigured operator has always had.
    pub fn new<I, S>(endpoints: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let slots: Vec<Slot> = endpoints
            .into_iter()
            .map(|e| Slot {
                endpoint: e.into(),
                permit: Arc::new(Semaphore::new(1)),
            })
            .collect();
        if slots.is_empty() {
            return Self::single(DEFAULT_CHROME_CONNECT);
        }
        Self {
            slots: Arc::new(slots),
            next: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// The one-Chrome pool: the original behavior.
    pub fn single(endpoint: impl Into<String>) -> Self {
        Self {
            slots: Arc::new(vec![Slot {
                endpoint: endpoint.into(),
                permit: Arc::new(Semaphore::new(1)),
            }]),
            next: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// How many Chromes, which is also how many runs can be in flight.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Never true: an empty pool falls back to a pool of one. Present because clippy asks for it
    /// next to `len`, and answering honestly beats suppressing the lint.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// The endpoints, in declaration order.
    pub fn endpoints(&self) -> impl Iterator<Item = &str> {
        self.slots.iter().map(|s| s.endpoint.as_str())
    }

    /// Which slot an account is pinned to. Exposed so a login and a later run cannot disagree.
    pub fn slot_of(&self, account: Option<&str>) -> Option<usize> {
        account.map(|a| slot_for_account(a, self.slots.len()))
    }

    /// The endpoint an account is pinned to, without taking a permit. For `pcw auth login`, which
    /// hands the browser to a human and must not hold a slot for as long as they take to type.
    pub fn endpoint_of(&self, account: Option<&str>) -> &str {
        let i = self.slot_of(account).unwrap_or(0);
        &self.slots[i].endpoint
    }

    /// The `--browser` bookkeeping name for an account's slot, given the caller's base name. The
    /// login and the run MUST agree on this too, not just the endpoint: it is the key chrome-agent
    /// caches the account's tab under.
    pub fn browser_name_of(&self, base: &str, account: Option<&str>) -> String {
        browser_name_for(base, self.slot_of(account).unwrap_or(0))
    }

    /// Lease a Chrome for one run.
    ///
    /// With an account, it is *that* account's slot or nothing: the session is there, so waiting for
    /// a busy slot is right and stealing a free one would land on a logged-out tab. Without an
    /// account, any free slot will do; if all are busy it waits on one, so a burst queues instead of
    /// two runs sharing a tab.
    pub async fn lease(&self, account: Option<&str>) -> ChromeLease {
        if let Some(i) = self.slot_of(account) {
            return self.lease_slot(i).await;
        }
        // Try each slot once, starting where the last accountless run started, so a burst spreads
        // instead of piling onto slot 0.
        let start = self.next.fetch_add(1, Ordering::Relaxed);
        for k in 0..self.slots.len() {
            let i = (start + k) % self.slots.len();
            if let Ok(permit) = self.slots[i].permit.clone().try_acquire_owned() {
                return ChromeLease {
                    endpoint: self.slots[i].endpoint.clone(),
                    slot: i,
                    _permit: permit,
                };
            }
        }
        // All busy: wait on the one this run would have taken first.
        self.lease_slot(start % self.slots.len()).await
    }

    async fn lease_slot(&self, i: usize) -> ChromeLease {
        let permit = self.slots[i]
            .permit
            .clone()
            .acquire_owned()
            .await
            .expect("pool semaphores are never closed");
        ChromeLease {
            endpoint: self.slots[i].endpoint.clone(),
            slot: i,
            _permit: permit,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_pool_of_one_is_the_original_behavior() {
        // No mode flag anywhere: the single-Chrome operator is a one-element pool, and every run —
        // account or not — attaches to the same endpoint it always did.
        let p = ChromePool::new(Vec::<String>::new());
        assert_eq!(p.len(), 1);
        assert_eq!(p.endpoint_of(None), DEFAULT_CHROME_CONNECT);
        assert_eq!(p.endpoint_of(Some("acme")), DEFAULT_CHROME_CONNECT);
        let l = p.lease(Some("acme")).await;
        assert_eq!(l.endpoint(), DEFAULT_CHROME_CONNECT);
    }

    #[tokio::test]
    async fn an_account_leases_the_chrome_it_signed_in_on() {
        // The load-bearing rule. `pcw auth login` uses `endpoint_of`, the run uses `lease`, and
        // they MUST agree — otherwise the run opens a logged-out tab in another browser.
        let p = ChromePool::new(["http://a:9222", "http://b:9222", "http://c:9222"]);
        for account in ["acme", "globex", "initech"] {
            let expected = p.endpoint_of(Some(account)).to_string();
            let l = p.lease(Some(account)).await;
            assert_eq!(l.endpoint(), expected, "login and run disagreed on {account}");
        }
    }

    #[tokio::test]
    async fn two_runs_never_share_one_chrome() {
        // A named page is a real tab: two concurrent runs in one browser drive the SAME tab and
        // clobber each other. The permit makes that unrepresentable.
        let p = ChromePool::new(["http://only:9222"]);
        let held = p.lease(None).await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), p.lease(None))
                .await
                .is_err(),
            "a second run got into the same Chrome"
        );
        drop(held);
        // Released, so the next run gets in immediately.
        assert_eq!(p.lease(None).await.endpoint(), "http://only:9222");
    }

    #[tokio::test]
    async fn an_accountless_burst_spreads_over_the_pool() {
        // The whole point of the scale case: three concurrent invoices occupy three browsers.
        let p = ChromePool::new(["http://a:9222", "http://b:9222", "http://c:9222"]);
        let leases = [p.lease(None).await, p.lease(None).await, p.lease(None).await];
        let used: std::collections::BTreeSet<&str> = leases.iter().map(|l| l.endpoint()).collect();
        assert_eq!(used.len(), 3, "runs did not spread: {used:?}");
        // And the pool is now full, so a fourth waits rather than doubling up.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), p.lease(None))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn an_account_waits_for_its_own_chrome_instead_of_taking_a_free_one() {
        // Tempting optimization, and wrong: the free browser has no session for this account.
        let p = ChromePool::new(["http://a:9222", "http://b:9222"]);
        let pinned = p.endpoint_of(Some("acme")).to_string();
        let held = p.lease(Some("acme")).await;
        assert_eq!(held.endpoint(), pinned);
        // The sibling slot is free, but `acme` must not be sent there.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), p.lease(Some("acme")))
                .await
                .is_err(),
            "an account was handed a Chrome it never signed in on"
        );
    }
}
