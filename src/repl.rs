use std::io::{self, Write};

use uuid::Uuid;

use crate::api::NeuralDeepClient;
use crate::chat::{Chat, ChatStore};
use crate::input::LineInput;
use crate::ui::TerminalUi;

const CLEAR_SCREEN: &str = "\x1b[2J\x1b[H";
const MAIN_PROMPT: &str = "agi";

pub(crate) async fn run<I: LineInput, W: Write>(
    client: &NeuralDeepClient,
    store: &ChatStore,
    chat: &mut Chat,
    initial_question: Option<String>,
    input: &mut I,
    output: &mut W,
    ui: &TerminalUi,
) -> io::Result<()> {
    writeln!(output, "agi — интерактивный клиент NeuralDeep")?;
    writeln!(output, "Введите вопрос или /help для списка команд.")?;
    if chat.is_persisted() {
        ui.print_chat(output, chat)?;
    } else {
        writeln!(
            output,
            "Новый чат. Он сохранится после первого ответа AI.\n"
        )?;
    }

    if let Some(question) = initial_question {
        ask(client, store, chat, &question, output, ui).await?;
    }

    loop {
        let Some(line) = input.read_line(MAIN_PROMPT)? else {
            return finish_session(store, chat, output);
        };
        if line.trim().is_empty() || line.trim() == "/" {
            continue;
        }

        if let Some(command) = ParsedCommand::parse(&line) {
            if command.matches(&["/exit", "/quit", "/выход"]) {
                return finish_session(store, chat, output);
            }
            if command.matches(&["/clear", "/очистить"]) {
                clear_screen(output)?;
            } else if command.matches(&["/help", "/помощь"]) {
                print_help(output)?;
            } else if command.matches(&["/settings", "/setting", "/настройки"]) {
                configure_settings(store, chat, input, output)?;
            } else if command.matches(&["/chat", "/chats", "/чаты"]) {
                choose_chat(store, chat, input, output, ui)?;
            } else if command.matches(&["/restore", "/восстановить"]) {
                restore_from_argument(store, chat, command.argument, output, ui)?;
            } else {
                writeln!(
                    output,
                    "Неизвестная команда: {}. Используйте /help.",
                    command.name
                )?;
            }
        } else {
            ask(client, store, chat, &line, output, ui).await?;
        }
    }
}

fn clear_screen<W: Write>(output: &mut W) -> io::Result<()> {
    write!(output, "{CLEAR_SCREEN}")?;
    output.flush()
}

async fn ask<W: Write>(
    client: &NeuralDeepClient,
    store: &ChatStore,
    chat: &mut Chat,
    question: &str,
    output: &mut W,
    ui: &TerminalUi,
) -> io::Result<()> {
    let mut live_answer = ui.begin_answer(output);
    let render_deltas = !chat.settings().response_format_enabled();
    let result = client
        .ask_streaming(
            chat.messages(),
            chat.id(),
            question,
            chat.settings(),
            |delta| {
                if render_deltas {
                    live_answer.push(delta)
                } else {
                    Ok(())
                }
            },
        )
        .await;

    match result {
        Ok(answer) => {
            live_answer.finish(&answer.content)?;
            let truncated = answer.truncated;
            chat.record_exchange(question.to_owned(), answer.content);
            if let Err(error) = store.save(chat) {
                writeln!(
                    output,
                    "Предупреждение: чат не удалось сохранить: {error}\n"
                )?;
            }
            if truncated {
                writeln!(
                    output,
                    "[Ответ обрезан лимитом max_tokens. Увеличьте его в /settings или задайте более узкий вопрос.]\n"
                )?;
            }
            Ok(())
        }
        Err(error) => {
            live_answer.abort()?;
            writeln!(output, "Ошибка запроса: {error}\n")
        }
    }
}

fn configure_settings<I: LineInput, W: Write>(
    store: &ChatStore,
    chat: &mut Chat,
    input: &mut I,
    output: &mut W,
) -> io::Result<()> {
    if chat.settings_mut().configure(input, output)? {
        chat.mark_changed();
        if chat.has_completed_turn()
            && let Err(error) = store.save(chat)
        {
            writeln!(
                output,
                "Предупреждение: настройки не удалось сохранить: {error}"
            )?;
        }
    }
    Ok(())
}

fn choose_chat<I: LineInput, W: Write>(
    store: &ChatStore,
    chat: &mut Chat,
    input: &mut I,
    output: &mut W,
    ui: &TerminalUi,
) -> io::Result<()> {
    let list = match store.list() {
        Ok(list) => list,
        Err(error) => {
            writeln!(output, "Не удалось прочитать историю чатов: {error}")?;
            return Ok(());
        }
    };
    if list.skipped_entries > 0 {
        writeln!(
            output,
            "Предупреждение: пропущено повреждённых записей: {}.",
            list.skipped_entries
        )?;
    }
    if list.chats.is_empty() {
        writeln!(output, "История чатов пока пуста.")?;
        return Ok(());
    }

    let items = list
        .chats
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    output.flush()?;
    let Some(selection) = input.select("История чатов — Enter: открыть, Esc: назад", &items)?
    else {
        return Ok(());
    };
    if let Some(summary) = list.chats.get(selection) {
        switch_chat(store, chat, summary.id, output, ui)?;
    }
    Ok(())
}

fn restore_from_argument<W: Write>(
    store: &ChatStore,
    chat: &mut Chat,
    argument: Option<&str>,
    output: &mut W,
    ui: &TerminalUi,
) -> io::Result<()> {
    let Some(argument) = argument else {
        writeln!(output, "Использование: /restore <UUID>. Список: /chat")?;
        return Ok(());
    };
    let id = match Uuid::parse_str(argument) {
        Ok(id) => id,
        Err(_) => {
            writeln!(output, "Некорректный UUID чата: {argument}")?;
            return Ok(());
        }
    };
    switch_chat(store, chat, id, output, ui)
}

fn switch_chat<W: Write>(
    store: &ChatStore,
    chat: &mut Chat,
    id: Uuid,
    output: &mut W,
    ui: &TerminalUi,
) -> io::Result<()> {
    if id == chat.id() && chat.is_persisted() {
        return ui.print_chat(output, chat);
    }

    let restored = match store.load(id) {
        Ok(chat) => chat,
        Err(error) => {
            writeln!(output, "Не удалось восстановить чат: {error}")?;
            return Ok(());
        }
    };

    if chat.has_completed_turn() && chat.is_dirty() {
        if let Err(error) = store.save(chat) {
            writeln!(
                output,
                "Переключение отменено: текущий чат не удалось сохранить: {error}"
            )?;
            return Ok(());
        }
    } else if !chat.has_completed_turn() && chat.is_dirty() {
        writeln!(output, "Новый чат без завершённых ответов не был сохранён.")?;
    }

    *chat = restored;
    ui.print_chat(output, chat)
}

fn finish_session<W: Write>(store: &ChatStore, chat: &mut Chat, output: &mut W) -> io::Result<()> {
    if chat.has_completed_turn() && chat.is_dirty() {
        match store.save(chat) {
            Ok(_) => {}
            Err(error) if chat.is_persisted() => {
                writeln!(
                    output,
                    "Предупреждение: последние изменения не сохранены: {error}"
                )?;
                writeln!(
                    output,
                    "Для возврата к последней сохранённой версии используйте agi --restore {}",
                    chat.id()
                )?;
                return Ok(());
            }
            Err(error) => {
                writeln!(output, "Чат не сохранён: {error}")?;
                return Ok(());
            }
        }
    }

    if chat.is_persisted() {
        writeln!(
            output,
            "Для возврата используйте agi --restore {}",
            chat.id()
        )
    } else {
        writeln!(output, "Чат не сохранён: нет завершённых ответов")
    }
}

struct ParsedCommand<'a> {
    name: &'a str,
    argument: Option<&'a str>,
}

impl<'a> ParsedCommand<'a> {
    fn parse(value: &'a str) -> Option<Self> {
        if value.contains('\n') || value.contains('\r') {
            return None;
        }
        let value = value.trim();
        if !value.starts_with('/') {
            return None;
        }
        let mut parts = value.splitn(2, char::is_whitespace);
        let name = parts.next()?;
        let argument = parts
            .next()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        Some(Self { name, argument })
    }

    fn matches(&self, commands: &[&str]) -> bool {
        is_command(self.name, commands)
    }
}

fn is_command(value: &str, commands: &[&str]) -> bool {
    let value = value.to_lowercase();
    let converted = from_russian_keyboard_layout(&value);
    if commands.contains(&value.as_str()) || commands.contains(&converted.as_str()) {
        return true;
    }

    let suffix = value
        .rsplit_once('/')
        .map(|(_, suffix)| format!("/{suffix}"));
    suffix.is_some_and(|suffix| {
        commands.contains(&suffix.as_str())
            || commands.contains(&from_russian_keyboard_layout(&suffix).as_str())
    })
}

fn from_russian_keyboard_layout(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            'й' => 'q',
            'ц' => 'w',
            'у' => 'e',
            'к' => 'r',
            'е' => 't',
            'н' => 'y',
            'г' => 'u',
            'ш' => 'i',
            'щ' => 'o',
            'з' => 'p',
            'ф' => 'a',
            'ы' => 's',
            'в' => 'd',
            'а' => 'f',
            'п' => 'g',
            'р' => 'h',
            'о' => 'j',
            'л' => 'k',
            'д' => 'l',
            'я' => 'z',
            'ч' => 'x',
            'с' => 'c',
            'м' => 'v',
            'и' => 'b',
            'т' => 'n',
            'ь' => 'm',
            other => other,
        })
        .collect()
}

fn print_help<W: Write>(output: &mut W) -> io::Result<()> {
    writeln!(output, "Доступные команды:")?;
    writeln!(output, "  /chat, /чаты             выбрать сохранённый чат")?;
    writeln!(
        output,
        "  /restore <UUID>          восстановить чат по UUID"
    )?;
    writeln!(
        output,
        "  /settings, /настройки   изменить настройки текущего чата"
    )?;
    writeln!(output, "  /clear, /очистить        очистить окно терминала")?;
    writeln!(output, "  /help, /помощь           показать эту справку")?;
    writeln!(
        output,
        "  /exit, /выход            сохранить и завершить работу"
    )?;
    writeln!(output, "\nРедактор строки:")?;
    writeln!(output, "  ↑/↓                      история запросов")?;
    writeln!(output, "  Ctrl+R                   поиск по истории")?;
    writeln!(
        output,
        "  Многострочная вставка    отправка только после Enter"
    )?;
    writeln!(
        output,
        "  agi --edit-mode vim      запустить с Vim-клавишами"
    )
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::io::Cursor;
    use std::path::PathBuf;

    use super::*;
    use crate::input::BufferedInput;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            Self(env::temp_dir().join(format!("agi-repl-test-{}", Uuid::new_v4())))
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn test_client() -> NeuralDeepClient {
        NeuralDeepClient::new(
            "test-key".to_owned(),
            "http://127.0.0.1:1".to_owned(),
            "test-model".to_owned(),
        )
        .expect("client should be built")
    }

    fn test_store() -> (TestDirectory, ChatStore) {
        let directory = TestDirectory::new();
        let store = ChatStore::for_tests(directory.0.clone()).expect("store should open");
        (directory, store)
    }

    #[tokio::test]
    async fn handles_local_commands_and_menu_settings_without_api_requests() {
        let (_directory, store) = test_store();
        let mut chat = Chat::new();
        let mut input = BufferedInput::new(Cursor::new(
            "/help\n/settings\n2\n900\nesc\n/unknown\n/exit\n",
        ));
        let mut output = Vec::new();
        let ui = TerminalUi::plain();

        run(
            &test_client(),
            &store,
            &mut chat,
            None,
            &mut input,
            &mut output,
            &ui,
        )
        .await
        .expect("REPL should exit successfully");

        let output = String::from_utf8(output).expect("output should be UTF-8");
        assert!(output.contains("Доступные команды:"));
        assert!(output.contains("Неизвестная команда: /unknown"));
        assert!(output.contains("Чат не сохранён: нет завершённых ответов"));
        assert_eq!(chat.settings().max_tokens(), 900);
    }

    #[tokio::test]
    async fn clears_terminal_and_keeps_session_running() {
        let (_directory, store) = test_store();
        let mut chat = Chat::new();
        let mut input = BufferedInput::new(Cursor::new("/clear\n/help\n/exit\n"));
        let mut output = Vec::new();
        let ui = TerminalUi::plain();

        run(
            &test_client(),
            &store,
            &mut chat,
            None,
            &mut input,
            &mut output,
            &ui,
        )
        .await
        .expect("REPL should continue after clearing");

        let output = String::from_utf8(output).expect("output should be UTF-8");
        assert!(output.contains(CLEAR_SCREEN));
        assert!(output.contains("Доступные команды:"));
    }

    #[tokio::test]
    async fn accepts_commands_typed_in_russian_keyboard_layout() {
        let (_directory, store) = test_store();
        let mut chat = Chat::new();
        let mut input = BufferedInput::new(Cursor::new("/ыуе/settings\nesc\n/учше\n"));
        let mut output = Vec::new();
        let ui = TerminalUi::plain();

        run(
            &test_client(),
            &store,
            &mut chat,
            None,
            &mut input,
            &mut output,
            &ui,
        )
        .await
        .expect("Russian-layout commands should work");

        assert!(
            String::from_utf8(output)
                .expect("output should be UTF-8")
                .contains("Чат не сохранён")
        );
    }

    #[tokio::test]
    async fn selects_saved_chat_and_prints_restore_hint() {
        let (_directory, store) = test_store();
        let mut saved = Chat::new();
        saved.record_exchange("Сохранённый вопрос".to_owned(), "Ответ".to_owned());
        store.save(&mut saved).expect("fixture should save");
        let saved_id = saved.id();
        let mut active = Chat::new();
        let mut input = BufferedInput::new(Cursor::new("/chat\n1\n/exit\n"));
        let mut output = Vec::new();
        let ui = TerminalUi::plain();

        run(
            &test_client(),
            &store,
            &mut active,
            None,
            &mut input,
            &mut output,
            &ui,
        )
        .await
        .expect("chat should be selected");

        let output = String::from_utf8(output).expect("output should be UTF-8");
        assert_eq!(active.id(), saved_id);
        assert!(output.contains("Чат: Сохранённый вопрос"));
        assert!(output.contains(&format!("agi --restore {saved_id}")));
    }

    #[tokio::test]
    async fn restores_chat_by_command() {
        let (_directory, store) = test_store();
        let mut saved = Chat::new();
        saved.record_exchange("Вопрос".to_owned(), "Ответ".to_owned());
        store.save(&mut saved).expect("fixture should save");
        let saved_id = saved.id();
        let mut active = Chat::new();
        let commands = format!("/restore {saved_id}\n/exit\n");
        let mut input = BufferedInput::new(Cursor::new(commands));
        let mut output = Vec::new();
        let ui = TerminalUi::plain();

        run(
            &test_client(),
            &store,
            &mut active,
            None,
            &mut input,
            &mut output,
            &ui,
        )
        .await
        .expect("chat should restore");

        assert_eq!(active.id(), saved_id);
    }

    #[tokio::test]
    async fn survives_invalid_utf8_terminal_input() {
        let (_directory, store) = test_store();
        let mut chat = Chat::new();
        let mut input = BufferedInput::new(Cursor::new(vec![
            b'/', 0xff, b'\n', b'/', b'e', b'x', b'i', b't', b'\n',
        ]));
        let mut output = Vec::new();
        let ui = TerminalUi::plain();

        run(
            &test_client(),
            &store,
            &mut chat,
            None,
            &mut input,
            &mut output,
            &ui,
        )
        .await
        .expect("invalid UTF-8 should not terminate the REPL");

        assert!(
            String::from_utf8(output)
                .expect("output should be UTF-8")
                .contains("Неизвестная команда")
        );
    }

    #[test]
    fn treats_multiline_text_starting_with_slash_as_a_question() {
        assert!(ParsedCommand::parse("/exit\nобъясни эту строку").is_none());
        assert!(ParsedCommand::parse("  /exit  ").is_some());
    }
}
