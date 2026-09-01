use std::io;

#[cfg(test)]
use std::io::BufRead;

use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

pub(crate) trait LineInput {
    fn read_line(&mut self, prompt: &str) -> io::Result<Option<String>>;
}

pub(crate) struct TerminalInput {
    editor: DefaultEditor,
}

impl TerminalInput {
    pub(crate) fn new() -> io::Result<Self> {
        let editor = DefaultEditor::new().map_err(readline_error)?;
        Ok(Self { editor })
    }
}

impl LineInput for TerminalInput {
    fn read_line(&mut self, prompt: &str) -> io::Result<Option<String>> {
        match self.editor.readline(prompt) {
            Ok(line) => Ok(Some(line.trim().to_owned())),
            Err(ReadlineError::Interrupted) => Ok(Some(String::new())),
            Err(ReadlineError::Eof) => Ok(None),
            Err(error) => Err(readline_error(error)),
        }
    }
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

fn readline_error(error: ReadlineError) -> io::Error {
    io::Error::other(error.to_string())
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
}
