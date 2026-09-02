use std::borrow::Cow;
use std::env;
use std::ffi::OsString;
use std::io;
use std::path::PathBuf;

#[cfg(test)]
use std::io::BufRead;

use reedline::{
    EditMode as ReedlineEditMode, Emacs, FileBackedHistory, Prompt, PromptEditMode,
    PromptHistorySearch, PromptHistorySearchStatus, PromptViMode, Reedline, Signal, Vi,
};

use crate::cli::EditMode;

const HISTORY_CAPACITY: usize = 1_000;
const MAIN_PROMPT: &str = "agi";

pub(crate) trait LineInput {
    fn read_line(&mut self, prompt: &str) -> io::Result<Option<String>>;
}

pub(crate) struct TerminalInput {
    editor: Reedline,
}

impl TerminalInput {
    pub(crate) fn new(mode: EditMode) -> io::Result<Self> {
        let history = match history_path() {
            Some(path) => FileBackedHistory::with_file(HISTORY_CAPACITY, path),
            None => FileBackedHistory::new(HISTORY_CAPACITY),
        }
        .map_err(reedline_error)?;

        let edit_mode: Box<dyn ReedlineEditMode> = match mode {
            EditMode::Emacs => Box::new(Emacs::default()),
            EditMode::Vim => Box::new(Vi::default()),
        };
        let editor = Reedline::create()
            .with_history(Box::new(history))
            .with_edit_mode(edit_mode);

        Ok(Self { editor })
    }
}

impl LineInput for TerminalInput {
    fn read_line(&mut self, prompt: &str) -> io::Result<Option<String>> {
        let prompt = InputPrompt::new(prompt);

        match self.editor.read_line(&prompt)? {
            Signal::Success(line) => Ok(Some(line.trim().to_owned())),
            Signal::CtrlC => Ok(Some(String::new())),
            Signal::CtrlD => Ok(None),
            Signal::HostCommand(_) | Signal::ExternalBreak(_) => Ok(Some(String::new())),
            _ => Ok(Some(String::new())),
        }
    }
}

struct InputPrompt<'a> {
    text: &'a str,
    is_main: bool,
}

impl<'a> InputPrompt<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            is_main: text == MAIN_PROMPT,
        }
    }
}

impl Prompt for InputPrompt<'_> {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed(self.text)
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, edit_mode: PromptEditMode) -> Cow<'_, str> {
        if !self.is_main {
            return Cow::Borrowed("");
        }

        match edit_mode {
            PromptEditMode::Vi(PromptViMode::Normal) => Cow::Borrowed(" [NORMAL] > "),
            PromptEditMode::Vi(PromptViMode::Visual) => Cow::Borrowed(" [VISUAL] > "),
            PromptEditMode::Vi(PromptViMode::Insert) => Cow::Borrowed(" [INSERT] > "),
            PromptEditMode::Default | PromptEditMode::Emacs | PromptEditMode::Custom(_) => {
                Cow::Borrowed(" > ")
            }
        }
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("... ")
    }

    fn render_prompt_history_search_indicator(
        &self,
        history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        let label = match history_search.status {
            PromptHistorySearchStatus::Passing => "поиск",
            PromptHistorySearchStatus::Failing => "не найдено",
        };
        Cow::Owned(format!("({label}: {}) ", history_search.term))
    }
}

fn history_path() -> Option<PathBuf> {
    history_path_from_lookup(|name| env::var_os(name))
}

fn history_path_from_lookup<F>(mut lookup: F) -> Option<PathBuf>
where
    F: FnMut(&str) -> Option<OsString>,
{
    if let Some(state_dir) = lookup("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(state_dir).join("agi/history.txt"));
    }

    if let Some(home_dir) = lookup("HOME").filter(|value| !value.is_empty()) {
        return Some(
            PathBuf::from(home_dir)
                .join(".local")
                .join("state")
                .join("agi")
                .join("history.txt"),
        );
    }

    lookup("APPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|path| path.join("agi/history.txt"))
}

fn reedline_error(error: reedline::ReedlineError) -> io::Error {
    io::Error::other(error.to_string())
}

#[cfg(test)]
pub(crate) struct BufferedInput<R> {
    inner: R,
}

#[cfg(test)]
impl<R> BufferedInput<R> {
    pub(crate) fn new(inner: R) -> Self {
        Self { inner }
    }
}

#[cfg(test)]
impl<R: BufRead> LineInput for BufferedInput<R> {
    fn read_line(&mut self, _prompt: &str) -> io::Result<Option<String>> {
        read_buffered_line(&mut self.inner)
    }
}

#[cfg(test)]
fn read_buffered_line<R: BufRead>(input: &mut R) -> io::Result<Option<String>> {
    let mut bytes = Vec::new();
    if input.read_until(b'\n', &mut bytes)? == 0 {
        return Ok(None);
    }

    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
        bytes.pop();
    }

    Ok(Some(String::from_utf8_lossy(&bytes).trim().to_owned()))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn reads_russian_utf8() {
        let mut input = BufferedInput::new(Cursor::new("Что такое лось?\n"));

        assert_eq!(
            input
                .read_line("> ")
                .expect("line should be read")
                .as_deref(),
            Some("Что такое лось?")
        );
    }

    #[test]
    fn replaces_invalid_utf8_instead_of_failing() {
        let mut input = BufferedInput::new(Cursor::new(vec![b'/', 0xff, b'\n']));

        assert_eq!(
            input
                .read_line("> ")
                .expect("invalid bytes should be tolerated"),
            Some("/�".to_owned())
        );
    }

    #[test]
    fn prefers_xdg_history_location() {
        let path = history_path_from_lookup(|name| match name {
            "XDG_STATE_HOME" => Some(OsString::from("/state")),
            "HOME" => Some(OsString::from("/home/user")),
            _ => None,
        });

        assert_eq!(path, Some(PathBuf::from("/state/agi/history.txt")));
    }

    #[test]
    fn falls_back_to_home_history_location() {
        let path =
            history_path_from_lookup(|name| (name == "HOME").then(|| OsString::from("/home/user")));

        assert_eq!(
            path,
            Some(PathBuf::from("/home/user/.local/state/agi/history.txt"))
        );
    }
}
