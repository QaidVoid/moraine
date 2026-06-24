//! A bounded worker pool for concurrent, blocking transfers.
//!
//! Sync uses no async runtime. Concurrency for independent transfers comes from
//! a fixed-size pool of OS threads consuming a shared work queue with blocking
//! IO. The pool bounds the number of in-flight transfers to its configured size
//! so concurrent syncs cannot exhaust connections or file descriptors.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Run `jobs` across at most `workers` threads, collecting each job's result.
///
/// Results are returned in input order. The number of concurrently executing
/// jobs never exceeds `workers`. A `workers` of zero is treated as one.
pub fn run_bounded<T, F>(workers: usize, jobs: Vec<F>) -> Vec<T>
where
    F: FnOnce() -> T + Send,
    T: Send,
{
    let workers = workers.max(1);
    let total = jobs.len();
    if total == 0 {
        return Vec::new();
    }

    let mut slots: Vec<Option<T>> = (0..total).map(|_| None).collect();
    // Each entry is taken exactly once by the worker that claims its index.
    let queue: Vec<Mutex<Option<F>>> = jobs.into_iter().map(|j| Mutex::new(Some(j))).collect();
    let queue = &queue;
    let next = AtomicUsize::new(0);
    let in_flight = AtomicUsize::new(0);
    let peak = AtomicUsize::new(0);

    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers.min(total));
        for _ in 0..workers.min(total) {
            let next = &next;
            let in_flight = &in_flight;
            let peak = &peak;
            handles.push(scope.spawn(move || {
                let mut local = Vec::new();
                loop {
                    let idx = next.fetch_add(1, Ordering::SeqCst);
                    if idx >= queue.len() {
                        break;
                    }
                    let Some(job) = queue[idx].lock().ok().and_then(|mut g| g.take()) else {
                        continue;
                    };
                    let running = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(running, Ordering::SeqCst);
                    let value = job();
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    local.push((idx, value));
                }
                local
            }));
        }
        for handle in handles {
            if let Ok(results) = handle.join() {
                for (idx, value) in results {
                    slots[idx] = Some(value);
                }
            }
        }
    });

    debug_assert!(peak.load(Ordering::SeqCst) <= workers);
    slots.into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn results_in_input_order() {
        let jobs: Vec<_> = (0..20).map(|i| move || i * 2).collect();
        let out = run_bounded(4, jobs);
        assert_eq!(out, (0..20).map(|i| i * 2).collect::<Vec<_>>());
    }

    #[test]
    fn concurrency_stays_bounded() {
        let workers = 3;
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let jobs: Vec<_> = (0..50)
            .map(|_| {
                let in_flight = Arc::clone(&in_flight);
                let peak = Arc::clone(&peak);
                move || {
                    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::yield_now();
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    now
                }
            })
            .collect();
        run_bounded(workers, jobs);
        assert!(peak.load(Ordering::SeqCst) <= workers);
    }
}
