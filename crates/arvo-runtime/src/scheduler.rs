use std::future::Future;
use std::time::Duration;
use tauri::async_runtime::JoinHandle;

/// Thin wrapper around `tokio::time::interval` — enough that multiple
/// callers (registry refresh here, source pulls later) can register a
/// periodic task without hand-rolling the same loop. Deliberately not the
/// original 12-phase draft's `Job { Schedule, Timeout, Retry Policy,
/// State, Progress, History }` — nothing has asked for persisted job
/// state or retry policies yet. `ponytail:` that's the upgrade path if it
/// ever gets asked for.
///
/// Spawns via `tauri::async_runtime`, not raw `tokio::spawn` — this runs
/// from inside Tauri's `.setup()`, which isn't guaranteed to already be on
/// a tokio reactor the way `#[tokio::main]`/`#[tokio::test]` are. Learned
/// the hard way: `tokio::spawn` here panics with "there is no reactor
/// running" the moment the app actually starts, despite passing in tests
/// (which run under `#[tokio::test]`, silently supplying the runtime this
/// bug depended on).
pub fn every<F, Fut>(interval: Duration, mut task: F) -> JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    tauri::async_runtime::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            task().await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn fires_repeatedly_on_the_given_interval() {
        let count = Arc::new(AtomicUsize::new(0));
        let counter = count.clone();

        let handle = every(Duration::from_millis(20), move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
            }
        });

        // Bounded poll, same pattern as wait_until_serving elsewhere in
        // this repo — avoids a flaky fixed sleep.
        let mut waited = 0;
        while count.load(Ordering::SeqCst) < 3 && waited < 50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            waited += 1;
        }

        handle.abort();
        assert!(
            count.load(Ordering::SeqCst) >= 3,
            "expected at least 3 ticks, got {}",
            count.load(Ordering::SeqCst)
        );
    }
}
