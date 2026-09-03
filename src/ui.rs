use std::io::{self, IsTerminal, Write};
use std::time::Duration;

use indicatif::{ProgressBar, ProgressStyle};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::{LinesWithEndings, as_24_bit_terminal_escaped};
use termimad::MadSkin;

use crate::chat::{Chat, MessageRole};

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

    #[cfg(test)]
    fn rendered_for_test() -> Self {
        Self {
            interactive: false,
            renderer: Some(MarkdownRenderer::new()),
        }
    }

    pub(crate) fn begin_answer<'a, W: Write>(&'a self, output: &'a mut W) -> LiveAnswer<'a, W> {
        LiveAnswer {
            output,
            renderer: self.renderer.as_ref(),
            status: RequestStatus::new(self.interactive),
            source: String::new(),
            pending_markdown: String::new(),
            wrote_plain: false,
            wrote_rendered: false,
        }
    }

    pub(crate) fn print_chat<W: Write>(&self, output: &mut W, chat: &Chat) -> io::Result<()> {
        writeln!(output, "\nЧат: {} [{}]", chat.title(), chat.id())?;

        for message in chat.messages() {
            let label = match message.role {
                MessageRole::User => "Вы",
                MessageRole::Assistant => "AI",
            };
            writeln!(output, "\n{label}:")?;
            let safe_content = sanitize_terminal_text(&message.content);
            let rendered = if message.role == MessageRole::Assistant {
                self.renderer
                    .as_ref()
                    .map(|renderer| renderer.render(&safe_content))
                    .unwrap_or(safe_content)
            } else {
                safe_content
            };
            output.write_all(rendered.as_bytes())?;
            if !rendered.ends_with('\n') {
                writeln!(output)?;
            }
        }

        writeln!(output)?;
        output.flush()
    }
}

pub(crate) struct LiveAnswer<'a, W: Write> {
    output: &'a mut W,
    renderer: Option<&'a MarkdownRenderer>,
    status: RequestStatus,
    source: String,
    pending_markdown: String,
    wrote_plain: bool,
    wrote_rendered: bool,
}

impl<W: Write> LiveAnswer<'_, W> {
    pub(crate) fn push(&mut self, delta: &str) -> io::Result<()> {
        let delta = sanitize_terminal_text(delta);
        if delta.is_empty() {
            return Ok(());
        }

        self.source.push_str(&delta);

        if self.renderer.is_some() {
            self.pending_markdown.push_str(&delta);
            self.render_complete_markdown_blocks()?;
        } else {
            self.status.clear();
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
            if self.source.is_empty() {
                self.pending_markdown = answer;
            }
            self.render_pending_markdown()?;
            if self.wrote_rendered {
                writeln!(self.output)?;
            }
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

        if self.renderer.is_some() && (!self.pending_markdown.is_empty() || self.wrote_rendered) {
            self.render_pending_markdown()?;
            writeln!(self.output)?;
        } else if self.wrote_plain && !self.source.ends_with('\n') {
            writeln!(self.output)?;
        }

        self.output.flush()
    }

    fn render_complete_markdown_blocks(&mut self) -> io::Result<()> {
        let complete_len = complete_markdown_prefix_len(&self.pending_markdown);
        if complete_len == 0 {
            return Ok(());
        }

        let pending = self.pending_markdown.split_off(complete_len);
        let complete = std::mem::replace(&mut self.pending_markdown, pending);
        self.write_markdown(&complete)
    }

    fn render_pending_markdown(&mut self) -> io::Result<()> {
        let pending = std::mem::take(&mut self.pending_markdown);
        self.write_markdown(&pending)
    }

    fn write_markdown(&mut self, markdown: &str) -> io::Result<()> {
        let Some(renderer) = self.renderer else {
            return Ok(());
        };
        if markdown.is_empty() {
            return Ok(());
        }

        self.status.clear();
        let rendered = renderer.render(markdown);
        self.output.write_all(rendered.as_bytes())?;
        if !rendered.ends_with('\n') {
            writeln!(self.output)?;
        }
        self.output.flush()?;

        self.wrote_rendered = true;
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
        if is_markdown_language(language) {
            push_rendered_segment(rendered, &self.render(code));
            return;
        }

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

fn is_markdown_language(language: &str) -> bool {
    matches!(
        language.to_ascii_lowercase().as_str(),
        "md" | "markdown" | "mdown" | "mkd"
    )
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

fn complete_markdown_prefix_len(markdown: &str) -> usize {
    let mut fence: Option<CodeFence> = None;
    let mut offset = 0;
    let mut complete = 0;

    for line in markdown.split_inclusive('\n') {
        if !line.ends_with('\n') {
            break;
        }
        offset += line.len();

        if let Some(active_fence) = &fence {
            if active_fence.closes(line) {
                fence = None;
                complete = offset;
            }
        } else if let Some(opening_fence) = CodeFence::open(line) {
            fence = Some(opening_fence);
        } else if line.trim().is_empty() {
            complete = offset;
        }
    }

    complete
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
    fn rendered_output_writes_each_markdown_block_once_without_repainting() {
        let ui = TerminalUi::rendered_for_test();
        let mut output = Vec::new();
        let mut answer = ui.begin_answer(&mut output);

        answer
            .push("# Заголо")
            .expect("partial heading should buffer");
        answer
            .push("вок\n\nПервый **абзац**.\n\nПоследний ")
            .expect("complete blocks should render");
        answer
            .push("абзац.")
            .expect("partial final block should buffer");
        answer
            .finish("# Заголовок\n\nПервый **абзац**.\n\nПоследний абзац.")
            .expect("answer should finish");

        let output = String::from_utf8(output).expect("output should be UTF-8");
        assert_eq!(output.matches("Заголовок").count(), 1);
        assert_eq!(output.matches("Первый").count(), 1);
        assert_eq!(output.matches("Последний").count(), 1);
        assert!(!output.contains("# Заголовок"));
        assert!(!output.contains("**абзац**"));
        assert!(!output.contains("\x1b[J"));
        assert!(!output.contains("\x1b[F"));
    }

    #[test]
    fn streaming_waits_for_a_closing_code_fence() {
        let open_fence = "```rust\nfn main() {\n\n";
        assert_eq!(complete_markdown_prefix_len(open_fence), 0);

        let closed_fence = "```rust\nfn main() {}\n```\n";
        assert_eq!(
            complete_markdown_prefix_len(closed_fence),
            closed_fence.len()
        );
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
    fn renders_fenced_markdown_as_a_document() {
        let renderer = MarkdownRenderer::new();
        let output = renderer.render("```markdown\n# Заголовок\n\n**Текст**\n```\n");

        assert!(output.contains("Заголовок"));
        assert!(output.contains("Текст"));
        assert!(!output.contains("── markdown ──"));
        assert!(!output.contains("# Заголовок"));
        assert!(!output.contains("**Текст**"));
    }

    #[test]
    fn prints_restored_chat_transcript() {
        let ui = TerminalUi::plain();
        let mut chat = Chat::new();
        chat.record_exchange("Вопрос".to_owned(), "**Ответ**".to_owned());
        let mut output = Vec::new();

        ui.print_chat(&mut output, &chat)
            .expect("transcript should render");
        let output = String::from_utf8(output).expect("output should be UTF-8");

        assert!(output.contains("Чат: Вопрос"));
        assert!(output.contains("Вы:\nВопрос"));
        assert!(output.contains("AI:\n**Ответ**"));
        assert!(!output.contains('\u{1b}'));
    }
}
