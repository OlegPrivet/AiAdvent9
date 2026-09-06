use std::io::{self, IsTerminal, Stdout};
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use ratatui_textarea::{CursorMove, TextArea};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::api::{ApiAnswer, ApiError, NeuralDeepClient};
use crate::chat::{Chat, ChatStore, ChatSummary, MessageRole};
use crate::cli::EditMode;
use crate::input::CommandHistory;
use crate::metrics::{ResponseMetrics, format_duration, metric_lines};
use crate::pricing::PriceCatalog;
use crate::repl::ParsedCommand;
use crate::settings::Settings;
use crate::ui::sanitize_terminal_text;

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(25);
const PRICE_RETRY_INTERVAL: Duration = Duration::from_secs(30);
const MAX_INPUT_HEIGHT: u16 = 8;
const COMMAND_PALETTE: &[CommandOption] = &[
    CommandOption::run("/chat", "выбрать сохранённый чат", &["/чаты"]),
    CommandOption::open_chats(
        "/restore <UUID>",
        "восстановить сохранённый чат",
        &["/восстановить"],
    ),
    CommandOption::run(
        "/settings",
        "настройки текущего чата",
        &["/setting", "/настройки"],
    ),
    CommandOption::run("/clear", "начать новый чат без контекста", &["/очистить"]),
    CommandOption::run("/help", "показать справку", &["/помощь"]),
    CommandOption::run("/exit", "сохранить чат и выйти", &["/quit", "/выход"]),
];

#[derive(Clone, Copy)]
struct CommandOption {
    syntax: &'static str,
    description: &'static str,
    aliases: &'static [&'static str],
    action: CommandAction,
}

impl CommandOption {
    const fn run(
        syntax: &'static str,
        description: &'static str,
        aliases: &'static [&'static str],
    ) -> Self {
        Self {
            syntax,
            description,
            aliases,
            action: CommandAction::Run(syntax),
        }
    }

    const fn open_chats(
        syntax: &'static str,
        description: &'static str,
        aliases: &'static [&'static str],
    ) -> Self {
        Self {
            syntax,
            description,
            aliases,
            action: CommandAction::OpenChats,
        }
    }

    fn matches(self, query: &str) -> bool {
        self.syntax.starts_with(query) || self.aliases.iter().any(|alias| alias.starts_with(query))
    }
}

#[derive(Clone, Copy)]
enum CommandAction {
    Run(&'static str),
    OpenChats,
}

pub(crate) fn is_supported() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

pub(crate) async fn run(
    client: &NeuralDeepClient,
    store: &ChatStore,
    chat: &mut Chat,
    initial_question: Option<String>,
    edit_mode: EditMode,
) -> io::Result<()> {
    let mut session = TerminalSession::new()?;
    let (worker_tx, mut worker_rx) = mpsc::unbounded_channel();
    let mut app = App::new(store, chat, edit_mode);
    let mut request_task = None;

    spawn_price_refresh(worker_tx.clone());
    app.price_refresh_started();

    if let Some(question) = initial_question.filter(|question| !question.trim().is_empty()) {
        let question = app.begin_question(question)?;
        request_task = Some(spawn_request(client, app.chat, question, worker_tx.clone()));
    }

    let mut needs_draw = true;
    let exit_message = loop {
        if needs_draw || app.request_started_at.is_some() {
            session.terminal.draw(|frame| app.render(frame))?;
            needs_draw = false;
        }

        while let Ok(worker_event) = worker_rx.try_recv() {
            if matches!(worker_event, WorkerEvent::Finished(_)) {
                request_task.take();
            }
            app.handle_worker_event(worker_event);
            needs_draw = true;
        }

        if app.should_refresh_prices() {
            spawn_price_refresh(worker_tx.clone());
            app.price_refresh_started();
        }

        let poll_interval = if app.request_started_at.is_some() {
            EVENT_POLL_INTERVAL
        } else {
            Duration::from_millis(200)
        };
        if event::poll(poll_interval)? {
            needs_draw = true;
            let action = match event::read()? {
                Event::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    app.handle_key(key)
                }
                Event::Mouse(mouse) => {
                    app.handle_mouse(mouse.kind);
                    Action::None
                }
                Event::Paste(text) => {
                    app.handle_paste(&text);
                    Action::None
                }
                Event::Resize(_, _) | Event::FocusGained | Event::FocusLost => Action::None,
                _ => Action::None,
            };
            match action {
                Action::None => {}
                Action::Submit(question) => {
                    request_task =
                        Some(spawn_request(client, app.chat, question, worker_tx.clone()));
                }
                Action::CancelRequest => {
                    if let Some(task) = request_task.take() {
                        task.abort();
                        app.cancel_request();
                    }
                }
                Action::Exit => {
                    if let Some(message) = app.finish_session() {
                        break message;
                    }
                }
            }
        }
    };

    drop(session);
    println!("{exit_message}");
    Ok(())
}

fn spawn_request(
    client: &NeuralDeepClient,
    chat: &Chat,
    question: String,
    worker_tx: UnboundedSender<WorkerEvent>,
) -> JoinHandle<()> {
    let client = client.clone();
    let history = chat.messages().to_vec();
    let chat_id = chat.id();
    let settings = chat.settings().clone();
    tokio::spawn(async move {
        let delta_tx = worker_tx.clone();
        let result = client
            .ask_streaming(&history, chat_id, &question, &settings, move |delta| {
                delta_tx
                    .send(WorkerEvent::Delta(delta.to_owned()))
                    .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "TUI закрыт"))
            })
            .await;
        let _ = worker_tx.send(WorkerEvent::Finished(result));
    })
}

fn spawn_price_refresh(worker_tx: UnboundedSender<WorkerEvent>) {
    tokio::spawn(async move {
        let result = PriceCatalog::fetch()
            .await
            .map_err(|error| error.to_string());
        let _ = worker_tx.send(WorkerEvent::Prices(result));
    });
}

enum WorkerEvent {
    Delta(String),
    Finished(Result<ApiAnswer, ApiError>),
    Prices(Result<PriceCatalog, String>),
}

enum Action {
    None,
    Submit(String),
    CancelRequest,
    Exit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VimMode {
    Insert,
    Normal,
}

struct App<'a> {
    store: &'a ChatStore,
    chat: &'a mut Chat,
    input: TextArea<'static>,
    edit_mode: EditMode,
    vim_mode: VimMode,
    command_history: CommandHistory,
    history_scroll: usize,
    max_history_scroll: usize,
    follow_tail: bool,
    visible_from: usize,
    pending_question: Option<String>,
    pending_model: Option<String>,
    streamed_answer: String,
    request_started_at: Option<Instant>,
    transient_metrics: Option<ResponseMetrics>,
    prices: PriceCatalog,
    price_refresh_in_flight: bool,
    last_price_attempt: Option<Instant>,
    notice: Option<String>,
    modal: Option<Modal>,
    exit_requested: bool,
    command_selection: usize,
}

impl<'a> App<'a> {
    fn new(store: &'a ChatStore, chat: &'a mut Chat, edit_mode: EditMode) -> Self {
        Self::with_history(store, chat, edit_mode, CommandHistory::load())
    }

    fn with_history(
        store: &'a ChatStore,
        chat: &'a mut Chat,
        edit_mode: EditMode,
        command_history: CommandHistory,
    ) -> Self {
        Self {
            store,
            chat,
            input: new_textarea(
                Vec::new(),
                "Ввод · Enter: отправить · Shift/Alt+Enter: новая строка",
            ),
            edit_mode,
            vim_mode: VimMode::Insert,
            command_history,
            history_scroll: 0,
            max_history_scroll: 0,
            follow_tail: true,
            visible_from: 0,
            pending_question: None,
            pending_model: None,
            streamed_answer: String::new(),
            request_started_at: None,
            transient_metrics: None,
            prices: PriceCatalog::default(),
            price_refresh_in_flight: false,
            last_price_attempt: None,
            notice: Some("Введите вопрос или /help для списка команд".to_owned()),
            modal: None,
            exit_requested: false,
            command_selection: 0,
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        let input_height = (self.input.lines().len() as u16 + 2).clamp(3, MAX_INPUT_HEIGHT);
        let command_options = self.command_palette_options();
        let palette_height = if command_options.is_empty() {
            0
        } else {
            u16::try_from(command_options.len())
                .unwrap_or(u16::MAX)
                .saturating_add(2)
                .min(8)
        };
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(3),
                Constraint::Length(input_height),
                Constraint::Length(palette_height),
                Constraint::Length(3),
            ])
            .split(area);

        self.render_header(frame, layout[0]);
        self.render_history(frame, layout[1]);
        self.render_input(frame, layout[2]);
        if !command_options.is_empty() {
            self.render_command_palette(frame, layout[3], &command_options);
        }
        self.render_metrics(frame, layout[4]);
        if let Some(modal) = &mut self.modal {
            render_modal(frame, modal);
        }
    }

    fn render_header(&self, frame: &mut Frame<'_>, area: Rect) {
        let title = format!(
            " agi · {} · {} ",
            sanitize_terminal_text(self.chat.title()),
            self.chat.settings().model()
        );
        let notice = self
            .notice
            .as_deref()
            .unwrap_or("PgUp/PgDn или колесо: история · Ctrl+End: к последнему ответу");
        let text = Text::from(vec![
            Line::styled(
                title,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::styled(
                sanitize_terminal_text(notice),
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        frame.render_widget(Paragraph::new(text), area);
    }

    fn render_history(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let markdown = self.transcript_markdown();
        let text = tui_markdown::from_str(&markdown);
        let inner_width = area.width.saturating_sub(2).max(1);
        let inner_height = area.height.saturating_sub(2) as usize;
        let total_height = wrapped_text_height(&text, inner_width);
        self.max_history_scroll = total_height
            .saturating_sub(inner_height)
            .min(usize::from(u16::MAX));
        if self.follow_tail {
            self.history_scroll = self.max_history_scroll;
        } else {
            self.history_scroll = self.history_scroll.min(self.max_history_scroll);
        }
        let scroll = u16::try_from(self.history_scroll).unwrap_or(u16::MAX);
        let title = if self.follow_tail {
            " История · автопрокрутка "
        } else {
            " История · просмотр · Ctrl+End: вниз "
        };
        let paragraph = Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0));
        frame.render_widget(paragraph, area);
    }

    fn render_input(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let title = if self.pending_question.is_some() {
            " Думаю… · Ctrl+C: отменить "
        } else if self.edit_mode == EditMode::Vim {
            match self.vim_mode {
                VimMode::Insert => " Ввод · INSERT · Enter: отправить · Esc: NORMAL ",
                VimMode::Normal => " Ввод · NORMAL · i: INSERT · Enter: отправить ",
            }
        } else {
            " Ввод · Enter: отправить · Shift/Alt+Enter: новая строка "
        };
        let border = if self.pending_question.is_some() {
            Color::DarkGray
        } else {
            Color::Cyan
        };
        self.input.set_block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(border)),
        );
        frame.render_widget(&self.input, area);
    }

    fn render_metrics(&self, frame: &mut Frame<'_>, area: Rect) {
        let lines = if let Some(started_at) = self.request_started_at {
            [
                format!(
                    "👉 Время ответа: {}",
                    format_duration(elapsed_millis(started_at.elapsed()))
                ),
                "👉 Токены: …".to_owned(),
                "👉 Стоимость: …".to_owned(),
            ]
        } else {
            metric_lines(
                self.transient_metrics
                    .as_ref()
                    .or_else(|| self.chat.last_response_metrics()),
            )
        };
        let text = Text::from(lines.map(Line::from).to_vec());
        frame.render_widget(Paragraph::new(text), area);
    }

    fn render_command_palette(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        options: &[CommandOption],
    ) {
        self.command_selection = self.command_selection.min(options.len().saturating_sub(1));
        let items = options
            .iter()
            .map(|option| ListItem::new(format!("{:<18} {}", option.syntax, option.description)))
            .collect::<Vec<_>>();
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Команды · ↑/↓: выбрать · Enter: выполнить · Esc: закрыть "),
            )
            .highlight_symbol("› ")
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            );
        let mut state = ListState::default().with_selected(Some(self.command_selection));
        frame.render_stateful_widget(list, area, &mut state);
    }

    fn transcript_markdown(&self) -> String {
        let mut markdown = String::new();
        for message in self.chat.messages().iter().skip(self.visible_from) {
            match message.role {
                MessageRole::User => markdown.push_str("**Вы**\n\n"),
                MessageRole::Assistant => markdown.push_str("**AI**\n\n"),
            }
            markdown.push_str(&sanitize_terminal_text(&message.content));
            markdown.push_str("\n\n---\n\n");
        }
        if let Some(question) = &self.pending_question {
            markdown.push_str("**Вы**\n\n");
            markdown.push_str(&sanitize_terminal_text(question));
            markdown.push_str("\n\n---\n\n**AI**\n\n");
            if self.streamed_answer.is_empty() {
                markdown.push_str("_Думаю…_");
            } else {
                markdown.push_str(&sanitize_terminal_text(&self.streamed_answer));
            }
        }
        if markdown.is_empty() {
            markdown.push_str("_Новый чат. Он сохранится после первого ответа AI._");
        }
        markdown
    }

    fn handle_key(&mut self, key: KeyEvent) -> Action {
        if self.modal.is_some() {
            self.handle_modal_key(key);
            return Action::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Home => {
                    self.follow_tail = false;
                    self.history_scroll = 0;
                    return Action::None;
                }
                KeyCode::End => {
                    self.follow_tail = true;
                    self.history_scroll = self.max_history_scroll;
                    return Action::None;
                }
                KeyCode::Char('c') if self.pending_question.is_some() => {
                    return Action::CancelRequest;
                }
                KeyCode::Char('c') => {
                    self.set_input("");
                    return Action::None;
                }
                KeyCode::Char('d')
                    if self.pending_question.is_none() && self.input_value().is_empty() =>
                {
                    return Action::Exit;
                }
                KeyCode::Char('r')
                    if self.pending_question.is_none()
                        && (self.edit_mode != EditMode::Vim
                            || self.vim_mode != VimMode::Normal) =>
                {
                    let query = self.input_value();
                    if let Some(found) = self.command_history.search_backwards(&query) {
                        self.set_input(&found);
                        self.notice = Some(format!("Найдено в истории: {}", one_line(&found)));
                    } else {
                        self.notice = Some("В истории ничего не найдено".to_owned());
                    }
                    return Action::None;
                }
                _ => {}
            }
        }
        if let Some(action) = self.handle_command_palette_key(key) {
            return action;
        }
        match key.code {
            KeyCode::PageUp => {
                self.scroll_up(10);
                Action::None
            }
            KeyCode::PageDown => {
                self.scroll_down(10);
                Action::None
            }
            _ if self.pending_question.is_some() => Action::None,
            _ if self.edit_mode == EditMode::Vim => self.handle_vim_key(key),
            _ => self.handle_emacs_key(key),
        }
    }

    fn handle_emacs_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                self.input.insert_newline();
                Action::None
            }
            KeyCode::Enter => match self.take_question() {
                Ok(Some(question)) => Action::Submit(question),
                Ok(None) if std::mem::take(&mut self.exit_requested) => Action::Exit,
                Ok(None) => Action::None,
                Err(error) => {
                    self.notice = Some(error.to_string());
                    Action::None
                }
            },
            KeyCode::Up if self.input.lines().len() == 1 => {
                let current = self.input_value();
                if let Some(previous) = self.command_history.previous(&current) {
                    self.set_input(&previous);
                }
                Action::None
            }
            KeyCode::Down if self.input.lines().len() == 1 => {
                if let Some(next) = self.command_history.next() {
                    self.set_input(&next);
                }
                Action::None
            }
            _ => {
                self.command_history.reset_navigation();
                self.command_selection = 0;
                self.input.input(key);
                Action::None
            }
        }
    }

    fn handle_vim_key(&mut self, key: KeyEvent) -> Action {
        if self.vim_mode == VimMode::Insert {
            if key.code == KeyCode::Esc {
                self.vim_mode = VimMode::Normal;
                return Action::None;
            }
            return self.handle_emacs_key(key);
        }

        match key.code {
            KeyCode::Enter => {
                return match self.take_question() {
                    Ok(Some(question)) => Action::Submit(question),
                    Ok(None) if std::mem::take(&mut self.exit_requested) => Action::Exit,
                    Ok(None) => Action::None,
                    Err(error) => {
                        self.notice = Some(error.to_string());
                        Action::None
                    }
                };
            }
            KeyCode::Char('i') => self.vim_mode = VimMode::Insert,
            KeyCode::Char('a') => {
                self.input.move_cursor(CursorMove::Forward);
                self.vim_mode = VimMode::Insert;
            }
            KeyCode::Char('I') => {
                self.input.move_cursor(CursorMove::Head);
                self.vim_mode = VimMode::Insert;
            }
            KeyCode::Char('A') => {
                self.input.move_cursor(CursorMove::End);
                self.vim_mode = VimMode::Insert;
            }
            KeyCode::Char('h') | KeyCode::Left => self.input.move_cursor(CursorMove::Back),
            KeyCode::Char('j') | KeyCode::Down => self.input.move_cursor(CursorMove::Down),
            KeyCode::Char('k') | KeyCode::Up => self.input.move_cursor(CursorMove::Up),
            KeyCode::Char('l') | KeyCode::Right => self.input.move_cursor(CursorMove::Forward),
            KeyCode::Char('w') => self.input.move_cursor(CursorMove::WordForward),
            KeyCode::Char('b') => self.input.move_cursor(CursorMove::WordBack),
            KeyCode::Char('0') => self.input.move_cursor(CursorMove::Head),
            KeyCode::Char('$') => self.input.move_cursor(CursorMove::End),
            KeyCode::Char('x') => {
                self.input.delete_next_char();
            }
            KeyCode::Char('u') => {
                self.input.undo();
            }
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.redo();
            }
            _ => {}
        }
        Action::None
    }

    fn handle_mouse(&mut self, kind: MouseEventKind) {
        match kind {
            MouseEventKind::ScrollUp => self.scroll_up(3),
            MouseEventKind::ScrollDown => self.scroll_down(3),
            _ => {}
        }
    }

    fn handle_paste(&mut self, text: &str) {
        if self.modal.is_none() && self.pending_question.is_none() {
            self.command_selection = 0;
            self.input.insert_str(text);
        } else if let Some(Modal::Text { input, .. }) = &mut self.modal {
            input.insert_str(text);
        }
    }

    fn scroll_up(&mut self, lines: usize) {
        self.follow_tail = false;
        self.history_scroll = self.history_scroll.saturating_sub(lines);
    }

    fn scroll_down(&mut self, lines: usize) {
        self.history_scroll = (self.history_scroll + lines).min(self.max_history_scroll);
        if self.history_scroll == self.max_history_scroll {
            self.follow_tail = true;
        }
    }

    fn take_question(&mut self) -> io::Result<Option<String>> {
        let value = self.input_value();
        if value.trim().is_empty() || value.trim() == "/" {
            self.set_input("");
            return Ok(None);
        }
        if let Some(command) = ParsedCommand::parse(&value) {
            self.set_input("");
            self.handle_command(command);
            return Ok(None);
        }
        self.begin_question(value).map(Some)
    }

    fn command_palette_options(&self) -> Vec<CommandOption> {
        if self.pending_question.is_some() || self.modal.is_some() {
            return Vec::new();
        }
        let value = self.input_value();
        let query = value.trim();
        if !query.starts_with('/') || query.chars().any(char::is_whitespace) {
            return Vec::new();
        }
        COMMAND_PALETTE
            .iter()
            .copied()
            .filter(|option| option.matches(query))
            .collect()
    }

    fn handle_command_palette_key(&mut self, key: KeyEvent) -> Option<Action> {
        let options = self.command_palette_options();
        if options.is_empty() {
            return None;
        }
        match key.code {
            KeyCode::Up | KeyCode::BackTab => {
                self.command_selection = self.command_selection.saturating_sub(1);
                Some(Action::None)
            }
            KeyCode::Down | KeyCode::Tab => {
                self.command_selection =
                    (self.command_selection + 1).min(options.len().saturating_sub(1));
                Some(Action::None)
            }
            KeyCode::Esc => {
                self.set_input("");
                Some(Action::None)
            }
            KeyCode::Enter => {
                let option = options[self.command_selection.min(options.len() - 1)];
                self.set_input("");
                match option.action {
                    CommandAction::Run(command) => {
                        if let Some(command) = ParsedCommand::parse(command) {
                            self.handle_command(command);
                        }
                    }
                    CommandAction::OpenChats => self.open_chats(),
                }
                Some(if std::mem::take(&mut self.exit_requested) {
                    Action::Exit
                } else {
                    Action::None
                })
            }
            _ => None,
        }
    }

    fn begin_question(&mut self, value: String) -> io::Result<String> {
        self.command_history.record(&value)?;
        self.set_input("");
        self.pending_model = Some(self.chat.settings().model().to_owned());
        self.pending_question = Some(value.clone());
        self.streamed_answer.clear();
        self.request_started_at = Some(Instant::now());
        self.transient_metrics = None;
        self.follow_tail = true;
        self.notice = None;
        Ok(value)
    }

    fn handle_command(&mut self, command: ParsedCommand<'_>) {
        if command.matches(&["/exit", "/quit", "/выход"]) {
            self.exit_requested = true;
        } else if command.matches(&["/clear", "/очистить"]) {
            self.start_new_chat();
        } else if command.matches(&["/help", "/помощь"]) {
            self.modal = Some(Modal::Help);
        } else if command.matches(&["/settings", "/setting", "/настройки"]) {
            self.open_settings();
        } else if command.matches(&["/chat", "/chats", "/чаты"]) {
            self.open_chats();
        } else if command.matches(&["/restore", "/восстановить"]) {
            match command
                .argument
                .and_then(|argument| Uuid::parse_str(argument).ok())
            {
                Some(id) => self.switch_chat(id),
                None => self.notice = Some("Использование: /restore <UUID>".to_owned()),
            }
        } else {
            self.notice = Some(format!(
                "Неизвестная команда: {}. Используйте /help.",
                command.name
            ));
        }
    }

    fn handle_worker_event(&mut self, event: WorkerEvent) {
        match event {
            WorkerEvent::Delta(delta) => {
                self.streamed_answer.push_str(&delta);
                self.follow_tail = true;
            }
            WorkerEvent::Finished(result) => {
                let question = self.pending_question.take().unwrap_or_default();
                let model = self
                    .pending_model
                    .take()
                    .unwrap_or_else(|| self.chat.settings().model().to_owned());
                let fallback_elapsed_ms = self
                    .request_started_at
                    .take()
                    .map(|started_at| elapsed_millis(started_at.elapsed()))
                    .unwrap_or_default();
                match result {
                    Ok(answer) => {
                        let truncated = answer.truncated;
                        let metrics = ResponseMetrics::new(
                            model,
                            answer.elapsed_ms,
                            answer.usage,
                            &self.prices,
                        );
                        self.chat.record_exchange_with_metrics(
                            question,
                            answer.content,
                            Some(metrics),
                        );
                        self.streamed_answer.clear();
                        self.transient_metrics = None;
                        if let Err(error) = self.store.save(self.chat) {
                            self.notice = Some(format!("Чат не удалось сохранить: {error}"));
                        } else if truncated {
                            self.notice =
                                Some("Ответ обрезан: увеличьте max_tokens в /settings".to_owned());
                        } else {
                            self.notice = None;
                        }
                    }
                    Err(error) => {
                        self.transient_metrics = Some(ResponseMetrics::new(
                            model,
                            fallback_elapsed_ms,
                            None,
                            &self.prices,
                        ));
                        self.streamed_answer.clear();
                        self.notice = Some(format!("Ошибка запроса: {error}"));
                    }
                }
                self.follow_tail = true;
            }
            WorkerEvent::Prices(result) => {
                self.price_refresh_in_flight = false;
                match result {
                    Ok(prices) => {
                        self.prices = prices;
                        if self.chat.refresh_last_response_cost(&self.prices)
                            && let Err(error) = self.store.save(self.chat)
                        {
                            self.notice = Some(format!("Метрики не удалось сохранить: {error}"));
                        }
                    }
                    Err(error) if self.prices.is_stale() => {
                        self.notice = Some(format!("Прайс временно недоступен: {error}"));
                    }
                    Err(_) => {}
                }
            }
        }
    }

    fn cancel_request(&mut self) {
        let elapsed_ms = self
            .request_started_at
            .take()
            .map(|started_at| elapsed_millis(started_at.elapsed()))
            .unwrap_or_default();
        let model = self
            .pending_model
            .take()
            .unwrap_or_else(|| self.chat.settings().model().to_owned());
        self.pending_question = None;
        self.streamed_answer.clear();
        self.transient_metrics = Some(ResponseMetrics::new(model, elapsed_ms, None, &self.prices));
        self.notice = Some("Запрос отменён".to_owned());
    }

    fn price_refresh_started(&mut self) {
        self.price_refresh_in_flight = true;
        self.last_price_attempt = Some(Instant::now());
    }

    fn should_refresh_prices(&self) -> bool {
        !self.price_refresh_in_flight
            && self.prices.is_stale()
            && self
                .last_price_attempt
                .is_none_or(|attempt| attempt.elapsed() >= PRICE_RETRY_INTERVAL)
    }

    fn set_input(&mut self, value: &str) {
        let lines = if value.is_empty() {
            Vec::new()
        } else {
            value.lines().map(str::to_owned).collect()
        };
        self.input = new_textarea(lines, "Ввод");
        self.input.move_cursor(CursorMove::Bottom);
        self.input.move_cursor(CursorMove::End);
        self.command_selection = 0;
    }

    fn input_value(&self) -> String {
        self.input.lines().join("\n")
    }

    fn open_settings(&mut self) {
        self.modal = Some(Modal::List {
            title: "Настройки текущего чата".to_owned(),
            items: self.chat.settings().menu_items(),
            selected: 0,
            kind: ListKind::Settings,
        });
    }

    fn open_chats(&mut self) {
        match self.store.list() {
            Ok(list) if list.chats.is_empty() => {
                self.notice = Some("История чатов пока пуста".to_owned());
            }
            Ok(list) => {
                let items = list.chats.iter().map(ToString::to_string).collect();
                self.modal = Some(Modal::List {
                    title: "История чатов".to_owned(),
                    items,
                    selected: 0,
                    kind: ListKind::Chats(list.chats),
                });
                if list.skipped_entries > 0 {
                    self.notice = Some(format!(
                        "Пропущено повреждённых записей: {}",
                        list.skipped_entries
                    ));
                }
            }
            Err(error) => self.notice = Some(format!("Историю не удалось прочитать: {error}")),
        }
    }

    fn handle_modal_key(&mut self, key: KeyEvent) {
        let Some(mut modal) = self.modal.take() else {
            return;
        };
        match &mut modal {
            Modal::Help => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
                    return;
                }
            }
            Modal::List {
                selected,
                items,
                kind,
                ..
            } => match key.code {
                KeyCode::Esc => return,
                KeyCode::Up | KeyCode::Char('k') => {
                    *selected = selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    *selected = (*selected + 1).min(items.len().saturating_sub(1));
                }
                KeyCode::Enter => {
                    let kind = kind.clone();
                    let selected = *selected;
                    self.activate_list_item(kind, selected);
                    return;
                }
                _ => {}
            },
            Modal::Text {
                kind, input, title, ..
            } => {
                if key.code == KeyCode::Esc {
                    return;
                }
                let multiline = kind.is_multiline();
                let submit = key.code == KeyCode::Enter
                    && (!multiline || key.modifiers.contains(KeyModifiers::CONTROL));
                if submit {
                    let value = input.lines().join("\n");
                    let kind = *kind;
                    if let Err(error) = self.apply_text_setting(kind, value) {
                        *title = error;
                        self.modal = Some(modal);
                    } else {
                        self.settings_changed();
                        self.open_settings();
                    }
                    return;
                }
                input.input(key);
            }
        }
        self.modal = Some(modal);
    }

    fn activate_list_item(&mut self, kind: ListKind, selected: usize) {
        match kind {
            ListKind::Settings => match selected {
                0 => {
                    self.modal = Some(Modal::List {
                        title: "Модель".to_owned(),
                        items: Settings::model_items(),
                        selected: self.chat.settings().model_index(),
                        kind: ListKind::Models,
                    });
                }
                1 => {
                    self.modal = Some(Modal::List {
                        title: "Structured Output".to_owned(),
                        items: vec!["Включить".to_owned(), "Выключить".to_owned()],
                        selected: usize::from(!self.chat.settings().response_format_enabled()),
                        kind: ListKind::Structured,
                    });
                }
                2 => self.open_text_modal(
                    "Максимальная длина · Enter: сохранить",
                    TextKind::MaxTokens,
                    self.chat.settings().max_tokens().to_string(),
                ),
                3 => self.open_text_modal(
                    "Температура 0.0–2.0 · Enter: сохранить",
                    TextKind::Temperature,
                    self.chat.settings().temperature().to_string(),
                ),
                4 => {
                    self.modal = Some(Modal::List {
                        title: "Условие завершения".to_owned(),
                        items: vec![
                            "Без условия".to_owned(),
                            "Stop sequence".to_owned(),
                            "Явная инструкция для модели".to_owned(),
                        ],
                        selected: 0,
                        kind: ListKind::Completion,
                    });
                }
                5 => {
                    self.modal = Some(Modal::List {
                        title: "Системный prompt".to_owned(),
                        items: vec![
                            "Задать или изменить".to_owned(),
                            "Удалить системный prompt".to_owned(),
                        ],
                        selected: 0,
                        kind: ListKind::SystemPrompt,
                    });
                }
                _ => self.open_settings(),
            },
            ListKind::Models => {
                if self.chat.settings_mut().select_model(selected) {
                    self.settings_changed();
                }
                self.open_settings();
            }
            ListKind::Structured => {
                self.notice = self.chat.settings_mut().set_response_format(selected == 0);
                self.settings_changed();
                self.open_settings();
            }
            ListKind::Completion => match selected {
                0 => {
                    self.chat.settings_mut().clear_completion_condition();
                    self.settings_changed();
                    self.open_settings();
                }
                1 => self.open_text_modal(
                    "Stop sequence · Enter: сохранить",
                    TextKind::StopSequence,
                    String::new(),
                ),
                2 => self.open_text_modal(
                    "Инструкция · Ctrl+Enter: сохранить",
                    TextKind::CompletionInstruction,
                    String::new(),
                ),
                _ => self.open_settings(),
            },
            ListKind::SystemPrompt => match selected {
                0 => self.open_text_modal(
                    "Системный prompt · Ctrl+Enter: сохранить",
                    TextKind::SystemPrompt,
                    self.chat
                        .settings()
                        .system_prompt()
                        .unwrap_or_default()
                        .to_owned(),
                ),
                1 => {
                    self.chat.settings_mut().clear_system_prompt();
                    self.settings_changed();
                    self.open_settings();
                }
                _ => self.open_settings(),
            },
            ListKind::Chats(chats) => {
                if let Some(summary) = chats.get(selected) {
                    self.switch_chat(summary.id);
                }
            }
        }
    }

    fn open_text_modal(&mut self, title: &str, kind: TextKind, initial: String) {
        self.modal = Some(Modal::Text {
            title: title.to_owned(),
            kind,
            input: Box::new(new_textarea(
                initial.lines().map(str::to_owned).collect(),
                title,
            )),
        });
    }

    fn apply_text_setting(&mut self, kind: TextKind, value: String) -> Result<(), String> {
        match kind {
            TextKind::MaxTokens => self
                .chat
                .settings_mut()
                .set_max_tokens(value.trim())
                .map(|warning| self.notice = warning),
            TextKind::Temperature => self.chat.settings_mut().set_temperature(value.trim()),
            TextKind::StopSequence => self.chat.settings_mut().set_stop_sequence(value),
            TextKind::CompletionInstruction => {
                self.chat.settings_mut().set_completion_instruction(value)
            }
            TextKind::SystemPrompt => self.chat.settings_mut().set_system_prompt(value),
        }
    }

    fn settings_changed(&mut self) {
        self.chat.mark_changed();
        if self.chat.has_completed_turn()
            && let Err(error) = self.store.save(self.chat)
        {
            self.notice = Some(format!("Настройки не удалось сохранить: {error}"));
        }
    }

    fn start_new_chat(&mut self) {
        if self.chat.has_completed_turn()
            && self.chat.is_dirty()
            && let Err(error) = self.store.save(self.chat)
        {
            self.notice = Some(format!(
                "Новый чат не создан: текущий чат не удалось сохранить: {error}"
            ));
            return;
        }

        *self.chat = Chat::new();
        self.visible_from = 0;
        self.history_scroll = 0;
        self.max_history_scroll = 0;
        self.follow_tail = true;
        self.transient_metrics = None;
        self.notice = Some("Начат новый чат без предыдущего контекста".to_owned());
    }

    fn switch_chat(&mut self, id: Uuid) {
        if id == self.chat.id() && self.chat.is_persisted() {
            self.modal = None;
            return;
        }
        if self.chat.has_completed_turn()
            && self.chat.is_dirty()
            && let Err(error) = self.store.save(self.chat)
        {
            self.notice = Some(format!("Переключение отменено: {error}"));
            self.modal = None;
            return;
        }
        match self.store.load(id) {
            Ok(restored) => {
                *self.chat = restored;
                self.visible_from = 0;
                self.transient_metrics = None;
                self.follow_tail = true;
                self.notice = None;
            }
            Err(error) => self.notice = Some(format!("Чат не удалось восстановить: {error}")),
        }
        self.modal = None;
    }

    fn finish_session(&mut self) -> Option<String> {
        if self.chat.has_completed_turn()
            && self.chat.is_dirty()
            && let Err(error) = self.store.save(self.chat)
        {
            self.notice = Some(format!("Последние изменения не сохранены: {error}"));
            return None;
        }
        Some(if self.chat.is_persisted() {
            format!("Для возврата используйте agi --restore {}", self.chat.id())
        } else {
            "Чат не сохранён: нет завершённых ответов".to_owned()
        })
    }
}

#[derive(Clone)]
enum ListKind {
    Settings,
    Models,
    Structured,
    Completion,
    SystemPrompt,
    Chats(Vec<ChatSummary>),
}

#[derive(Clone, Copy)]
enum TextKind {
    MaxTokens,
    Temperature,
    StopSequence,
    CompletionInstruction,
    SystemPrompt,
}

impl TextKind {
    fn is_multiline(self) -> bool {
        matches!(self, Self::CompletionInstruction | Self::SystemPrompt)
    }
}

enum Modal {
    Help,
    List {
        title: String,
        items: Vec<String>,
        selected: usize,
        kind: ListKind,
    },
    Text {
        title: String,
        kind: TextKind,
        input: Box<TextArea<'static>>,
    },
}

fn render_modal(frame: &mut Frame<'_>, modal: &mut Modal) {
    let area = centered_rect(80, 75, frame.area());
    frame.render_widget(Clear, area);
    match modal {
        Modal::Help => {
            let help = [
                "/chat, /чаты             выбрать сохранённый чат",
                "/restore <UUID>          восстановить чат",
                "/settings, /настройки   настройки текущего чата",
                "/clear, /очистить        начать новый чат без старого контекста",
                "/exit, /выход            завершить работу",
                "",
                "PgUp/PgDn, колесо        прокрутить историю",
                "Ctrl+Home/Ctrl+End       начало/конец истории",
                "Shift/Alt+Enter          новая строка",
                "/, затем ↑/↓ и Enter     выбрать и выполнить команду",
                "Ctrl+R                   найти запрос в истории",
                "Ctrl+D                   выход на пустой строке",
                "Ctrl+C                   отменить запрос или очистить ввод",
                "",
                "Esc, Enter или q         закрыть справку",
            ];
            let paragraph = Paragraph::new(help.join("\n"))
                .block(Block::default().borders(Borders::ALL).title(" Справка "))
                .wrap(Wrap { trim: false });
            frame.render_widget(paragraph, area);
        }
        Modal::List {
            title,
            items,
            selected,
            ..
        } => {
            let entries = items
                .iter()
                .map(|item| ListItem::new(sanitize_terminal_text(item)))
                .collect::<Vec<_>>();
            let list = List::new(entries)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!(" {title} · Enter: выбрать · Esc: назад ")),
                )
                .highlight_symbol("› ")
                .highlight_style(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                );
            let mut state = ListState::default().with_selected(Some(*selected));
            frame.render_stateful_widget(list, area, &mut state);
        }
        Modal::Text { title, input, .. } => {
            input.set_block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {title} · Esc: отмена ")),
            );
            frame.render_widget(&**input, area);
        }
    }
}

fn new_textarea(lines: Vec<String>, title: &str) -> TextArea<'static> {
    let mut input = TextArea::new(lines);
    input.set_cursor_line_style(Style::default());
    input.set_cursor_style(
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
    input.set_placeholder_text("Введите сообщение…");
    input.set_block(
        Block::default()
            .borders(Borders::ALL)
            .title(title.to_owned()),
    );
    input
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn wrapped_text_height(text: &Text<'_>, width: u16) -> usize {
    let width = usize::from(width.max(1));
    text.lines
        .iter()
        .map(|line| line.width().max(1).div_ceil(width))
        .sum()
}

fn elapsed_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn one_line(value: &str) -> String {
    value.replace(['\n', '\r'], " ")
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    fn new() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                let mut stdout = io::stdout();
                let _ = execute!(stdout, LeaveAlternateScreen, DisableMouseCapture);
                let _ = disable_raw_mode();
                Err(error)
            }
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        );
        let _ = self.terminal.show_cursor();
    }
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::path::PathBuf;

    use ratatui::backend::TestBackend;

    use super::*;
    use crate::metrics::TokenUsage;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            Self(env::temp_dir().join(format!("agi-tui-test-{}", Uuid::new_v4())))
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn calculates_wrapped_transcript_height() {
        let text = Text::from(vec![Line::from("1234567890"), Line::from("")]);
        assert_eq!(wrapped_text_height(&text, 5), 3);
    }

    #[test]
    fn text_targets_mark_multiline_values() {
        assert!(TextKind::SystemPrompt.is_multiline());
        assert!(TextKind::CompletionInstruction.is_multiline());
        assert!(!TextKind::Temperature.is_multiline());
    }

    #[test]
    fn clear_starts_a_new_chat_without_previous_context() {
        let directory = TestDirectory::new();
        let store = ChatStore::for_tests(directory.0.clone()).expect("test store should open");
        let mut chat = Chat::new();
        chat.record_exchange("Старый вопрос".to_owned(), "Старый ответ".to_owned());
        let old_id = chat.id();
        store.save(&mut chat).expect("old chat should save");
        let mut app = App::with_history(
            &store,
            &mut chat,
            EditMode::Emacs,
            CommandHistory::default(),
        );

        let command = ParsedCommand::parse("/clear").expect("clear command should parse");
        app.handle_command(command);

        assert_ne!(app.chat.id(), old_id);
        assert!(app.chat.messages().is_empty());
        assert_eq!(app.visible_from, 0);
        assert!(app.transient_metrics.is_none());
        assert_eq!(
            store
                .load(old_id)
                .expect("old chat should remain restorable")
                .messages()
                .len(),
            2
        );
    }

    #[test]
    fn slash_opens_command_palette_and_enter_executes_selection() {
        let directory = TestDirectory::new();
        let store = ChatStore::for_tests(directory.0.clone()).expect("test store should open");
        let mut chat = Chat::new();
        let mut app = App::with_history(
            &store,
            &mut chat,
            EditMode::Emacs,
            CommandHistory::default(),
        );
        app.set_input("/");
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal should open");

        terminal
            .draw(|frame| app.render(frame))
            .expect("command palette should render");

        let buffer = terminal.backend().buffer();
        assert!(buffer_row(buffer, 10).contains("Ввод"));
        assert!(buffer_row(buffer, 13).contains("Команды"));
        assert!(buffer_row(buffer, 14).contains("/chat"));
        assert!(buffer_row(buffer, 21).contains("Время ответа"));

        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Action::None
        ));
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Action::None
        ));
        assert!(matches!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::None
        ));
        assert!(app.input_value().is_empty());
        assert!(matches!(
            &app.modal,
            Some(Modal::List {
                kind: ListKind::Settings,
                ..
            })
        ));
    }

    #[test]
    fn keeps_input_and_metrics_in_fixed_bottom_rows() {
        let directory = TestDirectory::new();
        let store = ChatStore::for_tests(directory.0.clone()).expect("test store should open");
        let mut chat = Chat::new();
        let metrics = ResponseMetrics {
            model: "qwen3.8-27b-noreason".to_owned(),
            elapsed_ms: 2_345,
            usage: Some(TokenUsage {
                prompt_tokens: 980,
                completion_tokens: 254,
                total_tokens: 1_234,
                cached_prompt_tokens: 0,
            }),
            estimated_cost_microrubles: Some(42_137),
            premium: Some(false),
        };
        chat.record_exchange_with_metrics(
            "Вопрос".to_owned(),
            "**Длинный ответ**\n\nс Markdown".repeat(20),
            Some(metrics),
        );
        let mut app = App::with_history(
            &store,
            &mut chat,
            EditMode::Emacs,
            CommandHistory::default(),
        );
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal should open");

        terminal
            .draw(|frame| app.render(frame))
            .expect("TUI should render");

        let buffer = terminal.backend().buffer();
        assert!(buffer_row(buffer, 18).contains("Ввод"));
        assert!(buffer_row(buffer, 21).contains("Время ответа: 2,35 с"));
        assert!(buffer_row(buffer, 22).contains("Токены: 1 234"));
        assert!(buffer_row(buffer, 23).contains("Стоимость: ≈ 0,042137 ₽"));
    }

    fn buffer_row(buffer: &ratatui::buffer::Buffer, row: u16) -> String {
        (0..buffer.area.width)
            .map(|column| buffer[(column, row)].symbol())
            .collect()
    }
}
