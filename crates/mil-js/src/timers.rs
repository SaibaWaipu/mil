//! Virtual-clock execution of `setTimeout` / `setInterval` callbacks.

use std::{cell::RefCell, rc::Rc};

use rquickjs::{CaughtError, Ctx, Value};

use crate::JsDomMirror;

/// Hard cap on the number of queued callbacks executed per run.
pub const MAX_TIMER_ITERATIONS: usize = 16;
/// Hard cap on total virtual time, in milliseconds.
pub const MAX_TOTAL_MS: u64 = 1_000;

type Shared = Rc<RefCell<JsDomMirror>>;

/// Run queued timers on a virtual clock.
///
/// Each iteration advances the clock to the earliest due callback, executes it
/// and (for intervals) schedules the next occurrence. The loop stops after
/// [`MAX_TIMER_ITERATIONS`] callbacks or when the virtual clock would pass the
/// configured budget, whichever comes first.
pub fn run_timers(ctx: &Ctx<'_>, state: &Shared, budget_ms: u64) {
    let budget = budget_ms.min(MAX_TOTAL_MS);
    let mut now_ms = 0_u64;

    for _ in 0..MAX_TIMER_ITERATIONS {
        let next_due = state
            .borrow()
            .timers
            .iter()
            .filter(|timer| timer.due_ms <= budget)
            .map(|timer| timer.due_ms)
            .min();
        let Some(due_ms) = next_due else {
            break;
        };
        now_ms = now_ms.max(due_ms);

        let due_timers = take_due_timers(state, due_ms, now_ms);
        for timer in due_timers {
            let callback = match timer.callback.restore(ctx) {
                Ok(callback) => callback,
                Err(error) => {
                    state.borrow_mut().error(format!("timer restore: {error}"));
                    continue;
                }
            };
            if let Err(error) = callback.call::<_, Value>(()) {
                let message = CaughtError::from_error(ctx, error).to_string();
                state.borrow_mut().error(message);
            }
        }
    }

    // `Persistent` values must not outlive the runtime that created them.
    state.borrow_mut().timers.clear();
}

fn take_due_timers(state: &Shared, due_ms: u64, now_ms: u64) -> Vec<crate::mirror::Timer> {
    let mut mirror = state.borrow_mut();
    let mut due = Vec::new();
    mirror.timers.retain(|timer| {
        if timer.due_ms == due_ms {
            due.push(timer.clone());
            false
        } else {
            true
        }
    });
    for timer in &due {
        if let Some(interval) = timer.interval_ms {
            let mut next = timer.clone();
            next.due_ms = now_ms.saturating_add(interval);
            mirror.timers.push(next);
        }
    }
    due
}
