//! The project folder: where a person's scripts live (#121).
//!
//! One folder, chosen by the person so it can be a git repository, shown as
//! the Files view. Every command here is confined to it: a path from the
//! window is relative to the folder, checked segment by segment, and the
//! resolved location must still be inside the folder once links are followed.
//! That is what keeps the keychain, the evidence store and the rest of the
//! disk out of reach of anything the file tree does.
//!
//! Called the *project* folder because "workspace" already means a saved panel
//! arrangement in Settings.
//!
//! Deleting moves to the recycle bin rather than removing, because a tree view
//! is exactly where a mis-click happens and a script is exactly what someone
//! cannot get back.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use arvo_views::{FileContentView, FileEntryView, GitStatusView, ProjectFolderView, SearchHitView};

/// When `path` last changed, in milliseconds since the epoch, or 0 when the
/// file system cannot say. What a save compares against.
#[must_use]
pub fn modified(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

/// Where the chosen folder is remembered, in the app data directory. Not in
/// `session.json`, which is the window's layout; the folder is the engine's to
/// know (ADR-0018).
pub const FILE: &str = "project.json";

/// The largest file the editor opens. Scripts are kilobytes; a file past this
/// is data or a build artefact, and loading it into a text editor helps nobody.
pub const MAX_EDITABLE_BYTES: u64 = 2_000_000;

/// How a refused save says so; the window looks for it to offer Overwrite
/// or Reload rather than a plain error.
pub const CHANGED_ON_DISK: &str = "changed on disk since it was read";

/// Folders that are noise in a script tree and slow to list.
const HIDDEN: &[&str] = &[".git", ".venv", "venv", "__pycache__", "node_modules", ".pytest_cache"];

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Stored {
    pub folder: Option<PathBuf>,
}

/// A project folder, resolved to its real location.
#[derive(Debug, Clone)]
pub struct Project {
    root: PathBuf,
}

impl Project {
    /// Opens `folder`, which must exist and be a directory.
    ///
    /// # Errors
    ///
    /// When it does not exist or is not a directory.
    pub fn open(folder: &Path) -> Result<Self, String> {
        let root = dunce::canonicalize(folder)
            .map_err(|err| format!("{} cannot be opened: {err}", folder.display()))?;
        if !root.is_dir() {
            return Err(format!("{} is not a folder", root.display()));
        }
        Ok(Self { root })
    }

    #[must_use]
    pub fn view(&self) -> ProjectFolderView {
        ProjectFolderView {
            root: self.root.display().to_string(),
            name: self
                .root
                .file_name()
                .map_or_else(|| self.root.display().to_string(), |name| name.to_string_lossy().into_owned()),
        }
    }

    /// An existing file or folder, from a `/`-separated path relative to the
    /// root. `""` is the root itself.
    pub fn existing(&self, relative: &str) -> Result<PathBuf, String> {
        let mut path = self.root.clone();
        for segment in relative.split('/').filter(|segment| !segment.is_empty()) {
            path.push(name(segment)?);
        }
        let real = dunce::canonicalize(&path).map_err(|err| format!("{relative}: {err}"))?;
        if !real.starts_with(&self.root) {
            return Err(format!("{relative} is outside the project folder"));
        }
        Ok(real)
    }

    /// A path relative to the root, `/`-separated, for the window.
    fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .components()
            .filter_map(|part| match part {
                Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/")
    }

    /// A folder's entries: folders first, then files, each by name.
    ///
    /// # Errors
    ///
    /// When `relative` is not a folder inside the project.
    pub fn list(&self, relative: &str) -> Result<Vec<FileEntryView>, String> {
        let dir = self.existing(relative)?;
        let mut entries: Vec<FileEntryView> = std::fs::read_dir(&dir)
            .map_err(|err| format!("{relative}: {err}"))?
            .flatten()
            .filter(|entry| !HIDDEN.contains(&entry.file_name().to_string_lossy().as_ref()))
            .map(|entry| {
                let path = entry.path();
                FileEntryView {
                    name: entry.file_name().to_string_lossy().into_owned(),
                    is_dir: path.is_dir(),
                    path: self.relative(&path),
                }
            })
            .collect();
        entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(entries)
    }

    /// Every file in the project, as relative paths, for Go to File. The
    /// same names are skipped as in a listing, at any depth, so a `.venv`
    /// costs nothing. Capped: a palette over more paths than this is not
    /// one anyone scrolls.
    #[must_use]
    pub fn files(&self) -> Vec<String> {
        const CAP: usize = 20_000;
        self.walk().map(|path| self.relative(&path)).take(CAP).collect()
    }

    /// Every file under the root, skipping the noise folders at any depth.
    fn walk(&self) -> impl Iterator<Item = PathBuf> + '_ {
        walkdir::WalkDir::new(&self.root)
            .sort_by_file_name()
            .into_iter()
            .filter_entry(|entry| {
                entry.depth() == 0 || !HIDDEN.contains(&entry.file_name().to_string_lossy().as_ref())
            })
            .flatten()
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| entry.into_path())
    }

    /// Every line containing `needle`, case-insensitively, in every text
    /// file the editor would open. Capped, because a search that matches
    /// more lines than this needs a better needle, not a longer list.
    ///
    /// ponytail: a substring over every file on every search; an index or a
    /// regex option when a project grows past a few thousand files.
    #[must_use]
    pub fn search(&self, needle: &str) -> Vec<SearchHitView> {
        const CAP: usize = 2_000;
        const LINE_CAP: usize = 200;
        let needle = needle.to_lowercase();
        if needle.is_empty() {
            return Vec::new();
        }
        let mut hits = Vec::new();
        for path in self.walk() {
            if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > MAX_EDITABLE_BYTES) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let relative = self.relative(&path);
            for (index, line) in text.lines().enumerate() {
                let Some(at) = line.to_lowercase().find(&needle) else { continue };
                let column = line[..at.min(line.len())].chars().count() + 1;
                let shown: String = line.trim().chars().take(LINE_CAP).collect();
                hits.push(SearchHitView { path: relative.clone(), line: index as u32 + 1, column: column as u32, text: shown });
                if hits.len() >= CAP {
                    return hits;
                }
            }
        }
        hits
    }

    /// Creates an empty file, or a folder when `folder`, named `new` inside
    /// the folder at `relative`. Refuses to replace anything.
    ///
    /// # Errors
    ///
    /// A bad name, a parent outside the project, or something already there.
    pub fn create(&self, relative: &str, new: &str, folder: bool) -> Result<FileEntryView, String> {
        let parent = self.existing(relative)?;
        if !parent.is_dir() {
            return Err(format!("{relative} is not a folder"));
        }
        let path = parent.join(name(new)?);
        let made = if folder {
            std::fs::create_dir(&path)
        } else {
            std::fs::OpenOptions::new().write(true).create_new(true).open(&path).map(|_| ())
        };
        made.map_err(|err| format!("{new}: {err}"))?;
        Ok(FileEntryView {
            name: new.to_owned(),
            is_dir: folder,
            path: self.relative(&path),
        })
    }

    /// Renames the entry at `relative` to `new`, in the same folder.
    ///
    /// # Errors
    ///
    /// A bad name, the root itself, or something already called `new`.
    pub fn rename(&self, relative: &str, new: &str) -> Result<FileEntryView, String> {
        let from = self.existing(relative)?;
        if from == self.root {
            return Err("the project folder itself cannot be renamed here".to_owned());
        }
        let to = from
            .parent()
            .ok_or_else(|| format!("{relative} has no parent"))?
            .join(name(new)?);
        if to.exists() {
            return Err(format!("{new} already exists"));
        }
        std::fs::rename(&from, &to).map_err(|err| format!("{relative}: {err}"))?;
        Ok(FileEntryView {
            name: new.to_owned(),
            is_dir: to.is_dir(),
            path: self.relative(&to),
        })
    }

    /// Moves the entry at `relative` into the folder at `into`, keeping its
    /// name. Refuses to replace anything, and refuses to put a folder inside
    /// itself.
    ///
    /// # Errors
    ///
    /// The root itself, `into` not a folder, something already there, a
    /// folder moved into itself, or the move failing.
    pub fn move_into(&self, relative: &str, into: &str) -> Result<FileEntryView, String> {
        let from = self.existing(relative)?;
        if from == self.root {
            return Err("the project folder itself cannot be moved".to_owned());
        }
        let dir = self.existing(into)?;
        if !dir.is_dir() {
            return Err(format!("{into} is not a folder"));
        }
        if dir.starts_with(&from) {
            return Err(format!("{relative} cannot be moved into itself"));
        }
        let name = from.file_name().ok_or_else(|| format!("{relative} has no file name"))?;
        let to = dir.join(name);
        if to.exists() {
            return Err(format!("{} already exists there", name.to_string_lossy()));
        }
        std::fs::rename(&from, &to).map_err(|err| format!("{relative}: {err}"))?;
        Ok(FileEntryView {
            name: name.to_string_lossy().into_owned(),
            is_dir: to.is_dir(),
            path: self.relative(&to),
        })
    }

    /// What git says about every changed file, or nothing when the folder
    /// is not a repository or git is not installed. A process rather than a
    /// library: one call, and the repo may not be one.
    #[must_use]
    pub fn git_status(&self) -> Vec<GitStatusView> {
        let mut command = std::process::Command::new("git");
        command
            .args(["-C"])
            .arg(&self.root)
            .args(["status", "--porcelain=v1", "-z", "--untracked-files=all"]);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            command.creation_flags(0x0800_0000);
        }
        let Ok(output) = command.output() else { return Vec::new() };
        if !output.status.success() {
            return Vec::new();
        }
        parse_git_status(&String::from_utf8_lossy(&output.stdout))
    }

    /// An existing file's real location, for a tool that wants the path.
    ///
    /// # Errors
    ///
    /// Not a file inside the project.
    pub fn script_path(&self, relative: &str) -> Result<PathBuf, String> {
        let path = self.existing(relative)?;
        if !path.is_file() {
            return Err(format!("{relative} is not a file"));
        }
        Ok(path)
    }

    /// The folder itself.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// A Python script inside the project, ready to run.
    ///
    /// # Errors
    ///
    /// Not an existing `.py` file inside the project.
    pub fn script(&self, relative: &str) -> Result<PathBuf, String> {
        let path = self.existing(relative)?;
        let python = path.is_file()
            && path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("py"));
        if python {
            Ok(path)
        } else {
            Err(format!("{relative} is not a Python script"))
        }
    }

    /// A text file's contents.
    ///
    /// # Errors
    ///
    /// Not a file inside the project, larger than [`MAX_EDITABLE_BYTES`], or
    /// not UTF-8 — an editor that opened a binary would save it back mangled.
    pub fn read(&self, relative: &str) -> Result<FileContentView, String> {
        let path = self.existing(relative)?;
        if !path.is_file() {
            return Err(format!("{relative} is not a file"));
        }
        let size = std::fs::metadata(&path).map_err(|err| format!("{relative}: {err}"))?.len();
        if size > MAX_EDITABLE_BYTES {
            return Err(format!(
                "{relative} is {} MB, larger than the editor opens",
                size / 1_000_000
            ));
        }
        let bytes = std::fs::read(&path).map_err(|err| format!("{relative}: {err}"))?;
        let text = String::from_utf8(bytes).map_err(|_| format!("{relative} is not a UTF-8 text file"))?;
        Ok(FileContentView { text, modified: modified(&path) })
    }

    /// Replaces an existing file's contents, whole or not at all.
    ///
    /// Through a temporary file beside it and a rename, so a failed or
    /// interrupted save leaves the previous version rather than half of the
    /// new one. Only an existing file: creating is [`Self::create`]'s, which
    /// refuses to replace.
    ///
    /// With `expected`, the file's modified time as it was read: a file that
    /// changed on disk since is refused rather than overwritten, and the
    /// editor asks. Returns the modified time after the write, for the next
    /// save to hand back.
    ///
    /// # Errors
    ///
    /// Not an existing file inside the project, changed on disk since
    /// `expected`, or the write failing.
    pub fn write(&self, relative: &str, text: &str, expected: Option<u64>) -> Result<u64, String> {
        let path = self.existing(relative)?;
        if !path.is_file() {
            return Err(format!("{relative} is not a file"));
        }
        if expected.is_some_and(|read_at| modified(&path) != read_at) {
            return Err(format!("{relative} {CHANGED_ON_DISK}"));
        }
        let name = path
            .file_name()
            .ok_or_else(|| format!("{relative} has no file name"))?
            .to_string_lossy();
        let partial = path.with_file_name(format!(".{name}.arvo-saving"));
        std::fs::write(&partial, text).map_err(|err| format!("{relative}: {err}"))?;
        std::fs::rename(&partial, &path).map_err(|err| {
            let _ = std::fs::remove_file(&partial);
            format!("{relative}: {err}")
        })?;
        Ok(modified(&path))
    }

    /// Moves the entry at `relative` to the recycle bin.
    ///
    /// # Errors
    ///
    /// The root itself, a path outside the project, or the move failing.
    pub fn delete(&self, relative: &str) -> Result<(), String> {
        let path = self.existing(relative)?;
        if path == self.root {
            return Err("the project folder itself cannot be deleted here".to_owned());
        }
        trash::delete(&path).map_err(|err| format!("{relative}: {err}"))
    }
}

/// `git status --porcelain=v1 -z` as the tree shows it: one letter per
/// changed path. A rename carries the old path in the entry after it, which
/// is skipped.
#[must_use]
pub fn parse_git_status(text: &str) -> Vec<GitStatusView> {
    let mut out = Vec::new();
    let mut entries = text.split('\0').filter(|entry| !entry.is_empty());
    while let Some(entry) = entries.next() {
        if entry.len() < 4 {
            continue;
        }
        let (code, path) = entry.split_at(3);
        let mut flags = code.chars();
        let (index, work) = (flags.next().unwrap_or(' '), flags.next().unwrap_or(' '));
        if index == 'R' || index == 'C' {
            entries.next();
        }
        let status = match (index, work) {
            ('?', _) => "?",
            ('U', _) | (_, 'U') | ('A', 'A') | ('D', 'D') => "U",
            (_, 'D') | ('D', _) => "D",
            ('A', _) => "A",
            _ => "M",
        };
        out.push(GitStatusView { path: path.trim_end_matches('/').to_owned(), status: status.to_owned() });
    }
    out
}

/// One path segment, refused if it could name anything but a child.
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

/// What a project's `.gitignore` must carry so a push never includes what
/// the person holds: statements, account snapshots, the agent's call log.
/// Market data is ignored for size, not privacy; delete those two lines to
/// track it.
const GITIGNORE: &str = "\
# Arvo — keep personal data out of git
portfolios/
snapshots/
exports/
agent-audit.jsonl
# Arvo — market data, ignored for size; remove to track it
data/
option-quotes/
";

/// The window's own app data directory, `%APPDATA%/com.arvo.desktop`: the
/// same folder Tauri's `app_data_dir` resolves to, for a process without an
/// app handle.
///
/// # Errors
///
/// When `APPDATA` is not set.
pub fn app_data_root() -> Result<PathBuf, String> {
    std::env::var_os("APPDATA")
        .map(|appdata| PathBuf::from(appdata).join("com.arvo.desktop"))
        .ok_or_else(|| "APPDATA is not set".to_owned())
}

/// The project folder that was chosen, from the file alone, so the headless
/// engine and the MCP server find the same data as the window.
#[must_use]
pub fn remembered() -> Option<PathBuf> {
    let stored: Stored = std::fs::read_to_string(app_data_root().ok()?.join(FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    stored.folder.filter(|folder| folder.is_dir())
}

/// Makes sure `root/.gitignore` carries the Arvo block: written whole when
/// there is none, appended when a repository already has its own.
///
/// # Errors
///
/// When the file cannot be read or written.
pub fn ensure_gitignore(root: &Path) -> std::io::Result<()> {
    let path = root.join(".gitignore");
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err),
    };
    if existing.contains("# Arvo") {
        return Ok(());
    }
    let mut text = existing;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(GITIGNORE);
    std::fs::write(path, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> (tempfile::TempDir, Project) {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = Project::open(dir.path()).expect("opens");
        (dir, project)
    }

    #[test]
    fn gitignore_is_written_once_and_appended_to_a_repos_own() {
        let (dir, _) = project();
        let path = dir.path().join(".gitignore");

        ensure_gitignore(dir.path()).expect("writes");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), GITIGNORE);

        ensure_gitignore(dir.path()).expect("no-op");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), GITIGNORE, "not appended twice");

        std::fs::write(&path, "target/").expect("write");
        ensure_gitignore(dir.path()).expect("appends");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), format!("target/\n{GITIGNORE}"));
    }

    #[test]
    fn lists_folders_first_and_hides_the_noise() {
        let (dir, project) = project();
        std::fs::write(dir.path().join("b.py"), "").expect("write");
        std::fs::write(dir.path().join("A.py"), "").expect("write");
        std::fs::create_dir(dir.path().join("strategies")).expect("mkdir");
        std::fs::create_dir(dir.path().join(".venv")).expect("mkdir");
        std::fs::create_dir(dir.path().join("__pycache__")).expect("mkdir");

        let names: Vec<(String, bool)> =
            project.list("").expect("lists").into_iter().map(|e| (e.path, e.is_dir)).collect();
        assert_eq!(
            names,
            [("strategies".to_owned(), true), ("A.py".to_owned(), false), ("b.py".to_owned(), false)]
        );
    }

    #[test]
    fn creates_and_renames_inside_and_refuses_to_replace() {
        let (dir, project) = project();
        let folder = project.create("", "scans", true).expect("folder");
        assert_eq!(folder.path, "scans");
        let file = project.create("scans", "momentum.py", false).expect("file");
        assert_eq!(file.path, "scans/momentum.py");
        assert!(dir.path().join("scans").join("momentum.py").is_file());

        assert!(project.create("scans", "momentum.py", false).is_err(), "no replacing");
        let renamed = project.rename("scans/momentum.py", "trend.py").expect("renamed");
        assert_eq!(renamed.path, "scans/trend.py");
        project.create("scans", "other.py", false).expect("file");
        assert!(project.rename("scans/other.py", "trend.py").is_err(), "no clobbering");
    }

    #[test]
    fn a_saved_file_reads_back_and_leaves_nothing_behind() {
        let (dir, project) = project();
        project.create("", "scan.py", false).expect("file");
        let saved_at = project.write("scan.py", "print('é')\n", None).expect("saved");
        let read = project.read("scan.py").expect("read");
        assert_eq!(read.text, "print('é')\n");
        assert_eq!(read.modified, saved_at, "a save hands back what the next read sees");
        // A stale expectation is refused; the current one is accepted.
        assert!(project.write("scan.py", "x", Some(saved_at.wrapping_sub(1))).unwrap_err().contains(CHANGED_ON_DISK));
        project.write("scan.py", "print('still')\n", Some(saved_at)).expect("the file has not moved on");
        assert_eq!(project.read("scan.py").expect("read").text, "print('still')\n");

        let hits = project.search("STILL");
        assert_eq!(hits.len(), 1, "case-insensitive, one hit: {hits:?}");
        assert_eq!((hits[0].path.as_str(), hits[0].line, hits[0].column), ("scan.py", 1, 8));
        assert!(project.search("").is_empty());

        let parsed = parse_git_status("?? new.py\0 M scan.py\0R  to.py\0from.py\0A  added.py\0UU both.py\0");
        let as_pairs: Vec<(&str, &str)> = parsed.iter().map(|s| (s.path.as_str(), s.status.as_str())).collect();
        assert_eq!(as_pairs, [("new.py", "?"), ("scan.py", "M"), ("to.py", "M"), ("added.py", "A"), ("both.py", "U")]);
        let left: Vec<String> = std::fs::read_dir(dir.path())
            .expect("list")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["scan.py"], "no temporary file survives a save");

        assert!(project.write("new.py", "x", None).is_err(), "save does not create");
        assert!(project.read("").is_err(), "a folder is not a file");

        std::fs::create_dir(dir.path().join("lib")).unwrap();
        let moved = project.move_into("scan.py", "lib").expect("moved");
        assert_eq!(moved.path, "lib/scan.py");
        assert!(project.move_into("lib", "lib").is_err(), "a folder cannot go into itself");
    }

    #[test]
    fn a_binary_or_oversized_file_is_not_opened_as_text() {
        let (dir, project) = project();
        std::fs::write(dir.path().join("data.bin"), [0xff_u8, 0xfe, 0x00]).expect("write");
        assert!(project.read("data.bin").unwrap_err().contains("UTF-8"));
        let big = vec![b'a'; usize::try_from(MAX_EDITABLE_BYTES).expect("fits") + 1];
        std::fs::write(dir.path().join("big.csv"), big).expect("write");
        assert!(project.read("big.csv").unwrap_err().contains("larger"));
    }

    #[test]
    fn nothing_reaches_outside_the_folder() {
        let (dir, project) = project();
        std::fs::create_dir(dir.path().join("inside")).expect("mkdir");
        for escape in ["..", "../..", "inside/../..", "C:", "/etc", "\\Windows"] {
            assert!(project.list(escape).is_err(), "list {escape:?}");
        }
        for bad in ["..", ".", "", "a/b", "a\\b", "C:evil", "\u{0}"] {
            assert!(project.create("", bad, false).is_err(), "create {bad:?}");
            assert!(project.rename("inside", bad).is_err(), "rename to {bad:?}");
        }
        assert!(project.delete("").is_err(), "the root");
        assert!(project.rename("", "x").is_err(), "the root");
        assert!(project.delete("..").is_err());
    }
}
