//! Background jobs: what runs on a timer, and what each run said.
//!
//! Every periodic task registers here with a name and a cadence, and every
//! run is recorded: when it started, when it finished, and one line of
//! outcome or the error. The Jobs view lists that; a job can be run on
//! demand. The original twelve-phase draft's `Job { Schedule, Timeout, Retry
//! Policy, State, Progress, History }` is still not this: there is no retry
//! and no history beyond the last run, because nothing has asked for them.
//! This table itself does not persist either — what a person scheduled is
//! written down by `script_jobs` and registered here again at startup, which
//! is a smaller thing than a job store. This is the record the ponytail note
//! in the first version said would come when someone wanted to see the jobs.
//!
//! # Who spawns the loop
//!
//! A [`Spawner`], given at construction, rather than `tokio::spawn` here. The
//! window runs this from inside Tauri's `.setup()`, which is not guaranteed to
//! be on a tokio reactor the way `#[tokio::main]` is: raw `tokio::spawn` there
//! panics with "there is no reactor running" the moment the app starts,
//! despite passing every test. The engine, which *is* `#[tokio::main]`, hands
//! in `tokio::spawn`. Two callers, two answers, and neither has to know the
//! other exists.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arvo_api::JobView;
use chrono::{DateTime, Local, Utc};

/// When a job runs.
///
/// A cron schedule is read in local time, because a person who writes
/// `0 9 * * Mon-Fri` means nine o'clock where they are, not in UTC.
#[derive(Clone)]
pub enum When {
    /// This often, from the last time it started.
    Every(Duration),
    /// On this expression. Boxed because a parsed schedule is a great deal
    /// larger than a `Duration`, and most jobs are intervals.
    Cron(Box<cron::Schedule>),
}

/// The longest a cron job sleeps before looking at the clock again.
const HOP: Duration = Duration::from_secs(60);

/// Whether a job runs as soon as it is registered.
///
/// The runtime's own jobs do: probing plugins or checking staleness at
/// startup is most of the point of them. A person's scheduled script does
/// not, or every script they have scheduled would run at once, every time
/// Arvo opens.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum First {
    Now,
    AfterAWait,
}

/// What one run of a job says: one line for the Jobs view, or why it failed.
pub type Outcome = Result<String, String>;

/// How to stop a spawned loop. Called when a job is removed or replaced.
pub type Cancel = Box<dyn FnOnce() + Send>;

/// How a job's loop is put on a runtime, and how to cancel it.
///
/// See the note at the top of this module for why this is injected rather than
/// chosen here.
pub type Spawner = Arc<dyn Fn(Pin<Box<dyn Future<Output = ()> + Send>>) -> Cancel + Send + Sync>;

type Task = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Outcome> + Send>> + Send + Sync>;

struct Job {
    id: String,
    label: String,
    /// The project script this runs, for a scheduled script (`None` for one
    /// of the runtime's own).
    script: Option<String>,
    when: When,
    task: Task,
    /// The loop that ticks it, so a schedule that is changed or removed can
    /// be stopped rather than left running against a definition that is gone.
    cancel: Option<Cancel>,
    running: bool,
    runs: u32,
    last_started: Option<DateTime<Utc>>,
    last_finished: Option<DateTime<Utc>>,
    last_outcome: Option<Outcome>,
}

/// Every job the runtime runs on a timer. Cloned into each job's loop and
/// managed by Tauri for the commands.
#[derive(Clone)]
pub struct Jobs {
    jobs: Arc<Mutex<Vec<Job>>>,
    spawn: Spawner,
}

impl Jobs {
    /// A scheduler that puts its loops on `spawn`.
    #[must_use]
    pub fn new(spawn: Spawner) -> Self {
        Self { jobs: Arc::new(Mutex::new(Vec::new())), spawn }
    }

    /// Registers `task` to run every `interval`, starting now. `id` names the
    /// job to the window and to [`Self::run_now`].
    pub fn every<F, Fut>(&self, id: impl Into<String>, label: impl Into<String>, interval: Duration, task: F)
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Outcome> + Send + 'static,
    {
        self.add(id.into(), label.into(), None, When::Every(interval), First::Now, task);
    }

    /// The same, for a scheduled project script, which the window may add,
    /// change and remove while the app runs, and which may be on a cron
    /// expression rather than an interval.
    pub fn every_script<F, Fut>(
        &self,
        id: impl Into<String>,
        label: impl Into<String>,
        script: String,
        when: When,
        task: F,
    ) where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Outcome> + Send + 'static,
    {
        self.add(id.into(), label.into(), Some(script), when, First::AfterAWait, task);
    }

    fn add<F, Fut>(&self, id: String, label: String, script: Option<String>, when: When, first: First, task: F)
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Outcome> + Send + 'static,
    {
        self.remove(&id);
        let task: Task = Arc::new(move || Box::pin(task()));
        let ticking = self.clone();
        let ticks = id.clone();
        let ticking_on = when.clone();
        let cancel = (self.spawn)(Box::pin(async move {
            match ticking_on {
                When::Every(interval) => {
                    let mut ticker = tokio::time::interval(interval);
                    // `interval`'s first tick completes immediately, which is
                    // what `First::Now` wants and what the other must swallow.
                    if first == First::AfterAWait {
                        ticker.tick().await;
                    }
                    loop {
                        ticker.tick().await;
                        ticking.run(&ticks).await;
                    }
                }
                When::Cron(schedule) => loop {
                    // Nothing left to run — a fixed year already past — is a
                    // job that is over, not one to spin on.
                    let Some(next) = schedule.after(&Local::now()).next() else {
                        tracing::info!(job = %ticks, "no run left on this schedule");
                        break;
                    };
                    // Waiting in short hops rather than one long sleep. A
                    // laptop that suspends for the night stops the monotonic
                    // clock that `sleep` counts on, so a single sleep until
                    // `next` comes back however long the lid was shut; asking
                    // the wall clock again each minute costs nothing and keeps
                    // a nine o'clock job at nine o'clock.
                    while let Ok(left) = (next - Local::now()).to_std() {
                        if left.is_zero() {
                            break;
                        }
                        tokio::time::sleep(left.min(HOP)).await;
                    }
                    ticking.run(&ticks).await;
                },
            }
        }));
        if let Ok(mut jobs) = self.jobs.lock() {
            jobs.push(Job {
                id,
                label,
                script,
                when,
                task,
                cancel: Some(cancel),
                running: false,
                runs: 0,
                last_started: None,
                last_finished: None,
                last_outcome: None,
            });
        }
    }

    /// Stops a job's timer and forgets it. A run already under way finishes:
    /// killing a script mid-write to leave the schedule tidy would be the
    /// wrong trade.
    pub fn remove(&self, id: &str) {
        let Ok(mut jobs) = self.jobs.lock() else { return };
        let Some(at) = jobs.iter().position(|job| job.id == id) else { return };
        let job = jobs.remove(at);
        if let Some(cancel) = job.cancel {
            cancel();
        }
    }

    /// Runs job `id` once, now, recording the run. A job already running
    /// is left to finish rather than run twice at once.
    pub async fn run(&self, id: &str) -> Option<Outcome> {
        let task = {
            let mut jobs = self.jobs.lock().ok()?;
            let job = jobs.iter_mut().find(|job| job.id == id)?;
            if job.running {
                return None;
            }
            job.running = true;
            job.last_started = Some(Utc::now());
            job.task.clone()
        };
        let outcome = task().await;
        if let Ok(mut jobs) = self.jobs.lock() {
            if let Some(job) = jobs.iter_mut().find(|job| job.id == id) {
                job.running = false;
                job.runs += 1;
                job.last_finished = Some(Utc::now());
                match &outcome {
                    Ok(summary) => tracing::info!(job = id, summary, "job ran"),
                    Err(error) => tracing::warn!(job = id, error, "job failed"),
                }
                job.last_outcome = Some(outcome.clone());
            }
        }
        Some(outcome)
    }

    /// Runs job `id` in the background, off the caller's thread.
    pub fn run_now(&self, id: String) {
        let jobs = self.clone();
        // One run, so the canceller is dropped: `remove` cancels a job's
        // loop, and this is not one.
        drop((self.spawn)(Box::pin(async move {
            jobs.run(&id).await;
        })));
    }

    /// Every job as the window shows it.
    #[must_use]
    pub fn snapshot(&self) -> Vec<JobView> {
        let local = |at: Option<DateTime<Utc>>| {
            at.map(|at| at.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string())
        };
        self.jobs.lock().map_or_else(
            |_| Vec::new(),
            |jobs| {
                jobs.iter()
                    .map(|job| JobView {
                        id: job.id.clone(),
                        label: job.label.clone(),
                        script: job.script.clone(),
                        cron: match &job.when {
                            When::Every(_) => None,
                            When::Cron(schedule) => Some(schedule.to_string()),
                        },
                        every_secs: match &job.when {
                            When::Every(interval) => interval.as_secs(),
                            When::Cron(_) => 0,
                        },
                        running: job.running,
                        runs: job.runs,
                        last_started: local(job.last_started),
                        last_finished: local(job.last_finished),
                        last_result: job.last_outcome.as_ref().and_then(|o| o.as_ref().ok().cloned()),
                        last_error: job.last_outcome.as_ref().and_then(|o| o.as_ref().err().cloned()),
                        // An interval is only predictable once it has run; a
                        // cron expression always knows, which is what makes
                        // the Next column worth reading back as a check.
                        next_run: match &job.when {
                            When::Every(interval) => local(
                                job.last_started
                                    .map(|at| at + chrono::Duration::from_std(*interval).unwrap_or_default()),
                            ),
                            When::Cron(schedule) => schedule
                                .after(&Local::now())
                                .next()
                                .map(|at| at.format("%Y-%m-%d %H:%M:%S").to_string()),
                        },
                    })
                    .collect()
            },
        )
    }
}

/// Every background job, with its last run.
///
/// # Errors
///
/// Never; fallible only to match every other command's shape.
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A scheduler on the test's own tokio runtime: `#[tokio::test]` is on a
    /// reactor, so `tokio::spawn` is the right answer here.
    fn jobs() -> Jobs {
        Jobs::new(Arc::new(|future| {
            let handle = tokio::spawn(future);
            Box::new(move || handle.abort())
        }))
    }

    #[tokio::test]
    async fn fires_repeatedly_on_the_given_interval_and_records_each_run() {
        let count = Arc::new(AtomicUsize::new(0));
        let counter = count.clone();
        let jobs = jobs();

        jobs.every("tick", "Tick", Duration::from_millis(20), move || {
            let counter = counter.clone();
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                if n == 2 { Err("the second one fails".to_owned()) } else { Ok(format!("run {n}")) }
            }
        });

        // Bounded poll, same pattern as wait_until_serving elsewhere in
        // this repo — avoids a flaky fixed sleep.
        let mut waited = 0;
        while count.load(Ordering::SeqCst) < 3 && waited < 50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            waited += 1;
        }

        // A snapshot taken between ticks: the job runs every 20ms and a
        // snapshot mid-run is not a failure, so wait for one that is not.
        let mut shown = jobs.snapshot();
        for _ in 0..50 {
            if shown.first().is_some_and(|job| !job.running) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
            shown = jobs.snapshot();
        }
        assert_eq!(shown.len(), 1);
        let job = &shown[0];
        assert!(job.runs >= 3, "ran {} times", job.runs);
        assert!(job.last_started.is_some() && job.last_finished.is_some() && job.next_run.is_some());
        assert!(!job.running, "a snapshot between runs");
        assert!(job.last_result.as_deref().is_some_and(|s| s.starts_with("run ")) || job.last_error.is_some());

        // Removed: its timer stops and it is gone from the list. Registering
        // the same id again replaces rather than duplicates.
        jobs.every("tick", "Tick", Duration::from_millis(20), || async { Ok("again".to_owned()) });
        assert_eq!(jobs.snapshot().len(), 1, "one id, one job");
        jobs.remove("tick");
        assert!(jobs.snapshot().is_empty());

        // A run on demand is recorded like any other. On a job whose own timer
        // is a long way off, because counting runs on one that ticks every
        // 20ms races the tick and the assertion only usually wins.
        jobs.every_script("quiet", "Quiet", "scan.py".to_owned(), When::Every(Duration::from_secs(600)), || async {
            Ok("by hand".to_owned())
        });
        jobs.run("quiet").await;
        let quiet = jobs.snapshot().into_iter().find(|job| job.id == "quiet").expect("the quiet job");
        assert_eq!(quiet.runs, 1);
        assert_eq!(quiet.last_result.as_deref(), Some("by hand"));
        assert!(jobs.run("nope").await.is_none());
    }

    /// The runtime's own jobs run at startup on purpose. A person's script
    /// must not, or opening Arvo would fire every script they have scheduled.
    #[tokio::test]
    async fn a_scheduled_script_waits_for_its_first_interval_and_a_runtime_job_does_not() {
        let jobs = jobs();
        jobs.every("theirs", "Theirs", Duration::from_secs(600), || async { Ok("now".to_owned()) });
        jobs.every_script("mine", "Mine", "scan.py".to_owned(), When::Every(Duration::from_secs(600)), || async {
            Ok("now".to_owned())
        });

        let mut waited = 0;
        while jobs.snapshot().iter().all(|job| job.runs == 0) && waited < 50 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            waited += 1;
        }

        let shown = jobs.snapshot();
        let theirs = shown.iter().find(|job| job.id == "theirs").expect("the runtime's job");
        let mine = shown.iter().find(|job| job.id == "mine").expect("the script");
        assert_eq!(theirs.runs, 1, "a runtime job runs as soon as it is registered");
        assert_eq!(mine.runs, 0, "a scheduled script waits for its first interval");
        assert_eq!(mine.script.as_deref(), Some("scan.py"));
        assert_eq!(mine.cron, None, "an interval schedule has no expression to show");
    }

    #[tokio::test]
    async fn a_cron_job_reports_its_expression_and_knows_its_next_run_before_it_has_ever_run() {
        use std::str::FromStr as _;
        // Noon on the first of January: far enough off that the test never
        // waits for it, and a date the Next column can be checked against.
        let schedule = cron::Schedule::from_str("0 0 12 1 1 *").expect("a cron expression");
        let jobs = jobs();
        jobs.every_script("new-year", "New year", "party.py".to_owned(), When::Cron(Box::new(schedule)), || async {
            Ok("ran".to_owned())
        });

        let shown = jobs.snapshot();
        let job = &shown[0];
        assert_eq!(job.runs, 0);
        assert_eq!(job.every_secs, 0, "a cron job has no interval to report");
        assert!(job.cron.is_some(), "the expression is what the Cadence column shows");
        let next = job.next_run.as_deref().expect("a cron job always knows its next run");
        assert!(next.ends_with("01-01 12:00:00"), "next run was {next}");
    }
}
