use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};

use indicatif::{ProgressBar, ProgressStyle};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::{LinesWithEndings, as_24_bit_terminal_escaped};
use termimad::MadSkin;
use unicode_width::UnicodeWidthChar;

const LIVE_REFRESH_INTERVAL: Duration = Duration::from_millis(40);
const RESET_STYLE: &str = "\x1b[0m";

pub(crate) struct TerminalUi {
    interactive: bool,
    renderer: Option<MarkdownRenderer>,
}

impl TerminalUi {
    pub(crate) fn detect() -> Self {
        let interactive = io::stdout().is_terminal() && io::stderr().is_terminal();
        let renderer = interactive.then(MarkdownRenderer::new);

        Self {
            interactive,
            renderer,
        }
    }

    #[cfg(test)]
    pub(crate) const fn plain() -> Self {
        Self {
            interactive: false,
            renderer: None,
        }
    }

    pub(crate) fn begin_answer<'a, W: Write>(&'a self, output: &'a mut W) -> LiveAnswer<'a, W> {
        LiveAnswer {
            output,
            renderer: self.renderer.as_ref(),
            status: RequestStatus::new(self.interactive),
            source: String::new(),
            rendered_rows: 0,
            wrote_plain: false,
            last_render: Instant::now(),
        }
    }
}

pub(crate) struct LiveAnswer<'a, W: Write> {
    output: &'a mut W,
    renderer: Option<&'a MarkdownRenderer>,
    status: RequestStatus,
    source: String,
    rendered_rows: usize,
    wrote_plain: bool,
    last_render: Instant,
}

impl<W: Write> LiveAnswer<'_, W> {
    pub(crate) fn push(&mut self, delta: &str) -> io::Result<()> {
        let delta = sanitize_terminal_text(delta);
        if delta.is_empty() {
            return Ok(());
        }

        self.status.clear();
        self.source.push_str(&delta);

        if self.renderer.is_some() {
            if self.rendered_rows == 0 || self.last_render.elapsed() >= LIVE_REFRESH_INTERVAL {
                self.render_current()?;
            }
        } else {
            self.output.write_all(delta.as_bytes())?;
            self.output.flush()?;
            self.wrote_plain = true;
        }

        Ok(())
    }

    pub(crate) fn finish(mut self, answer: &str) -> io::Result<()> {
        self.status.clear();
        let answer = sanitize_terminal_text(answer);

        if self.renderer.is_some() {
            self.source = answer;
            self.render_current()?;
            writeln!(self.output)?;
        } else if self.wrote_plain {
            if !self.source.ends_with('\n') {
                writeln!(self.output)?;
            }
            writeln!(self.output)?;
        } else {
            writeln!(self.output, "{answer}")?;
            writeln!(self.output)?;
        }

        self.output.flush()
    }

    pub(crate) fn abort(mut self) -> io::Result<()> {
        self.status.clear();

        if self.renderer.is_some() && !self.source.is_empty() {
            self.render_current()?;
            writeln!(self.output)?;
        } else if self.wrote_plain && !self.source.ends_with('\n') {
            writeln!(self.output)?;
        }

        self.output.flush()
    }

    fn render_current(&mut self) -> io::Result<()> {
        let Some(renderer) = self.renderer else {
            return Ok(());
        };
        let rendered = renderer.render(&self.source);

        if self.rendered_rows > 0 {
            write!(self.output, "\x1b[{}F\x1b[J", self.rendered_rows)?;
        }
        self.output.write_all(rendered.as_bytes())?;
        if !rendered.ends_with('\n') {
            writeln!(self.output)?;
        }
        self.output.flush()?;

        let width = usize::from(termimad::terminal_size().0.max(1));
        self.rendered_rows = rendered_rows(&rendered, width);
        self.last_render = Instant::now();
        Ok(())
    }
}

struct RequestStatus {
    spinner: Option<ProgressBar>,
}

impl RequestStatus {
    fn new(interactive: bool) -> Self {
        if !interactive {
            return Self { spinner: None };
        }

        let spinner = ProgressBar::new_spinner();
        let style = ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner())
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]);
        spinner.set_style(style);
        spinner.set_message("Думаю…");
        spinner.enable_steady_tick(Duration::from_millis(80));

        Self {
            spinner: Some(spinner),
        }
    }

    fn clear(&mut self) {
        if let Some(spinner) = self.spinner.take() {
            spinner.finish_and_clear();
        }
    }
}

impl Drop for RequestStatus {
    fn drop(&mut self) {
        self.clear();
    }
}

struct MarkdownRenderer {
    skin: MadSkin,
    syntaxes: SyntaxSet,
    theme: Theme,
}

impl MarkdownRenderer {
    fn new() -> Self {
        let themes = ThemeSet::load_defaults();
        let theme = themes
            .themes
            .get("base16-ocean.dark")
            .cloned()
            .unwrap_or_default();

        Self {
            skin: MadSkin::default_dark(),
            syntaxes: SyntaxSet::load_defaults_newlines(),
            theme,
        }
    }

    fn render(&self, markdown: &str) -> String {
        let mut rendered = String::new();
        let mut prose = String::new();
        let mut code = String::new();
        let mut fence: Option<CodeFence> = None;

        for line in markdown.split_inclusive('\n') {
            if let Some(active_fence) = &fence {
                if active_fence.closes(line) {
                    self.push_code(&mut rendered, &code, &active_fence.language);
                    code.clear();
                    fence = None;
                } else {
                    code.push_str(line);
                }
            } else if let Some(opening_fence) = CodeFence::open(line) {
                self.push_prose(&mut rendered, &prose);
                prose.clear();
                fence = Some(opening_fence);
            } else {
                prose.push_str(line);
            }
        }

        if let Some(active_fence) = fence {
            self.push_code(&mut rendered, &code, &active_fence.language);
        } else {
            self.push_prose(&mut rendered, &prose);
        }

        rendered
    }

    fn push_prose(&self, rendered: &mut String, prose: &str) {
        if prose.is_empty() {
            return;
        }
        push_rendered_segment(rendered, &self.skin.term_text(prose).to_string());
    }

    fn push_code(&self, rendered: &mut String, code: &str, language: &str) {
        let syntax = self
            .syntaxes
            .find_syntax_by_token(language)
            .unwrap_or_else(|| self.syntaxes.find_syntax_plain_text());
        let mut highlighter = HighlightLines::new(syntax, &self.theme);
        let mut highlighted = String::new();

        if !language.is_empty() {
            highlighted.push_str("\x1b[2m── ");
            highlighted.push_str(language);
            highlighted.push_str(" ──\x1b[0m\n");
        }

        for line in LinesWithEndings::from(code) {
            match highlighter.highlight_line(line, &self.syntaxes) {
                Ok(ranges) => {
                    highlighted.push_str(&as_24_bit_terminal_escaped(&ranges, false));
                    highlighted.push_str(RESET_STYLE);
                }
                Err(_) => highlighted.push_str(line),
            }
        }

        if code.is_empty() {
            highlighted.push('\n');
        }
        push_rendered_segment(rendered, &highlighted);
    }
}

#[derive(Debug)]
struct CodeFence {
    marker: char,
    length: usize,
    language: String,
}

impl CodeFence {
    fn open(line: &str) -> Option<Self> {
        let trimmed = line.trim_start();
        let marker = trimmed.chars().next()?;
        if !matches!(marker, '`' | '~') {
            return None;
        }

        let length = trimmed.chars().take_while(|value| *value == marker).count();
        if length < 3 {
            return None;
        }

        let language = trimmed[length..]
            .trim()
            .split(|value: char| value.is_whitespace() || matches!(value, ',' | '{'))
            .next()
            .unwrap_or_default()
            .to_owned();
        Some(Self {
            marker,
            length,
            language,
        })
    }

    fn closes(&self, line: &str) -> bool {
        let trimmed = line.trim();
        let marker_count = trimmed
            .chars()
            .take_while(|value| *value == self.marker)
            .count();
        marker_count >= self.length && trimmed.chars().all(|value| value == self.marker)
    }
}

fn push_rendered_segment(rendered: &mut String, segment: &str) {
    if segment.is_empty() {
        return;
    }
    if !rendered.is_empty() && !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    rendered.push_str(segment);
}

fn sanitize_terminal_text(text: &str) -> String {
    text.chars()
        .filter(|character| {
            matches!(character, '\n' | '\t') || (!character.is_control() && *character != '\u{7f}')
        })
        .collect()
}

fn rendered_rows(rendered: &str, terminal_width: usize) -> usize {
    rendered
        .split_terminator('\n')
        .map(|line| {
            let width = visible_width(line);
            width.max(1).div_ceil(terminal_width)
        })
        .sum::<usize>()
        .max(1)
}

fn visible_width(line: &str) -> usize {
    let mut width = 0;
    let mut escape = false;
    let mut control_sequence = false;

    for character in line.chars() {
        if escape {
            if !control_sequence && character == '[' {
                control_sequence = true;
            } else if !control_sequence || ('@'..='~').contains(&character) {
                escape = false;
                control_sequence = false;
            }
        } else if character == '\u{1b}' {
            escape = true;
        } else {
            width += UnicodeWidthChar::width(character).unwrap_or(0);
        }
    }

    width
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_output_streams_without_duplicating_answer() {
        let ui = TerminalUi::plain();
        let mut output = Vec::new();
        let mut answer = ui.begin_answer(&mut output);

        answer.push("Привет, ").expect("first chunk should render");
        answer.push("мир!").expect("second chunk should render");
        answer.finish("Привет, мир!").expect("answer should finish");

        assert_eq!(String::from_utf8(output).unwrap(), "Привет, мир!\n\n");
    }

    #[test]
    fn removes_terminal_control_characters_from_model_text() {
        assert_eq!(sanitize_terminal_text("до\x1b[2Jпосле\r\n"), "до[2Jпосле\n");
    }

    #[test]
    fn highlights_fenced_rust_code() {
        let renderer = MarkdownRenderer::new();
        let output = renderer.render("Текст\n```rust\nfn main() {}\n```\n");

        assert!(output.contains("Текст"));
        assert!(output.contains("── rust ──"));
        assert!(output.contains("\x1b[38;2;"));
        assert!(output.contains("fn"));
    }

    #[test]
    fn measures_text_without_ansi_sequences() {
        assert_eq!(visible_width("\x1b[38;2;1;2;3mлось\x1b[0m"), 4);
    }
}
