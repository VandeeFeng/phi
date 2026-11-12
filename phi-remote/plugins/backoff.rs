use std::time::Duration;

pub(super) struct Backoff {
    initial: Duration,
    maximum: Duration,
    next: Duration,
}

impl Backoff {
    pub(super) fn new(initial: Duration, maximum: Duration) -> Self {
        Self {
            initial,
            maximum,
            next: initial,
        }
    }

    pub(super) fn next_delay(&mut self) -> Duration {
        let delay = self.next;
        self.next = delay.saturating_mul(2).min(self.maximum);
        delay
    }

    pub(super) fn reset(&mut self) {
        self.next = self.initial;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig { failure_persistence: None, ..ProptestConfig::default() })]
        #[test]
        fn delays_are_bounded_and_reset(initial_ms in 1u64..1000, factor in 1u32..100, attempts in 1usize..100) {
            let initial = Duration::from_millis(initial_ms);
            let maximum = initial * factor;
            let mut backoff = Backoff::new(initial, maximum);
            let mut previous = Duration::ZERO;
            for _ in 0..attempts {
                let delay = backoff.next_delay();
                prop_assert!(delay >= previous && delay >= initial && delay <= maximum);
                previous = delay;
            }
            backoff.reset();
            prop_assert_eq!(backoff.next_delay(), initial);
        }
    }
}
