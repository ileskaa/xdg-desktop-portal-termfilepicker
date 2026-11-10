//! The interactive half: runs inside the terminal, drives Yazi, asks for save
//! names and confirmations, and writes the outcome to the private workspace.
//! It never creates or touches files outside that workspace.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

use crate::config::Config;
use crate::protocol::{self, Mode, PickRequest, REQUEST_FILE, RESULT_FILE};
use crate::validate::{is_deceptive_char, is_plain_filename, path_from_bytes, split_extension};
use crate::workspace::{read_private, remove_if_exists, write_private};

const CHOSEN_FILE: &str = "chosen";
const CWD_FILE: &str = "cwd";

pub fn main(workspace: &Path) -> ExitCode {
    match run(workspace) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("termfilepicker: {e}");
            let _ = DefaultEditor::new().and_then(|mut ed| ed.readline("Press Enter to close… "));
            ExitCode::FAILURE
        }
    }
}

fn run(workspace: &Path) -> Result<(), String> {
    let request = read_private(workspace, REQUEST_FILE)
        .map_err(|e| format!("cannot read request: {e}"))?
        .and_then(|b| PickRequest::decode(&b))
        .ok_or("missing or malformed request")?;
    let config = Config::load()?;
    let mut picker = Picker {
        yazi: Yazi {
            bin: config.yazi(),
            workspace: workspace.to_owned(),
        },
        ui: Ui::new(intro(&request.app_id, &request.title))?,
        dir: request.start_dir,
    };
    let chosen = match &request.mode {
        Mode::Open {
            multiple,
            directory,
        } => picker.open(*multiple, *directory)?,
        Mode::Save { name } => picker.save(name)?,
        Mode::SaveMany { names } => picker.save_many(names)?,
    };
    match chosen {
        Some(paths) => write_private(workspace, RESULT_FILE, &protocol::encode_paths(&paths))
            .map_err(|e| format!("cannot write result: {e}")),
        None => Ok(()),
    }
}

enum Step {
    Done(PathBuf),
    Retry,
    Cancel,
}

struct Picker {
    yazi: Yazi,
    ui: Ui,
    dir: PathBuf,
}

impl Picker {
    fn browse(&mut self) -> Result<Outcome, String> {
        let outcome = self.yazi.run(&self.dir)?;
        if let Some(cwd) = &outcome.cwd {
            self.dir = cwd.clone();
        }
        Ok(outcome)
    }

    fn open(&mut self, multiple: bool, directory: bool) -> Result<Option<Vec<PathBuf>>, String> {
        loop {
            let out = self.browse()?;
            if out.chosen.is_empty() && !directory {
                return Ok(None);
            }
            let step = match selection_problem(&out.chosen, multiple, directory) {
                _ if out.chosen.is_empty() => self.confirm_folder(out.cwd),
                None => return Ok(Some(out.chosen)),
                Some(msg) => self.retry_or_cancel(msg),
            };
            match step {
                Step::Done(dir) => return Ok(Some(vec![dir])),
                Step::Retry => continue,
                Step::Cancel => return Ok(None),
            }
        }
    }

    /// Directory mode: quitting with `q` offers Yazi's current folder; `Q` cancels.
    fn confirm_folder(&mut self, cwd: Option<PathBuf>) -> Step {
        let Some(cwd) = cwd else { return Step::Cancel };
        match self
            .ui
            .confirm(&format!("Select folder {}?", shown(&cwd)), true)
        {
            Answer::Yes => Step::Done(cwd),
            Answer::No => Step::Retry,
            Answer::Cancel => Step::Cancel,
        }
    }

    fn save(&mut self, suggested: &OsStr) -> Result<Option<Vec<PathBuf>>, String> {
        loop {
            let out = self.browse()?;
            let step = match (out.chosen.as_slice(), out.cwd) {
                ([], None) => Step::Cancel,
                ([], Some(cwd)) => self.ask_name(&cwd, suggested),
                ([file], _) => self.confirm_target(file.clone()),
                _ if self.ui.notice(SAVE_HELP) => Step::Retry,
                _ => Step::Cancel,
            };
            match step {
                Step::Done(path) => return Ok(Some(vec![path])),
                Step::Retry => continue,
                Step::Cancel => return Ok(None),
            }
        }
    }

    fn ask_name(&mut self, dir: &Path, suggested: &OsStr) -> Step {
        self.ui.heading();
        println!("Saving into {}", shown(dir));
        let suggested_text = suggested.to_string_lossy();
        let initial = split_extension(&suggested_text);
        loop {
            let input = match self.ui.edit("Save as: ", initial) {
                Ok(line) => line,
                Err(ReadlineError::Interrupted) => return Step::Retry,
                Err(_) => return Step::Cancel,
            };
            let name = match input.as_str() {
                "" => suggested.to_owned(),
                typed => OsString::from(typed),
            };
            if is_plain_filename(&name) {
                return self.confirm_target(dir.join(name));
            }
            println!("Not a valid file name (no '/' or control characters, not '.' or '..').");
        }
    }

    fn confirm_target(&mut self, path: PathBuf) -> Step {
        let meta = match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Step::Done(path),
            Err(_) => return self.retry_or_cancel("That location cannot be inspected."),
            Ok(meta) => meta,
        };
        if meta.is_dir() || path.is_dir() {
            return self.retry_or_cancel("That is a folder; choose or type a file name.");
        }
        match self
            .ui
            .confirm(&format!("Replace existing {}?", shown(&path)), false)
        {
            Answer::Yes => Step::Done(path),
            Answer::No => Step::Retry,
            Answer::Cancel => Step::Cancel,
        }
    }

    fn retry_or_cancel(&mut self, msg: &str) -> Step {
        if self.ui.notice(msg) {
            Step::Retry
        } else {
            Step::Cancel
        }
    }

    fn save_many(&mut self, names: &[OsString]) -> Result<Option<Vec<PathBuf>>, String> {
        loop {
            let out = self.browse()?;
            let step = match (out.chosen.is_empty(), out.cwd) {
                (true, None) => Step::Cancel,
                (true, Some(dir)) => self.confirm_many(&dir, names),
                (false, _) => self.retry_or_cancel(SAVE_MANY_HELP),
            };
            match step {
                Step::Done(dir) => return Ok(Some(names.iter().map(|n| dir.join(n)).collect())),
                Step::Retry => continue,
                Step::Cancel => return Ok(None),
            }
        }
    }

    fn confirm_many(&mut self, dir: &Path, names: &[OsString]) -> Step {
        let existing: Vec<PathBuf> = names
            .iter()
            .map(|n| dir.join(n))
            .filter(|p| std::fs::symlink_metadata(p).is_ok())
            .collect();
        if existing.iter().any(|p| p.is_dir()) {
            return self.retry_or_cancel("A folder there has the same name as a file to save.");
        }
        for path in &existing {
            println!("  exists: {}", shown(path));
        }
        let question = match existing.len() {
            0 => format!("Save {} file(s) into {}?", names.len(), shown(dir)),
            n => format!("Replace {n} existing file(s) in {}?", shown(dir)),
        };
        match self.ui.confirm(&question, existing.is_empty()) {
            Answer::Yes => Step::Done(dir.to_owned()),
            Answer::No => Step::Retry,
            Answer::Cancel => Step::Cancel,
        }
    }
}

const DIR_HELP: &str =
    "Select folders: mark with Space and press Enter, or quit with q inside the folder.";
const SAVE_HELP: &str = "Choose one existing file to replace, or quit with q inside the target folder to type a new name.";
const SAVE_MANY_HELP: &str = "Quit with q inside the folder to save into.";

fn selection_problem(chosen: &[PathBuf], multiple: bool, directory: bool) -> Option<&'static str> {
    if directory && chosen.iter().any(|p| !p.is_dir()) {
        return Some(DIR_HELP);
    }
    if !directory && chosen.iter().any(|p| p.is_dir()) {
        return Some("Only files can be chosen here.");
    }
    if !multiple && chosen.len() > 1 {
        return Some("Only one item can be chosen here.");
    }
    None
}

/// Paths come from the file system and may contain terminal control sequences.
fn shown(path: &Path) -> String {
    escaped(&path.to_string_lossy())
}

fn escaped(text: &str) -> String {
    text.chars()
        .flat_map(|c| -> Box<dyn Iterator<Item = char>> {
            if is_deceptive_char(c) {
                Box::new(c.escape_unicode())
            } else {
                Box::new(std::iter::once(c))
            }
        })
        .collect()
}

/// The app id comes from xdg-desktop-portal; for unsandboxed apps it is empty
/// or self-declared, so it is labelled as such.
fn intro(app_id: &str, title: &str) -> String {
    let who = if app_id.is_empty() {
        "An unsandboxed app".to_string()
    } else {
        escaped(app_id)
    };
    match title {
        "" => format!("{who} asks for a file."),
        t => format!("{who} asks: {}", escaped(t)),
    }
}

struct Outcome {
    chosen: Vec<PathBuf>,
    cwd: Option<PathBuf>,
}

struct Yazi {
    bin: PathBuf,
    workspace: PathBuf,
}

impl Yazi {
    /// The start folder is passed as the process cwd so it never appears in argv.
    fn run(&self, dir: &Path) -> Result<Outcome, String> {
        let chooser = self.workspace.join(CHOSEN_FILE);
        let cwd_file = self.workspace.join(CWD_FILE);
        for stale in [&chooser, &cwd_file] {
            remove_if_exists(stale).map_err(|e| format!("cannot reset workspace: {e}"))?;
        }
        let status = Command::new(&self.bin)
            .arg(flag("--chooser-file=", &chooser))
            .arg(flag("--cwd-file=", &cwd_file))
            .current_dir(dir)
            .status()
            .map_err(|e| format!("cannot start {}: {e}", self.bin.display()))?;
        if !status.success() {
            return Err(format!("yazi failed ({status})"));
        }
        let read = |name| {
            read_private(&self.workspace, name).map_err(|e| format!("cannot read yazi output: {e}"))
        };
        let chosen = read(CHOSEN_FILE)?
            .map(|b| parse_chosen(&b))
            .unwrap_or_default();
        let cwd = read(CWD_FILE)?
            .and_then(|b| path_from_bytes(b.strip_suffix(b"\n").unwrap_or(&b)))
            .filter(|p| p.is_absolute());
        Ok(Outcome { chosen, cwd })
    }
}

fn flag(name: &str, path: &Path) -> OsString {
    let mut arg = OsString::from(name);
    arg.push(path);
    arg
}

/// Yazi writes one absolute path per line.
fn parse_chosen(bytes: &[u8]) -> Vec<PathBuf> {
    bytes
        .split(|b| *b == b'\n')
        .filter_map(path_from_bytes)
        .filter(|p| p.is_absolute())
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
enum Answer {
    Yes,
    No,
    Cancel,
}

struct Ui {
    editor: DefaultEditor,
    intro: String,
}

impl Ui {
    fn new(intro: String) -> Result<Self, String> {
        let editor =
            DefaultEditor::new().map_err(|e| format!("cannot open terminal input: {e}"))?;
        Ok(Self { editor, intro })
    }

    /// Every prompt names the requesting app, so a prompt cannot be passed off as another app's.
    fn heading(&self) {
        println!("\n{}", self.intro);
    }

    fn confirm(&mut self, question: &str, default_yes: bool) -> Answer {
        self.heading();
        let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
        loop {
            let Ok(line) = self.editor.readline(&format!("{question} {hint} ")) else {
                return Answer::Cancel;
            };
            if let Some(answer) = parse_answer(&line, default_yes) {
                return answer;
            }
        }
    }

    /// Shows `msg`; `false` if the user closed input instead of continuing.
    fn notice(&mut self, msg: &str) -> bool {
        self.heading();
        println!("{msg}");
        self.editor
            .readline("Press Enter to return to Yazi… ")
            .is_ok()
    }

    fn edit(&mut self, prompt: &str, initial: (&str, &str)) -> Result<String, ReadlineError> {
        self.editor.readline_with_initial(prompt, initial)
    }
}

fn parse_answer(line: &str, default_yes: bool) -> Option<Answer> {
    match line.trim().to_ascii_lowercase().as_str() {
        "" if default_yes => Some(Answer::Yes),
        "" => Some(Answer::No),
        "y" | "yes" => Some(Answer::Yes),
        "n" | "no" => Some(Answer::No),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chosen_lines_ignore_blank_and_relative_entries() {
        let parsed = parse_chosen(b"/a/b\n\nrel\n/c d\n");
        assert_eq!(parsed, [PathBuf::from("/a/b"), PathBuf::from("/c d")]);
    }

    #[test]
    fn answers_respect_default() {
        assert_eq!(parse_answer("", false), Some(Answer::No));
        assert_eq!(parse_answer(" Y ", false), Some(Answer::Yes));
        assert_eq!(parse_answer("", true), Some(Answer::Yes));
        assert_eq!(parse_answer("maybe", true), None);
    }

    #[test]
    fn shown_escapes_terminal_control_sequences() {
        let evil = Path::new("/tmp/\x1b]0;pwned\x07name");
        let out = shown(evil);
        assert!(!out.contains('\x1b') && !out.contains('\x07'), "{out}");
    }

    #[test]
    fn shown_escapes_bidi_and_separators() {
        let out = shown(Path::new("/tmp/invoice\u{202E}fdp.exe\u{2028}x"));
        assert!(!out.chars().any(is_deceptive_char), "{out}");
        assert!(out.contains("\\u{202e}"), "{out}");
    }

    #[test]
    fn intro_names_the_app_and_escapes_its_text() {
        assert_eq!(intro("", ""), "An unsandboxed app asks for a file.");
        let out = intro("org.x\x1b[2J", "Up\u{202E}load");
        assert!(out.starts_with("org.x"), "{out}");
        assert!(!out.chars().any(is_deceptive_char), "{out}");
    }

    #[test]
    fn selection_rules() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, b"").unwrap();
        let d = dir.path().to_owned();
        assert!(selection_problem(std::slice::from_ref(&file), false, false).is_none());
        assert!(selection_problem(std::slice::from_ref(&d), false, false).is_some());
        assert!(selection_problem(&[file.clone(), file.clone()], false, false).is_some());
        assert!(selection_problem(std::slice::from_ref(&d), false, true).is_none());
        assert!(selection_problem(&[file], true, true).is_some());
    }
}
