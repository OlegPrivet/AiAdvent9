use std::io::{self, IsTerminal, Stdout};
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use ratatui_textarea::{CursorMove, TextArea};
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::agent::{Agent, AgentAnswer, AgentError, AgentEvent, AgentRequest};
use crate::agents_ui::{AgentManager, AgentPage};
use crate::chat::{Chat, ChatStore, ChatSummary, MessageRole};
use crate::cli::EditMode;
use crate::input::CommandHistory;
use crate::metrics::{ResponseMetrics, format_duration, metric_lines};
use crate::pricing::PriceCatalog;
use crate::repl::ParsedCommand;
use crate::settings::Settings;
use crate::ui::sanitize_terminal_text;

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(25);
const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(50);
const ACTIVE_CLOCK_INTERVAL: Duration = Duration::from_millis(250);
const STREAM_BATCH_INTERVAL: Duration = Duration::from_millis(40);
const STREAM_BATCH_BYTES: usize = 1024;
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
    CommandOption::run("/summarize", "заменить историю резюме", &["/суммаризация"]),
    CommandOption::run("/clear", "начать новый чат без контекста", &["/очистить"]),
    CommandOption::run("/help", "показать справку", &["/помощь"]),
    CommandOption::run("/exit", "сохранить чат и выйти", &["/quit", "/выход"]),
    CommandOption::run("/agents", "глобальный каталог агентов", &["/агенты"]),
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
    client: &Agent,
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
        request_task = Some(spawn_request(
            client,
            store,
            app.chat,
            question,
            app.request_id,
            worker_tx.clone(),
        ));
    }

    let mut needs_draw = true;
    let mut last_draw = Instant::now() - MIN_FRAME_INTERVAL;
    let exit_message = loop {
        if let Some(Modal::Agents(agents)) = &mut app.modal
            && agents.poll_generation()
        {
            needs_draw = true;
        }

        while let Ok(worker_event) = worker_rx.try_recv() {
            if matches!(&worker_event, WorkerEvent::Finished(id, _) if *id == app.request_id && app.pending_question.is_some())
            {
                request_task.take();
            }
            app.handle_worker_event(worker_event);
            needs_draw = true;
        }

        if app.request_started_at.is_some() && last_draw.elapsed() >= ACTIVE_CLOCK_INTERVAL {
            needs_draw = true;
        }
        if needs_draw && last_draw.elapsed() >= MIN_FRAME_INTERVAL {
            session.terminal.draw(|frame| app.render(frame))?;
            needs_draw = false;
            last_draw = Instant::now();
        }

        if app.should_refresh_prices() {
            spawn_price_refresh(worker_tx.clone());
            app.price_refresh_started();
        }

        let base_poll_interval = if app.request_started_at.is_some() {
            EVENT_POLL_INTERVAL
        } else {
            Duration::from_millis(200)
        };
        let poll_interval = if needs_draw {
            base_poll_interval.min(MIN_FRAME_INTERVAL.saturating_sub(last_draw.elapsed()))
        } else {
            base_poll_interval
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
                Action::GenerateSystemPrompt => {
                    if let Some(Modal::Agents(agents)) = &mut app.modal {
                        agents.generate_system_prompt(client, app.chat.settings().model());
                    }
                }
                Action::None => {}
                Action::Submit(question) => {
                    request_task = Some(spawn_request(
                        client,
                        store,
                        app.chat,
                        question,
                        app.request_id,
                        worker_tx.clone(),
                    ));
                }
                Action::CancelRequest => {
                    if let Some(task) = request_task.take() {
                        task.0.abort();
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
    client: &Agent,
    store: &ChatStore,
    chat: &Chat,
    question: String,
    request_id: Uuid,
    worker_tx: UnboundedSender<WorkerEvent>,
) -> RequestTask {
    let client = client.clone();
    let manual = crate::summary::is_command(&question);
    let request = store.agents().list().map(|agents| {
        AgentRequest::new(chat, if manual { String::new() } else { question }, agents)
    });
    RequestTask(tokio::spawn(async move {
        let delta_tx = worker_tx.clone();
        let result = match request {
            Err(error) => Err(AgentError::from(error)),
            Ok(mut request) => {
                async {
                    let mut summary_metrics = None;
                    if let Some(target) = crate::summary::prepare(&request, manual)? {
                        let _ = worker_tx.send(WorkerEvent::SummaryStarted(request_id));
                        let summary = client.summarize(&request, target).await?;
                        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
                        worker_tx
                            .send(WorkerEvent::SummaryReady(
                                request_id,
                                summary.clone(),
                                ack_tx,
                            ))
                            .map_err(|_| AgentError::InvalidRequest("TUI закрыт".into()))?;
                        ack_rx
                            .await
                            .map_err(|_| {
                                AgentError::InvalidRequest("Суммаризация отменена".into())
                            })?
                            .map_err(AgentError::InvalidRequest)?;
                        summary_metrics = Some(summary.metrics.clone());
                        crate::summary::apply(&mut request, summary);
                    }
                    if manual {
                        return Ok(AgentAnswer {
                            content: String::new(),
                            truncated: false,
                            elapsed_ms: 0,
                            calls: Vec::new(),
                            already_counted_usage: None,
                        });
                    }
                    let mut pending_delta = String::new();
                    let mut last_delta_flush = Instant::now() - STREAM_BATCH_INTERVAL;
                    let answer = {
                        let mut forward_event = |event| match event {
                            AgentEvent::MainDelta(delta) => {
                                pending_delta.push_str(&delta);
                                if pending_delta.len() >= STREAM_BATCH_BYTES
                                    || last_delta_flush.elapsed() >= STREAM_BATCH_INTERVAL
                                {
                                    send_agent_event(
                                        &delta_tx,
                                        request_id,
                                        AgentEvent::MainDelta(std::mem::take(&mut pending_delta)),
                                    )?;
                                    last_delta_flush = Instant::now();
                                }
                                Ok(())
                            }
                            event => {
                                if !pending_delta.is_empty() {
                                    send_agent_event(
                                        &delta_tx,
                                        request_id,
                                        AgentEvent::MainDelta(std::mem::take(&mut pending_delta)),
                                    )?;
                                    last_delta_flush = Instant::now();
                                }
                                send_agent_event(&delta_tx, request_id, event)
                            }
                        };
                        client.respond_streaming(request, &mut forward_event).await
                    };
                    if !pending_delta.is_empty() {
                        send_agent_event(
                            &delta_tx,
                            request_id,
                            AgentEvent::MainDelta(pending_delta),
                        )?;
                    }
                    let mut answer = answer?;
                    if let Some(metrics) = summary_metrics {
                        answer.already_counted_usage = metrics.usage;
                        let mut calls = metrics.calls;
                        calls.append(&mut answer.calls);
                        answer.calls = calls;
                        answer.elapsed_ms = answer.elapsed_ms.saturating_add(metrics.elapsed_ms);
                    }
                    Ok(answer)
                }
                .await
            }
        };
        let _ = worker_tx.send(WorkerEvent::Finished(request_id, result));
    }))
}

fn send_agent_event(
    worker_tx: &UnboundedSender<WorkerEvent>,
    request_id: Uuid,
    event: AgentEvent,
) -> io::Result<()> {
    worker_tx
        .send(WorkerEvent::Agent(request_id, event))
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "TUI закрыт"))
}

struct RequestTask(JoinHandle<()>);

impl Drop for RequestTask {
    fn drop(&mut self) {
        self.0.abort();
    }
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
    SummaryStarted(Uuid),
    SummaryReady(
        Uuid,
        crate::summary::ConversationSummary,
        tokio::sync::oneshot::Sender<Result<(), String>>,
    ),
    Agent(Uuid, AgentEvent),
    Finished(Uuid, Result<AgentAnswer, AgentError>),
    Prices(Result<PriceCatalog, String>),
}

enum Action {
    GenerateSystemPrompt,
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
    request_id: Uuid,
    agent_events: Vec<AgentEvent>,
    trace_question: Option<String>,
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
            request_id: Uuid::nil(),
            agent_events: Vec::new(),
            trace_question: None,
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
                Constraint::Length(5),
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
                    "Время ответа: {}",
                    format_duration(elapsed_millis(started_at.elapsed()))
                ),
                "Контекстное окно: …".to_owned(),
                "Выход последнего вызова: …".to_owned(),
                "API за весь диалог: … · вход … · выход …".to_owned(),
                "Стоимость: …".to_owned(),
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
        if let Some(summary) = self.chat.summary() {
            markdown.push_str("**Резюме диалога**\n\n");
            markdown.push_str(&sanitize_terminal_text(&summary.content));
            markdown.push_str(&format!("\n\n{}\n\n---\n\n", summary.report()));
        }
        for (index, message) in self
            .chat
            .messages()
            .iter()
            .enumerate()
            .skip(self.visible_from)
        {
            if self.pending_question.is_none()
                && self.trace_question.is_none()
                && index + 1 == self.chat.messages().len()
            {
                markdown.push_str(&self.agent_trace_markdown());
            }
            match message.role {
                MessageRole::User => markdown.push_str("**Вы**\n\n"),
                MessageRole::Assistant => markdown.push_str("**AI**\n\n"),
            }
            markdown.push_str(&sanitize_terminal_text(&message.content));
            markdown.push_str("\n\n---\n\n");
        }
        if self.pending_question.is_none()
            && let Some(question) = &self.trace_question
        {
            markdown.push_str(&format!(
                "**Вы**\n\n{}\n\n{}",
                sanitize_terminal_text(question),
                self.agent_trace_markdown()
            ));
        }
        if let Some(question) = &self.pending_question {
            markdown.push_str("**Вы**\n\n");
            markdown.push_str(&sanitize_terminal_text(question));
            markdown.push_str("\n\n---\n\n");
            markdown.push_str(&self.agent_trace_markdown());
            markdown.push_str("**Главный агент**\n\n");
            if self.streamed_answer.is_empty() {
                markdown.push_str("_Думаю…_");
            } else {
                markdown.push_str(&sanitize_terminal_text(&self.streamed_answer));
            }
        }
        if markdown.is_empty() {
            markdown.push_str(&self.agent_trace_markdown());
            markdown.push_str("_Новый чат. Он сохранится после первого ответа AI._");
        }
        markdown
    }

    fn agent_trace_markdown(&self) -> String {
        if self.agent_events.is_empty() {
            return String::new();
        }
        let state = if self.pending_question.is_some() {
            "выполняется…"
        } else if self.transient_metrics.is_some() {
            "запрос прерван"
        } else {
            "завершён"
        };
        let mut trace = format!("**Главный агент: {state}**\n\n");
        for event in &self.agent_events {
            match event {
                AgentEvent::ChildStarted {
                    id,
                    name,
                    handle,
                    model,
                    task,
                } => {
                    trace.push_str(&format!(
                        "**{} (@{}) · {}**\n\nЗадача: {}\n\n",
                        sanitize_terminal_text(name),
                        handle,
                        sanitize_terminal_text(model),
                        sanitize_terminal_text(task)
                    ));
                    let result = self.agent_events.iter().rev().find(|event|matches!(event, AgentEvent::ChildCompleted{id:other,..} | AgentEvent::ChildFailed{id:other,..} if other==id));
                    match result {
                        Some(AgentEvent::ChildCompleted {
                            content, truncated, ..
                        }) => trace.push_str(&format!(
                            "Статус: завершён{}\n\n{}\n\n",
                            if *truncated {
                                " (ответ обрезан)"
                            } else {
                                ""
                            },
                            sanitize_terminal_text(content)
                        )),
                        Some(AgentEvent::ChildFailed { error, .. }) => trace.push_str(&format!(
                            "Статус: ошибка — {}\n\n",
                            sanitize_terminal_text(error)
                        )),
                        _ => trace.push_str(if self.pending_question.is_some() {
                            "Статус: выполняется…\n\n"
                        } else {
                            "Статус: прерван\n\n"
                        }),
                    }
                }
                AgentEvent::ChildFailed { id, .. }
                    if !self.agent_events.iter().any(
                        |event| matches!(event,AgentEvent::ChildStarted{id:other,..} if other==id),
                    ) =>
                {
                    trace.push_str(&format!("{}\n\n", sanitize_terminal_text(&event.display())))
                }
                _ => {}
            }
        }
        trace.push_str("---\n\n");
        trace
    }

    fn handle_key(&mut self, key: KeyEvent) -> Action {
        if self.modal.is_some() {
            if key.code == KeyCode::F(2)
                && matches!(&self.modal, Some(Modal::Agents(agents)) if agents.manager.is_system_prompt())
            {
                return Action::GenerateSystemPrompt;
            }
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
        } else if let Some(Modal::Agents(agents)) = &mut self.modal
            && matches!(agents.manager.page(), AgentPage::Text { .. })
            && agents.generation.is_none()
        {
            agents.input.insert_str(text);
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
        if crate::summary::is_command(&value) {
            return self.begin_question("/summarize".into()).map(Some);
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
                        if crate::summary::is_command(command) {
                            return Some(match self.begin_question("/summarize".into()) {
                                Ok(question) => Action::Submit(question),
                                Err(error) => {
                                    self.notice = Some(error.to_string());
                                    Action::None
                                }
                            });
                        }
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
        self.request_id = Uuid::new_v4();
        self.agent_events.clear();
        self.trace_question = Some(value.clone());
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
        if self.pending_question.is_some() {
            self.notice = Some("Дождитесь завершения запроса или отмените его через Ctrl+C".into());
            return;
        }
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
        } else if command.matches(&["/agents", "/агенты"]) {
            match AgentManager::new(&self.store.agents()) {
                Ok(manager) => {
                    self.modal = Some(Modal::Agents(Box::new(AgentsModal::new(manager))))
                }
                Err(error) => self.notice = Some(error.to_string()),
            }
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
        if matches!(&event, WorkerEvent::Agent(id, _) | WorkerEvent::Finished(id, _) | WorkerEvent::SummaryStarted(id) | WorkerEvent::SummaryReady(id, _, _) if *id != self.request_id || self.pending_question.is_none())
        {
            return;
        }
        match event {
            WorkerEvent::SummaryStarted(_) => {
                self.notice = Some("Сжимаю историю…".into());
            }
            WorkerEvent::SummaryReady(_, mut summary, ack) => {
                summary.metrics.refresh_cost(&self.prices);
                let result = self
                    .store
                    .replace_with_summary(self.chat, summary.clone())
                    .map_err(|error| error.to_string());
                if result.is_ok() {
                    self.visible_from = 0;
                    self.history_scroll = 0;
                    self.follow_tail = true;
                    self.notice = Some(summary.report());
                }
                let _ = ack.send(result);
            }
            WorkerEvent::Agent(_, event) => {
                match event {
                    AgentEvent::MainDelta(delta) => self.streamed_answer.push_str(&delta),
                    AgentEvent::MainStarted => {
                        self.streamed_answer.clear();
                        self.agent_events.push(AgentEvent::MainStarted);
                    }
                    event => self.agent_events.push(event),
                }
                self.follow_tail = true;
            }
            WorkerEvent::Finished(_, result) => {
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
                    Ok(_) if crate::summary::is_command(&question) => {
                        self.trace_question = None;
                        self.streamed_answer.clear();
                        self.transient_metrics = None;
                        self.notice = Some(self.chat.summary().map_or_else(
                            || "Нет новых сообщений для суммаризации.".into(),
                            |summary| summary.report(),
                        ));
                    }
                    Ok(answer) => {
                        self.trace_question = None;
                        let truncated = answer.truncated;
                        let mut metrics = ResponseMetrics::from_calls(
                            &model,
                            answer.elapsed_ms,
                            answer.calls,
                            &self.prices,
                        );
                        metrics.already_counted_usage = answer.already_counted_usage;
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
            Modal::Agents(agents) => {
                if agents.handle_key(key, &self.store.agents()) {
                    return;
                }
            }
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
                let submit = submits_text_field(key, multiline);
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
        self.agent_events.clear();
        self.trace_question = None;
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
                self.agent_events.clear();
                self.trace_question = None;
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

struct AgentsModal {
    manager: AgentManager,
    input: TextArea<'static>,
    selected: usize,
    generation: Option<RequestTask>,
    generated: Option<oneshot::Receiver<Result<String, AgentError>>>,
}

impl AgentsModal {
    fn new(manager: AgentManager) -> Self {
        let mut modal = Self {
            manager,
            input: new_textarea(vec![], "Агенты"),
            selected: 0,
            generation: None,
            generated: None,
        };
        modal.refresh();
        modal
    }

    fn refresh(&mut self) {
        self.selected = 0;
        if let AgentPage::Text { value, title, .. } = self.manager.page() {
            self.input = new_textarea(
                sanitize_terminal_text(&value)
                    .split('\n')
                    .map(str::to_owned)
                    .collect(),
                &title,
            );
        }
    }

    fn handle_key(&mut self, key: KeyEvent, store: &crate::agent_catalog::AgentStore<'_>) -> bool {
        if self.generation.is_some() {
            if key.code == KeyCode::Esc
                || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
            {
                self.generation = None;
                self.generated = None;
                self.manager.notice = Some("Генерация отменена. Введённый текст сохранён".into());
            }
            return false;
        }
        let result = if key.code == KeyCode::Esc {
            Some(self.manager.cancel(store))
        } else {
            match self.manager.page() {
                AgentPage::List { items, .. } => match key.code {
                    KeyCode::Up => {
                        self.selected = self.selected.saturating_sub(1);
                        None
                    }
                    KeyCode::Down => {
                        self.selected = (self.selected + 1).min(items.len().saturating_sub(1));
                        None
                    }
                    KeyCode::Enter => Some(self.manager.select(self.selected, store)),
                    _ => None,
                },
                AgentPage::Read { .. } => match key.code {
                    KeyCode::Up | KeyCode::PageUp => {
                        self.selected = self.selected.saturating_sub(5);
                        None
                    }
                    KeyCode::Down | KeyCode::PageDown => {
                        self.selected = self.selected.saturating_add(5).min(u16::MAX as usize);
                        None
                    }
                    KeyCode::Enter => Some(self.manager.select(0, store)),
                    _ => None,
                },
                AgentPage::Text { multiline, .. } => {
                    if submits_text_field(key, multiline) {
                        Some(
                            self.manager
                                .submit(self.input.lines().join("\n"), store)
                                .map(|()| false),
                        )
                    } else {
                        self.input.input(key);
                        None
                    }
                }
            }
        };
        match result {
            Some(Ok(true)) => true,
            Some(Ok(false)) => {
                self.refresh();
                false
            }
            Some(Err(error)) => {
                self.manager.notice = Some(error.to_string());
                false
            }
            None => false,
        }
    }

    fn generate_system_prompt(&mut self, agent: &Agent, model: &str) {
        if self.generation.is_some() || !self.manager.is_system_prompt() {
            return;
        }
        let request = self
            .manager
            .system_prompt_request(self.input.lines().join("\n"), model);
        let agent = agent.clone();
        let (tx, rx) = oneshot::channel();
        self.generation = Some(RequestTask(tokio::spawn(async move {
            let _ = tx.send(agent.generate_system_prompt(request).await);
        })));
        self.generated = Some(rx);
        self.manager.notice =
            Some("Запрашиваю system prompt у LLM… Esc или Ctrl+C: отменить".into());
    }

    fn poll_generation(&mut self) -> bool {
        let Some(receiver) = &mut self.generated else {
            return false;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(oneshot::error::TryRecvError::Empty) => return false,
            Err(oneshot::error::TryRecvError::Closed) => Err(AgentError::InvalidRequest(
                "Запрос system prompt прерван".into(),
            )),
        };
        self.generation = None;
        self.generated = None;
        match result {
            Ok(prompt) => {
                self.manager.preview_system_prompt(prompt);
                self.refresh();
            }
            Err(error) => self.manager.notice = Some(format!("System prompt не изменён: {error}")),
        }
        true
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let notice = self
            .manager
            .notice
            .as_deref()
            .map(sanitize_terminal_text)
            .unwrap_or_default();
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(if notice.is_empty() { 0 } else { 3 }),
                Constraint::Min(1),
            ])
            .split(area);
        if !notice.is_empty() {
            frame.render_widget(
                Paragraph::new(notice)
                    .style(Style::default().fg(Color::Yellow))
                    .wrap(Wrap { trim: false }),
                layout[0],
            );
        }
        let area = layout[1];
        match self.manager.page() {
            AgentPage::List { title, items } => {
                let list = List::new(
                    items
                        .into_iter()
                        .map(|item| ListItem::new(sanitize_terminal_text(&item)))
                        .collect::<Vec<_>>(),
                )
                .block(Block::default().borders(Borders::ALL).title(format!(
                    " {} · Enter: выбрать · Esc: назад ",
                    sanitize_terminal_text(&title)
                )))
                .highlight_symbol("› ")
                .highlight_style(Style::default().fg(Color::Cyan));
                let mut state = ListState::default().with_selected(Some(self.selected));
                frame.render_stateful_widget(list, area, &mut state);
            }
            AgentPage::Text {
                title, multiline, ..
            } => {
                self.input.set_block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!(
                            " {title} · {}: далее · Esc: отмена ",
                            if multiline { "Ctrl+Enter/F4" } else { "Enter" }
                        ))
                        .title_bottom(if self.manager.is_system_prompt() {
                            " F2: создать system prompt через LLM "
                        } else {
                            ""
                        }),
                );
                frame.render_widget(&self.input, area);
            }
            AgentPage::Read { title, content } => {
                let text = Text::from(sanitize_terminal_text(&content));
                let max_scroll = wrapped_text_height(&text, area.width.saturating_sub(2))
                    .saturating_sub(area.height.saturating_sub(2) as usize);
                self.selected = self.selected.min(max_scroll).min(u16::MAX as usize);
                frame.render_widget(
                    Paragraph::new(text)
                        .wrap(Wrap { trim: false })
                        .scroll((self.selected as u16, 0))
                        .block(Block::default().borders(Borders::ALL).title(format!(
                            " {} · Enter: действия · ↑/↓: прокрутка · Esc: назад ",
                            sanitize_terminal_text(&title)
                        ))),
                    area,
                );
            }
        }
    }
}

fn submits_text_field(key: KeyEvent, multiline: bool) -> bool {
    key.code == KeyCode::F(4)
        || (key.code == KeyCode::Enter
            && (!multiline || key.modifiers.contains(KeyModifiers::CONTROL)))
}

enum Modal {
    Agents(Box<AgentsModal>),
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
        Modal::Agents(agents) => agents.render(frame, area),
        Modal::Help => {
            let help = [
                "/chat, /чаты             выбрать сохранённый чат",
                "/restore <UUID>          восстановить чат",
                "/settings, /настройки   настройки текущего чата",
                "/agents, /агенты       глобальный каталог агентов · вызов @handle",
                "/summarize, /суммаризация заменить историю резюме",
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
    enhanced_keyboard: bool,
}

impl TerminalSession {
    fn new() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        ) {
            let _ = execute!(
                stdout,
                DisableBracketedPaste,
                DisableMouseCapture,
                LeaveAlternateScreen
            );
            let _ = disable_raw_mode();
            return Err(error);
        }
        let enhanced_keyboard = supports_keyboard_enhancement().unwrap_or(false);
        if enhanced_keyboard
            && let Err(error) = execute!(
                stdout,
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )
        {
            let _ = execute!(
                stdout,
                PopKeyboardEnhancementFlags,
                LeaveAlternateScreen,
                DisableMouseCapture,
                DisableBracketedPaste
            );
            let _ = disable_raw_mode();
            return Err(error);
        }
        match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => Ok(Self {
                terminal,
                enhanced_keyboard,
            }),
            Err(error) => {
                let mut stdout = io::stdout();
                if enhanced_keyboard {
                    let _ = execute!(stdout, PopKeyboardEnhancementFlags);
                }
                let _ = execute!(
                    stdout,
                    DisableBracketedPaste,
                    LeaveAlternateScreen,
                    DisableMouseCapture
                );
                let _ = disable_raw_mode();
                Err(error)
            }
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if self.enhanced_keyboard {
            let _ = execute!(self.terminal.backend_mut(), PopKeyboardEnhancementFlags);
        }
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            DisableBracketedPaste,
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
    fn long_multiline_paste_is_inserted_in_full_and_waits_for_enter() {
        for mode in [EditMode::Emacs, EditMode::Vim] {
            let directory = TestDirectory::new();
            let store = ChatStore::for_tests(directory.0.clone()).expect("store");
            let mut chat = Chat::new();
            let mut app = App::with_history(&store, &mut chat, mode, CommandHistory::default());
            let text = format!(
                "/exit\n{}Конец вставки\n",
                "Длинная строка с Unicode 🙂 и пробелами  \n".repeat(5000)
            );
            app.handle_paste(&text);
            assert_eq!(app.input_value(), text);
            assert!(app.pending_question.is_none());
            assert!(app.request_started_at.is_none());
            assert!(!app.exit_requested, "pasted commands must remain text");
            assert!(app.chat.messages().is_empty());
            match app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)) {
                Action::Submit(question) => assert_eq!(question, text),
                _ => panic!("only explicit Enter should submit the full paste"),
            }
            assert_eq!(app.pending_question.as_deref(), Some(text.as_str()));
        }
    }

    #[tokio::test]
    async fn summary_worker_waits_for_persistence_and_ignores_cancelled_results() {
        let server = crate::test_http::MockServer::new(|request| {
            crate::test_http::text_response(
                if request["messages"][0]["content"]
                    .as_str()
                    .is_some_and(|text| text.starts_with("Сожми"))
                {
                    "Резюме"
                } else {
                    "Ответ"
                },
            )
        });
        let client = Agent::new(
            crate::api::NeuralDeepClient::new("test-key".into(), server.url.clone())
                .expect("client"),
        );
        let directory = TestDirectory::new();
        let store = ChatStore::for_tests(directory.0.clone()).expect("store");
        let mut chat = Chat::new();
        *chat.settings_mut() = crate::settings::Settings::for_summary("qwen3.8-27b", 20000, 1000);
        let metrics = ResponseMetrics::from_calls(
            "qwen3.8-27b",
            0,
            vec![crate::metrics::CallUsage {
                context: None,
                model: "qwen3.8-27b".into(),
                usage: Some(TokenUsage {
                    total_tokens: 17_000,
                    ..TokenUsage::default()
                }),
            }],
            &PriceCatalog::default(),
        );
        chat.record_exchange_with_metrics("x".repeat(17000), "Старый ответ".into(), Some(metrics));
        store.save(&mut chat).expect("save");
        let mut app = App::with_history(
            &store,
            &mut chat,
            EditMode::Emacs,
            CommandHistory::default(),
        );
        let question = app.begin_question("Продолжи".into()).expect("question");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let _task = spawn_request(&client, &store, app.chat, question, app.request_id, tx);
        let ready = loop {
            let event = rx.recv().await.expect("event");
            if matches!(event, WorkerEvent::SummaryReady(..)) {
                break event;
            }
            app.handle_worker_event(event);
        };
        let summary_calls = server.requests().len();
        assert!(
            server
                .requests()
                .iter()
                .all(|call| call["messages"][0]["content"]
                    .as_str()
                    .is_some_and(|text| text.starts_with("Сожми"))),
            "main must wait for commit acknowledgement"
        );
        assert!(app.chat.summary().is_none());
        app.handle_worker_event(ready);
        assert!(
            store
                .load(app.chat.id())
                .expect("saved before main")
                .summary()
                .is_some()
        );
        loop {
            let event = rx.recv().await.expect("event");
            let finished = matches!(event, WorkerEvent::Finished(..));
            app.handle_worker_event(event);
            if finished {
                break;
            }
        }
        assert_eq!(server.requests().len(), summary_calls + 1);
        assert!(app.transcript_markdown().contains("Резюме диалога"));
        assert!(!app.transcript_markdown().contains("Старый ответ"));
        let summary = app.chat.summary().expect("summary").clone();
        app.begin_question("/summarize".into()).expect("manual");
        let id = app.request_id;
        app.cancel_request();
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        app.handle_worker_event(WorkerEvent::SummaryReady(id, summary, ack_tx));
        assert!(ack_rx.await.is_err());
        assert_eq!(
            app.chat.messages().len(),
            2,
            "stale result must not replace new messages"
        );
    }

    #[test]
    fn calculates_wrapped_transcript_height() {
        let text = Text::from(vec![Line::from("1234567890"), Line::from("")]);
        assert_eq!(wrapped_text_height(&text, 5), 3);
    }

    fn description_modal(store: &ChatStore) -> AgentsModal {
        let mut manager = AgentManager::new(&store.agents()).expect("manager");
        manager.select(0, &store.agents()).expect("create");
        manager
            .submit("Редактор".into(), &store.agents())
            .expect("name");
        manager
            .submit("editor".into(), &store.agents())
            .expect("handle");
        AgentsModal::new(manager)
    }

    fn system_prompt_modal(store: &ChatStore) -> AgentsModal {
        let mut modal = description_modal(store);
        modal.input.insert_str("Описание вручную");
        modal.handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &store.agents(),
        );
        modal
    }

    #[test]
    fn description_cannot_generate_with_api() {
        let directory = TestDirectory::new();
        let store = ChatStore::for_tests(directory.0.clone()).expect("store");
        let mut modal = description_modal(&store);
        let agent = Agent::new(
            crate::api::NeuralDeepClient::new("test-key".into(), "http://127.0.0.1:1".into())
                .expect("client"),
        );
        modal.input.insert_str("Описание вручную");
        modal.generate_system_prompt(&agent, "qwen3.8-27b");
        assert!(modal.generation.is_none());
        assert_eq!(modal.input.lines().join("\n"), "Описание вручную");
    }

    #[test]
    fn description_accepts_plain_enter_control_enter_and_f4() {
        let directory = TestDirectory::new();
        let store = ChatStore::for_tests(directory.0.clone()).expect("store");
        for key in [
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::F(4), KeyModifiers::NONE),
        ] {
            let mut modal = description_modal(&store);
            assert!(matches!(
                modal.manager.page(),
                AgentPage::Text {
                    multiline: false,
                    ..
                }
            ));
            modal.input.insert_str("Проверяет текст");
            assert!(!modal.handle_key(key, &store.agents()));
            assert!(
                matches!(modal.manager.page(),AgentPage::Text {title,multiline:true,..} if title=="System prompt")
            );
            assert!(store.agents().list().expect("agents").is_empty());
        }
    }

    #[test]
    fn multiline_fields_keep_enter_for_newline_and_accept_portable_f4() {
        let directory = TestDirectory::new();
        let store = ChatStore::for_tests(directory.0.clone()).expect("store");
        let mut modal = description_modal(&store);
        modal.input.insert_str("Описание");
        modal.handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &store.agents(),
        );
        modal.input.insert_str("Инструкция");
        modal.handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &store.agents(),
        );
        assert_eq!(modal.input.lines().len(), 2);
        assert!(matches!(
            modal.manager.page(),
            AgentPage::Text {
                multiline: true,
                ..
            }
        ));
        modal.handle_key(
            KeyEvent::new(KeyCode::F(4), KeyModifiers::NONE),
            &store.agents(),
        );
        assert!(
            matches!(modal.manager.page(),AgentPage::List {title,..} if title=="Модель агента")
        );
    }

    #[tokio::test]
    async fn system_prompt_generation_is_reviewable_and_preserves_text_on_error() {
        for status in [200, 503] {
            let server = crate::test_http::MockServer::new(move |_| {
                let body = if status == 200 {
                    serde_json::json!({"choices":[{"message":{"content":"Редактирует тексты и исправляет ошибки."},"finish_reason":"stop"}]})
                } else {
                    serde_json::json!({"detail":"Недоступно"})
                };
                (status, body.to_string())
            });
            let agent = Agent::new(
                crate::api::NeuralDeepClient::new("test-key".into(), server.url.clone())
                    .expect("client"),
            );
            let directory = TestDirectory::new();
            let store = ChatStore::for_tests(directory.0.clone()).expect("store");
            let mut modal = system_prompt_modal(&store);
            modal.input.insert_str("Создай инструкции редактора");
            modal.generate_system_prompt(&agent, "qwen3.8-27b");
            modal.handle_key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                &store.agents(),
            );
            assert!(modal.manager.is_system_prompt());
            tokio::time::timeout(Duration::from_secs(3), async {
                while !modal.poll_generation() {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            })
            .await
            .expect("generation complete");
            assert!(modal.manager.is_system_prompt());
            assert!(store.agents().list().expect("agents").is_empty());
            if status == 200 {
                assert_eq!(
                    modal.input.lines().join("\n"),
                    "Редактирует тексты и исправляет ошибки."
                );
                modal.handle_key(
                    KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
                    &store.agents(),
                );
                assert!(!modal.manager.is_system_prompt());
            } else {
                assert_eq!(
                    modal.input.lines().join("\n"),
                    "Создай инструкции редактора"
                );
                assert!(
                    modal
                        .manager
                        .notice
                        .as_deref()
                        .is_some_and(|text| text.contains("не изменён"))
                );
            }
        }
    }

    #[tokio::test]
    async fn cancelling_system_prompt_discards_late_result_and_keeps_editor() {
        let directory = TestDirectory::new();
        let store = ChatStore::for_tests(directory.0.clone()).expect("store");
        let mut modal = system_prompt_modal(&store);
        modal.input.insert_str("Мой запрос");
        let (tx, rx) = oneshot::channel();
        modal.generated = Some(rx);
        modal.generation = Some(RequestTask(tokio::spawn(std::future::pending())));
        assert!(!modal.handle_key(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &store.agents()
        ));
        assert!(modal.generation.is_none());
        assert!(tx.send(Ok("Поздний ответ".into())).is_err());
        assert!(!modal.poll_generation());
        assert_eq!(modal.input.lines().join("\n"), "Мой запрос");
        assert!(modal.manager.is_system_prompt());
    }

    #[test]
    fn agents_menu_renders_global_catalog_and_is_blocked_during_request() {
        let directory = TestDirectory::new();
        let store = ChatStore::for_tests(directory.0.clone()).expect("store");
        let mut definition = crate::agent_catalog::AgentDefinition::draft();
        definition.name = "Редактор".into();
        definition.handle = "editor".into();
        definition.description = "Проверка текста".into();
        definition
            .settings
            .set_system_prompt("Редактируй".into())
            .expect("prompt");
        store.agents().save(&definition, true).expect("save");
        let mut chat = Chat::new();
        let mut app = App::with_history(
            &store,
            &mut chat,
            EditMode::Emacs,
            CommandHistory::default(),
        );
        app.handle_command(ParsedCommand::parse("/agents").expect("command"));
        assert!(matches!(app.modal, Some(Modal::Agents(_))));
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
        terminal.draw(|frame| app.render(frame)).expect("render");
        let screen = (0..30)
            .map(|row| buffer_row(terminal.backend().buffer(), row))
            .collect::<String>();
        assert!(screen.contains("Глобальные агенты"));
        assert!(screen.contains("@editor"));
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.begin_question("Вопрос".into()).expect("question");
        app.handle_command(ParsedCommand::parse("/agents").expect("command"));
        assert!(app.modal.is_none());
        assert!(
            app.notice
                .as_deref()
                .is_some_and(|notice| notice.contains("Ctrl+C"))
        );
    }

    #[test]
    fn child_trace_is_visible_but_not_restored_and_stale_events_are_ignored() {
        let directory = TestDirectory::new();
        let store = ChatStore::for_tests(directory.0.clone()).expect("store");
        let mut chat = Chat::new();
        let mut app = App::with_history(
            &store,
            &mut chat,
            EditMode::Emacs,
            CommandHistory::default(),
        );
        app.begin_question("@editor Проверь".into())
            .expect("question");
        let id = app.request_id;
        app.handle_worker_event(WorkerEvent::Agent(id, AgentEvent::MainStarted));
        app.handle_worker_event(WorkerEvent::Agent(
            id,
            AgentEvent::ChildStarted {
                id: "child1".into(),
                name: "Редактор".into(),
                handle: "editor".into(),
                model: "qwen3.8-27b".into(),
                task: "Проверка".into(),
            },
        ));
        assert!(app.transcript_markdown().contains("Статус: выполняется"));
        app.handle_worker_event(WorkerEvent::Agent(
            id,
            AgentEvent::ChildCompleted {
                id: "child1".into(),
                content: "Дочерний результат".into(),
                truncated: false,
            },
        ));
        app.handle_worker_event(WorkerEvent::Finished(
            id,
            Ok(AgentAnswer {
                content: "Итог главного".into(),
                truncated: false,
                elapsed_ms: 100,
                calls: vec![crate::metrics::CallUsage {
                    context: None,
                    model: "qwen3.8-27b".into(),
                    usage: Some(TokenUsage::default()),
                }],
                already_counted_usage: None,
            }),
        ));
        let trace = app.transcript_markdown();
        assert!(trace.contains("Дочерний результат"));
        assert!(!trace.contains("выполняется"));
        let chat_id = app.chat.id();
        let mut restored = store.load(chat_id).expect("restore");
        assert_eq!(restored.messages().len(), 2);
        assert_eq!(restored.messages()[1].content, "Итог главного");
        let restored_app = App::with_history(
            &store,
            &mut restored,
            EditMode::Emacs,
            CommandHistory::default(),
        );
        assert!(
            !restored_app
                .transcript_markdown()
                .contains("Дочерний результат")
        );
        app.begin_question("Второй".into()).expect("question");
        let cancelled = app.request_id;
        app.cancel_request();
        app.begin_question("Третий".into()).expect("question");
        app.handle_worker_event(WorkerEvent::Agent(
            cancelled,
            AgentEvent::MainDelta("Запоздавший ответ".into()),
        ));
        assert!(app.streamed_answer.is_empty());
        assert_eq!(app.pending_question.as_deref(), Some("Третий"));
        assert!(!app.transcript_markdown().contains("Дочерний результат"));
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
        assert!(buffer_row(buffer, 8).contains("Ввод"));
        assert!(buffer_row(buffer, 11).contains("Команды"));
        assert!(buffer_row(buffer, 12).contains("/chat"));
        assert!(buffer_row(buffer, 19).contains("Время ответа"));

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
            is_summary: false,
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
            calls: Vec::new(),
            cumulative_usage: None,
            already_counted_usage: None,
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
        assert!(buffer_row(buffer, 16).contains("Ввод"));
        assert!(buffer_row(buffer, 19).contains("Время ответа: 2,35 с"));
        assert!(
            buffer_row(buffer, 20).contains("Контекстное окно: 1 234 / 262 144 токенов (0,5%)")
        );
        assert!(buffer_row(buffer, 21).contains("Выход последнего вызова: 254 токенов"));
        assert!(buffer_row(buffer, 22).contains("API за весь диалог: 1 234 токенов"));
        assert!(buffer_row(buffer, 23).contains("Стоимость: ≈ 0,042137 ₽"));
    }

    fn buffer_row(buffer: &ratatui::buffer::Buffer, row: u16) -> String {
        (0..buffer.area.width)
            .map(|column| buffer[(column, row)].symbol())
            .collect()
    }
}
