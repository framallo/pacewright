use rand::rngs::StdRng;
use rand::{Rng as _, SeedableRng};
use std::sync::Mutex;

pub trait Rng: Send + Sync {
    /// Return base_ms scaled by a random factor in [1-factor, 1+factor].
    fn jitter(&self, base_ms: i64, factor: f64) -> i64;
}

pub struct SeededRng(Mutex<StdRng>);
impl SeededRng {
    pub fn new(seed: u64) -> Self {
        Self(Mutex::new(StdRng::seed_from_u64(seed)))
    }
}
impl Rng for SeededRng {
    fn jitter(&self, base_ms: i64, factor: f64) -> i64 {
        let f = self.0.lock().unwrap().gen_range(-factor..=factor);
        (base_ms as f64 * (1.0 + f)).round() as i64
    }
}

/// Deterministic: always returns exactly `value_ms` regardless of input.
pub struct TestRng(pub i64);
impl TestRng {
    pub fn fixed(value_ms: i64) -> Self {
        Self(value_ms)
    }
}
impl Rng for TestRng {
    fn jitter(&self, _base_ms: i64, _factor: f64) -> i64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_seeded_is_reproducible() {
        let a = SeededRng::new(7).jitter(1000, 0.5);
        let b = SeededRng::new(7).jitter(1000, 0.5);
        assert_eq!(a, b);
        assert!((500..=1500).contains(&a));
    }
    #[test]
    fn test_fixed_rng() {
        assert_eq!(TestRng::fixed(1234).jitter(999, 0.9), 1234);
    }
}
