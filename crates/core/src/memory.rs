//! Memory shared out among threads, for work whose size is known before it
//! starts, decoding an image say.

use std::sync::{Condvar, Mutex, PoisonError};

/// Memory to be shared out among threads, each holding some for as long as it
/// needs it, and waiting while too little is left.
pub struct MemoryBudget {
    left: Mutex<u64>,
    freed: Condvar,
    total: u64,
}

impl MemoryBudget {
    pub const fn new(total: u64) -> Self {
        Self {
            left: Mutex::new(total),
            freed: Condvar::new(),
            total,
        }
    }

    /// All there is to hold. Work that would need more is not to be started.
    pub const fn total(&self) -> u64 {
        self.total
    }

    /// Hold `amount`, or the whole budget if it is more, waiting until that
    /// much is left. It is given back when what this returns is dropped.
    pub fn hold(&self, amount: u64) -> HeldMemory<'_> {
        let amount = amount.min(self.total);
        let mut left = self.left.lock().unwrap_or_else(PoisonError::into_inner);
        while *left < amount {
            left = self
                .freed
                .wait(left)
                .unwrap_or_else(PoisonError::into_inner);
        }
        *left -= amount;
        HeldMemory {
            budget: self,
            amount,
        }
    }
}

/// Memory held from a [`MemoryBudget`], given back when this is dropped.
pub struct HeldMemory<'a> {
    budget: &'a MemoryBudget,
    amount: u64,
}

impl Drop for HeldMemory<'_> {
    fn drop(&mut self) {
        let mut left = self
            .budget
            .left
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *left += self.amount;
        self.budget.freed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[test]
    fn a_hold_waits_until_enough_is_given_back() {
        let budget = MemoryBudget::new(100);
        let first = budget.hold(80);
        let given_back = AtomicBool::new(false);

        std::thread::scope(|scope| {
            scope.spawn(|| {
                let _second = budget.hold(50);
                assert!(
                    given_back.load(Ordering::SeqCst),
                    "held before it was there"
                );
            });
            std::thread::sleep(Duration::from_millis(50));
            given_back.store(true, Ordering::SeqCst);
            drop(first);
        });
    }

    /// More than there is to hold is the whole of it, which is there once
    /// nothing else is held, rather than never.
    #[test]
    fn more_than_the_whole_budget_holds_all_of_it() {
        let budget = MemoryBudget::new(100);
        drop(budget.hold(1_000));
        let _all = budget.hold(100);
    }
}
