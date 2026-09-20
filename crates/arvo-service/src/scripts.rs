//! A person's own scripts: running one, and the cadences they set (#123).
//!
//! # Why the engine runs them and not the window
//!
//! It used to be the window. Schedules were registered from Tauri's setup
//! hook against window state, so they fired only while the window was open:
//! someone who set a cadence and closed the window got nothing, silently, and
//! would only notice by the absence of results. A schedule that needs a
//! watcher is not a schedule, and the engine is the process that keeps
//! running when the window closes (ADR-0018). So it runs them.
//!
//! Nothing else changes. A script runs with the person's privileges, as
//! anything they start from a terminal does, and the engine runs as the same
//! user on the same machine. What a script can reach of Arvo is the research
//! API through the `arvo` package, which cannot fetch, trade or name a
//! credential (ADR-0016). No agent tool starts a run.
//!
//! # One runner
//!
//! A run started by hand and a run started by a timer are the same run: same
//! interpreter, same output stream, same stop. That was true when the window
//! owned this and it stays true here, which is why [`Scripts::run`] is what
//! both the `RunScript` call and the job closure use.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::FromStr as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arvo_api::{ScriptJobView, ScriptOutputView};
use chrono::Local;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::{broadcast, oneshot};

use crate::CommandError;

/// What the schedules are kept in, beside the app's other state.
const FILE: &str = "scheduled-scripts.json";

/// The settings a person edits, user-wide then per project. The engine reads
/// the same two files the window writes; the interpreter choice (ADR-0020)
/// is the one setting a run consults.
const USER_SETTINGS: &str = "settings.json";
const PROJECT_SETTINGS: &str = ".arvo/settings.json";

/// The tightest cadence allowed. A script every few seconds is a way to wedge
/// the machine by accident, and nothing about research needs it.
const FLOOR_SECS: u64 = 60;

/// How many output lines a subscriber may fall behind before it starts
/// missing them. A script printing faster than a reader reads is the reader's
/// problem to survive, not a reason to stall the script.
const OUTPUT_BACKLOG: usize = 1024;

/// The engine's script runner and the schedules it keeps.
pub struct Scripts {
    /// The project folder. A script is a file inside it and nowhere else.
    root: PathBuf,
    /// Where `scheduled-scripts.json` and the user settings live.
    state: PathBuf,
    jobs: arvo_schedule::Jobs,
    /// Runs in progress: how to stop each one.
    runs: Mutex<(u32, HashMap<u32, oneshot::Sender<()>>)>,
    output: broadcast::Sender<ScriptOutputView>,
}

impl Scripts {
    /// The runner, with every saved schedule already registered.
    ///
    /// A schedule that will not parse is logged and skipped: the file may
    /// have been edited by hand, and the others are still good.
    #[must_use]
    pub fn new(root: PathBuf, state: PathBuf, jobs: arvo_schedule::Jobs) -> Arc<Self> {
        let scripts = Arc::new(Self {
            root,
            state,
            jobs,
            runs: Mutex::new((0, HashMap::new())),
            output: broadcast::channel(OUTPUT_BACKLOG).0,
        });
        for job in scripts.saved() {
            if job.enabled {
                if let Err(why) = scripts.register(&job) {
                    tracing::warn!(script = %job.script, why, "a saved schedule could not be registered");
                }
            }
        }
        scripts
    }

    /// Every line every run prints, from now on.
    #[must_use]
    pub fn watch(&self) -> broadcast::Receiver<ScriptOutputView> {
        self.output.subscribe()
    }

    /// Starts the script at `path` and returns the run's number.
    ///
    /// Returns as soon as it is running. Output arrives on [`Self::watch`],
    /// ending with an `exit` line.
    ///
    /// # Errors
    ///
    /// Not a Python file inside the project, or no interpreter to run it.
    pub fn start(self: &Arc<Self>, path: &str) -> Result<u32, CommandError> {
        let (child, run, stopped) = self.spawn(path)?;
        let scripts = Arc::clone(self);
        tokio::spawn(async move { scripts.stream_to_end(child, run, stopped).await });
        Ok(run)
    }

    /// Runs the script at `path` to completion and says how it ended. What a
    /// scheduled run awaits.
    ///
    /// # Errors
    ///
    /// Not a Python file inside the project, or no interpreter to run it.
    pub async fn run(self: &Arc<Self>, path: &str) -> Result<String, CommandError> {
        let (child, run, stopped) = self.spawn(path)?;
        Ok(self.stream_to_end(child, run, stopped).await)
    }

    /// Stops a run. Nothing happens for a run that has already ended.
    pub fn stop(&self, run: u32) {
        if let Some(stop) = self.runs.lock().ok().and_then(|mut guard| guard.1.remove(&run)) {
            let _ = stop.send(());
        }
    }

    /// Every saved schedule, enabled or not.
    ///
    /// A file that is missing or unreadable is no schedules: a corrupt list
    /// should not stop the engine starting.
    #[must_use]
    pub fn saved(&self) -> Vec<ScriptJobView> {
        std::fs::read_to_string(self.state.join(FILE))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Saves a cadence for one script and starts its timer, replacing any
    /// schedule that script already had.
    ///
    /// A cadence under [`FLOOR_SECS`] is raised to it; a cron expression that
    /// would run oftener is refused rather than rounded, because there is no
    /// obvious thing to round it to.
    ///
    /// # Errors
    ///
    /// `script` is not a Python file inside the project, `cron` is not an
    /// expression, or the schedule file cannot be written. Checked here
    /// rather than at the first tick, so a typo is refused while the person
    /// is still looking at it.
    pub fn save(
        self: &Arc<Self>,
        script: String,
        every_secs: u64,
        cron: Option<String>,
        enabled: bool,
    ) -> Result<Vec<ScriptJobView>, CommandError> {
        self.script_path(&script)?;
        // An empty box is no expression, not a broken one.
        let cron = cron.map(|text| text.trim().to_owned()).filter(|text| !text.is_empty());

        let job = ScriptJobView { script, every_secs: every_secs.max(FLOOR_SECS), cron, enabled };
        if job.enabled {
            // Registering first, so an expression that will not parse is
            // refused before it is written down.
            self.register(&job).map_err(CommandError::Failed)?;
        } else {
            if let Some(expression) = &job.cron {
                parse_cron(expression).map_err(CommandError::Failed)?;
            }
            self.jobs.remove(&job_id(&job.script));
        }

        let mut all = self.saved();
        upsert(&mut all, job.clone());
        self.write(&all)?;
        tracing::info!(
            script = %job.script,
            every = job.every_secs,
            cron = job.cron.as_deref().unwrap_or("-"),
            enabled = job.enabled,
            "script schedule saved"
        );
        Ok(all)
    }

    /// Forgets a script's schedule and stops its timer. The script itself is
    /// left alone: this removes a schedule, not a file.
    ///
    /// # Errors
    ///
    /// The schedule file cannot be written.
    pub fn remove(&self, script: &str) -> Result<Vec<ScriptJobView>, CommandError> {
        let mut all = self.saved();
        all.retain(|saved| saved.script != script);
        self.write(&all)?;
        self.jobs.remove(&job_id(script));
        tracing::info!(script, "script schedule removed");
        Ok(all)
    }

    /// Registers one schedule with the job table. Replaces any job already
    /// under that id, so saving a new cadence takes effect without a restart.
    fn register(self: &Arc<Self>, job: &ScriptJobView) -> Result<(), String> {
        let when = match &job.cron {
            Some(expression) => arvo_schedule::When::Cron(Box::new(parse_cron(expression)?)),
            None => arvo_schedule::When::Every(Duration::from_secs(job.every_secs.max(FLOOR_SECS))),
        };
        let scripts = Arc::clone(self);
        let script = job.script.clone();
        self.jobs.every_script(job_id(&job.script), format!("Run {}", job.script), job.script.clone(), when, move || {
            let scripts = Arc::clone(&scripts);
            let script = script.clone();
            async move {
                match scripts.run(&script).await {
                    // How it ended is the job's outcome: a script that exits
                    // non-zero is a failed run, and the Jobs view should say
                    // so rather than report a success because it was started.
                    Ok(ended) if ended == "exited with code 0" => Ok(format!("{script}: ok")),
                    Ok(ended) => Err(format!("{script}: {ended}")),
                    Err(err) => Err(err.to_string()),
                }
            }
        });
        Ok(())
    }

    fn write(&self, jobs: &[ScriptJobView]) -> Result<(), CommandError> {
        std::fs::create_dir_all(&self.state)
            .map_err(|err| CommandError::Failed(format!("{}: {err}", self.state.display())))?;
        let mut text = serde_json::to_string_pretty(jobs).map_err(|err| CommandError::Failed(err.to_string()))?;
        text.push('\n');
        std::fs::write(self.state.join(FILE), text)
            .map_err(|err| CommandError::Failed(format!("saving the schedule: {err}")))
    }

    /// The file `relative` names, if it is a Python file inside the project.
    fn script_path(&self, relative: &str) -> Result<PathBuf, CommandError> {
        let mut path = self.root.clone();
        for segment in relative.split('/').filter(|segment| !segment.is_empty()) {
            path.push(name(segment).map_err(CommandError::Failed)?);
        }
        let real =
            dunce::canonicalize(&path).map_err(|err| CommandError::Failed(format!("{relative}: {err}")))?;
        if !real.starts_with(&self.root) {
            return Err(CommandError::Failed(format!("{relative} is outside the project folder")));
        }
        if !(real.is_file() && real.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("py"))) {
            return Err(CommandError::Failed(format!("{relative} is not a Python script")));
        }
        Ok(real)
    }

    /// Spawns the script and books it into the run table, so a run started by
    /// hand and one started by a timer are the same kind of run: same Python,
    /// same output stream, same stop.
    fn spawn(&self, path: &str) -> Result<(tokio::process::Child, u32, oneshot::Receiver<()>), CommandError> {
        let script = self.script_path(path)?;
        // The person's choice first (ADR-0020), then the project's own.
        let python = self.python_override().unwrap_or_else(|| python_for(&self.root));

        let mut command = tokio::process::Command::new(&python);
        command
            .arg("-u")
            .arg(&script)
            .current_dir(&self.root)
            .env("PYTHONUTF8", "1")
            .env("PYTHONIOENCODING", "utf-8")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // No console window flashing up for every run.
        #[cfg(windows)]
        command.creation_flags(0x0800_0000);

        let child = command.spawn().map_err(|err| {
            CommandError::Failed(if err.kind() == std::io::ErrorKind::NotFound {
                format!(
                    "no Python found: looked for a .venv in {} and for `python` on the PATH. \
                     Create one with `uv venv` in the project, or install Python",
                    self.root.display()
                )
            } else {
                format!("could not start {}: {err}", python.display())
            })
        })?;

        let (run, stopped) = self.book(format!("{} {path}", python.display()))?;
        Ok((child, run, stopped))
    }

    /// Gives a run its number and its stop channel, and announces it.
    fn book(&self, info: String) -> Result<(u32, oneshot::Receiver<()>), CommandError> {
        let (stop, stopped) = oneshot::channel();
        let run = {
            let mut guard = self.runs.lock().map_err(|_| CommandError::Failed("run table poisoned".to_owned()))?;
            guard.0 += 1;
            let run = guard.0;
            guard.1.insert(run, stop);
            run
        };
        self.send(run, "info", info);
        Ok((run, stopped))
    }

    fn send(&self, run: u32, stream: &str, text: String) {
        // A send with no subscribers is not a failure: nobody is watching,
        // and the run is not for the watchers' benefit.
        let _ = self.output.send(ScriptOutputView { run, stream: stream.to_owned(), text });
    }

    /// Streams a run's output until it ends, and says how it ended.
    async fn stream_to_end(
        self: &Arc<Self>,
        mut child: tokio::process::Child,
        run: u32,
        stopped: oneshot::Receiver<()>,
    ) -> String {
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let forward = |stream: &'static str| {
            let scripts = Arc::clone(self);
            move |text| scripts.send(run, stream, text)
        };
        let out = stdout.map(|from| tokio::spawn(pump(from, forward("out"))));
        let err = stderr.map(|from| tokio::spawn(pump(from, forward("err"))));

        let ended = tokio::select! {
            status = child.wait() => match status {
                Ok(status) => status.code().map_or_else(|| "ended".to_owned(), |code| format!("exited with code {code}")),
                Err(err) => format!("could not wait for the script: {err}"),
            },
            _ = stopped => {
                let _ = child.kill().await;
                "stopped".to_owned()
            }
        };
        // Every line before the exit line, so the last thing shown is how it
        // ended.
        for pumping in [out, err].into_iter().flatten() {
            let _ = pumping.await;
        }
        self.send(run, "exit", ended.clone());
        if let Ok(mut guard) = self.runs.lock() {
            guard.1.remove(&run);
        }
        ended
    }

    /// The interpreter the person chose for this project, if any. User
    /// settings, then the project's own, which wins.
    fn python_override(&self) -> Option<PathBuf> {
        let read = |path: PathBuf| -> serde_json::Map<String, serde_json::Value> {
            std::fs::read_to_string(path)
                .ok()
                .and_then(|text| serde_json::from_str(&text).ok())
                .unwrap_or_default()
        };
        let mut settings = read(self.state.join(USER_SETTINGS));
        settings.extend(read(self.root.join(PROJECT_SETTINGS)));
        python_from(&settings, &self.root)
    }
}

/// The Python a project's scripts run with: its own virtual environment when
/// it has one, otherwise whatever `python` is on the PATH.
///
/// The project's own first, because that is where its dependencies — the
/// `arvo` package among them — were installed.
#[must_use]
pub fn python_for(root: &Path) -> PathBuf {
    [".venv/Scripts/python.exe", ".venv/bin/python", "venv/Scripts/python.exe", "venv/bin/python"]
        .iter()
        .map(|candidate| root.join(candidate))
        .find(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("python"))
}

/// The interpreter a settings map names for `root`, if any. A relative path
/// is relative to the project, which is where a `.venv` lives.
#[must_use]
pub fn python_from(settings: &serde_json::Map<String, serde_json::Value>, root: &Path) -> Option<PathBuf> {
    let named = settings.get("python.defaultInterpreterPath")?.as_str()?.trim();
    if named.is_empty() {
        return None;
    }
    let path = PathBuf::from(named);
    Some(if path.is_absolute() { path } else { root.join(path) })
}

/// Hands every line of `from` to `line`, lossily for bytes that are not
/// UTF-8 — a script printing binary still has its other lines shown.
async fn pump(from: impl AsyncRead + Unpin, mut line: impl FnMut(String)) {
    let mut reader = BufReader::new(from);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(_) => line(String::from_utf8_lossy(&buffer).trim_end_matches(['\n', '\r']).to_owned()),
        }
    }
}

/// One path segment, if it is a file or folder name and not a way out of the
/// project.
fn name(segment: &str) -> Result<&str, String> {
    let trimmed = segment.trim();
    let bad = trimmed.is_empty()
        || trimmed == "."
        || trimmed == ".."
        || trimmed.chars().any(|c| matches!(c, '/' | '\\' | ':') || c.is_control())
        || Path::new(trimmed).is_absolute();
    if bad {
        Err(format!("{segment:?} is not a file or folder name"))
    } else {
        Ok(trimmed)
    }
}

/// The job id a scheduled script registers under. Prefixed so it cannot
/// collide with one of the engine's own jobs.
fn job_id(script: &str) -> String {
    format!("script:{script}")
}

/// Puts `job` into `all`, replacing the schedule for that script if there is
/// one. Kept apart from the file so the rule that makes it one-per-script is
/// testable without a running engine.
fn upsert(all: &mut Vec<ScriptJobView>, job: ScriptJobView) {
    match all.iter_mut().find(|saved| saved.script == job.script) {
        Some(saved) => *saved = job,
        None => all.push(job),
    }
    all.sort_by(|a, b| a.script.cmp(&b.script));
}

/// A cron expression in the spelling the `cron` crate wants: six fields,
/// seconds first.
///
/// People write five-field Unix cron, and the two dialects disagree about the
/// day of the week. Unix counts Sunday as 0 (and accepts 7 for it); this crate
/// counts Sunday as 1. So `1-5` is Monday-to-Friday in one and
/// Sunday-to-Thursday in the other, and prepending a seconds field without
/// touching the rest would run a weekday script on Sunday and not on Friday —
/// silently, and only noticeably a week later. Hence the translation.
///
/// Six- and seven-field expressions are passed through untouched: someone who
/// writes a seconds field is speaking the crate's dialect already.
fn normalize_cron(expression: &str) -> Result<String, String> {
    let fields: Vec<&str> = expression.split_whitespace().collect();
    match fields[..] {
        [minute, hour, day, month, weekday] => {
            Ok(format!("0 {minute} {hour} {day} {month} {}", unix_weekdays(weekday)?))
        }
        [..] if fields.len() == 6 || fields.len() == 7 => Ok(fields.join(" ")),
        [..] => Err(format!(
            "a cron expression has 5 fields (minute hour day month weekday), or 6 with seconds first; \
             this one has {}",
            fields.len()
        )),
    }
}

/// A Unix day-of-week field in the `cron` crate's numbering, leaving `*`,
/// names and step values alone. `1,3-5/2` and `Mon-Fri` both survive.
fn unix_weekdays(field: &str) -> Result<String, String> {
    let shifted: Result<Vec<String>, String> = field
        .split(',')
        .map(|item| {
            let (base, step) = item.split_once('/').map_or((item, None), |(base, step)| (base, Some(step)));
            let days: Result<Vec<String>, String> = base.split('-').map(shift_weekday).collect();
            let days = days?.join("-");
            Ok(step.map_or_else(|| days.clone(), |step| format!("{days}/{step}")))
        })
        .collect();
    Ok(shifted?.join(","))
}

/// One day of the week: Unix 0-6 (Sunday first, 7 also Sunday) becomes the
/// crate's 1-7. Anything that is not a number — `*`, `Mon` — is spelled the
/// same in both and is left alone.
fn shift_weekday(day: &str) -> Result<String, String> {
    match day.trim().parse::<u8>() {
        Ok(0 | 7) => Ok("1".to_owned()),
        Ok(number) if number <= 6 => Ok((number + 1).to_string()),
        Ok(number) => Err(format!("{number} is not a day of the week: use 0-6 with Sunday as 0, or a name like Mon")),
        Err(_) => Ok(day.trim().to_owned()),
    }
}

/// The parsed schedule for an expression a person typed.
fn parse_cron(expression: &str) -> Result<cron::Schedule, String> {
    let normalized = normalize_cron(expression)?;
    let schedule = cron::Schedule::from_str(&normalized).map_err(|err| {
        // The crate's message is the expression, a caret under the offending
        // field, then what is wrong with it. Only the last line is worth
        // putting in front of a person, and it is not always there.
        let said = err.to_string();
        let detail = said
            .lines()
            .map(str::trim)
            .rfind(|line| !line.is_empty() && !line.starts_with('^') && *line != normalized)
            .unwrap_or_default()
            .to_owned();
        if detail.is_empty() {
            format!("{expression} is not a cron expression")
        } else {
            format!("{expression} is not a cron expression: {detail}")
        }
    })?;

    // ponytail: the gap between the next two runs stands in for the cadence,
    // which is right for every expression anyone writes and wrong for a
    // contrived one that fires twice and then not for a year. Compare more
    // occurrences if that ever matters.
    let mut upcoming = schedule.upcoming(Local);
    if let (Some(first), Some(second)) = (upcoming.next(), upcoming.next()) {
        let gap = (second - first).num_seconds();
        if gap < i64::try_from(FLOOR_SECS).unwrap_or(i64::MAX) {
            return Err(format!("{expression} would run every {gap} seconds; the closest allowed is {FLOOR_SECS}"));
        }
    }
    Ok(schedule)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schedule(script: &str, every_secs: u64) -> ScriptJobView {
        ScriptJobView { script: script.to_owned(), every_secs, cron: None, enabled: true }
    }

    #[test]
    fn one_schedule_per_script_and_saving_again_replaces_it() {
        let mut all = Vec::new();
        upsert(&mut all, schedule("scan.py", 300));
        upsert(&mut all, schedule("a.py", 60));
        upsert(&mut all, ScriptJobView { enabled: false, ..schedule("scan.py", 900) });

        assert_eq!(all.len(), 2, "the second scan.py replaced the first");
        assert_eq!(all[0].script, "a.py", "sorted, so the list does not jump about");
        assert_eq!(all[1].every_secs, 900);
        assert!(!all[1].enabled);
        assert_eq!(job_id("scan.py"), "script:scan.py");
    }

    /// The whole reason five-field expressions are translated rather than
    /// padded: the same text means different days in the two dialects.
    #[test]
    fn a_unix_weekday_field_keeps_its_days_through_the_translation() {
        assert_eq!(normalize_cron("35 9 * * 1-5").expect("weekdays"), "0 35 9 * * 2-6");
        assert_eq!(normalize_cron("0 9 * * 0").expect("sunday"), "0 0 9 * * 1");
        assert_eq!(normalize_cron("0 9 * * 7").expect("sunday again"), "0 0 9 * * 1");
        assert_eq!(normalize_cron("0 9 * * 6").expect("saturday"), "0 0 9 * * 7");
        assert_eq!(normalize_cron("0 9 * * 1,3,5").expect("a list"), "0 0 9 * * 2,4,6");
        assert_eq!(normalize_cron("0 9 * * 1-5/2").expect("a step"), "0 0 9 * * 2-6/2");
        assert_eq!(normalize_cron("0 9 * * Mon-Fri").expect("names"), "0 0 9 * * Mon-Fri");
        assert_eq!(normalize_cron("0 9 * * *").expect("every day"), "0 0 9 * * *");

        // Six fields are the crate's own dialect and are not touched.
        assert_eq!(normalize_cron("0 0 9 * * 2-6").expect("passed through"), "0 0 9 * * 2-6");
        assert!(normalize_cron("0 9 * *").is_err(), "four fields is not cron");
        assert!(normalize_cron("0 9 * * 9").is_err(), "there is no ninth day");
    }

    #[test]
    fn a_five_field_weekday_expression_lands_on_the_days_it_names() {
        let schedule = parse_cron("35 9 * * 1-5").expect("Monday to Friday at 09:35");
        let days: Vec<chrono::Weekday> = schedule
            .upcoming(Local)
            .take(10)
            .map(|at| chrono::Datelike::weekday(&at))
            .collect();
        assert!(
            days.iter().all(|day| !matches!(day, chrono::Weekday::Sat | chrono::Weekday::Sun)),
            "a weekday expression ran at the weekend: {days:?}"
        );
        assert!(days.contains(&chrono::Weekday::Fri), "Friday is a weekday: {days:?}");
        assert!(days.contains(&chrono::Weekday::Mon), "Monday is a weekday: {days:?}");
    }

    #[test]
    fn a_schedule_tighter_than_the_floor_is_refused_rather_than_run() {
        assert!(parse_cron("* * * * * *").is_err(), "every second");
        assert!(parse_cron("0,30 * * * * *").is_err(), "twice a minute");
        assert!(parse_cron("0 * * * * *").is_ok(), "every minute is the floor, not under it");
        assert!(parse_cron("nonsense").is_err());
    }

    #[test]
    fn a_project_venv_is_preferred_over_the_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(python_for(dir.path()), PathBuf::from("python"), "no venv");

        let scripts = dir.path().join(".venv").join("Scripts");
        std::fs::create_dir_all(&scripts).expect("mkdir");
        std::fs::write(scripts.join("python.exe"), "").expect("write");
        assert_eq!(python_for(dir.path()), scripts.join("python.exe"));
    }

    #[test]
    fn only_a_python_file_inside_the_project_is_a_script() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(dir.path()).expect("canonical");
        std::fs::write(root.join("scan.py"), "").expect("file");
        std::fs::write(root.join("notes.txt"), "").expect("file");
        std::fs::create_dir(root.join("pkg")).expect("folder");
        let scripts = Scripts::new(root, dir.path().join("state"), arvo_schedule::Jobs::new(std::sync::Arc::new(|_| Box::new(|| {}))));

        assert!(scripts.script_path("scan.py").is_ok());
        assert!(scripts.script_path("notes.txt").is_err(), "not Python");
        assert!(scripts.script_path("pkg").is_err(), "a folder is not a script");
        assert!(scripts.script_path("../scan.py").is_err(), "outside the project");
    }

    /// A schedule saved while the window was open used to stop when it
    /// closed. It is written where the engine reads it, and read back here.
    #[test]
    fn a_saved_schedule_is_read_back_from_the_engines_own_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dunce::canonicalize(dir.path()).expect("canonical");
        std::fs::write(root.join("scan.py"), "").expect("file");
        let state = root.join("state");
        let jobs = || arvo_schedule::Jobs::new(std::sync::Arc::new(|_| Box::new(|| {})));

        let scripts = Scripts::new(root.clone(), state.clone(), jobs());
        let all = scripts.save("scan.py".to_owned(), 300, None, true).expect("saves");
        assert_eq!(all.len(), 1);

        // A second engine over the same state finds it, which is the whole
        // point of the move.
        let restarted = Scripts::new(root, state, jobs());
        assert_eq!(restarted.saved(), all);
        assert_eq!(restarted.saved()[0].every_secs, 300);
    }
}
