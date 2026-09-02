use std::io::{self, Write};

use crate::api::NeuralDeepClient;
use crate::input::LineInput;
use crate::settings::Settings;
use crate::ui::TerminalUi;

const CLEAR_SCREEN: &str = "\x1b[2J\x1b[H";
const MAIN_PROMPT: &str = "agi";

pub(crate) async fn run<I: LineInput, W: Write>(
    client: &NeuralDeepClient,
    settings: &mut Settings,
    initial_question: Option<String>,
    input: &mut I,
    output: &mut W,
    ui: &TerminalUi,
) -> io::Result<()> {
    writeln!(output, "agi — интерактивный клиент NeuralDeep")?;
    writeln!(output, "Введите вопрос или /help для списка команд.\n")?;

    if let Some(question) = initial_question {
        ask(client, settings, &question, output, ui).await?;
    }

    loop {
        let Some(line) = input.read_line(MAIN_PROMPT)? else {
            writeln!(output)?;
            return Ok(());
        };

        match line.as_str() {
            "" | "/" => {}
            command if is_command(command, &["/exit", "/quit", "/выход"]) => return Ok(()),
            command if is_command(command, &["/clear", "/очистить"]) => {
                clear_screen(output)?
            }
            command if is_command(command, &["/help", "/помощь"]) => print_help(output)?,
            command if is_command(command, &["/settings", "/setting", "/настройки"]) => {
                settings.configure(input, output)?
            }
            value if value.starts_with('/') => {
                writeln!(output, "Неизвестная команда: {value}. Используйте /help.")?;
            }
            question => ask(client, settings, question, output, ui).await?,
        }
    }
}

fn clear_screen<W: Write>(output: &mut W) -> io::Result<()> {
    write!(output, "{CLEAR_SCREEN}")?;
    output.flush()
}

async fn ask<W: Write>(
    client: &NeuralDeepClient,
    settings: &Settings,
    question: &str,
    output: &mut W,
    ui: &TerminalUi,
) -> io::Result<()> {
    let mut live_answer = ui.begin_answer(output);
    let render_deltas = !settings.response_format_enabled();
    let result = client
        .ask_streaming(question, settings, |delta| {
            if render_deltas {
                live_answer.push(delta)
            } else {
                Ok(())
            }
        })
        .await;

    match result {
        Ok(answer) => {
            live_answer.finish(&answer.content)?;
            if answer.truncated {
                writeln!(
                    output,
                    "[Ответ обрезан лимитом max_tokens. Увеличьте пункт 2 в /settings или задайте более узкий вопрос.]\n"
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
    writeln!(
        output,
        "  /settings, /настройки  изменить настройки текущей сессии"
    )?;
    writeln!(output, "  /clear, /очистить        очистить окно терминала")?;
    writeln!(output, "  /help, /помощь          показать эту справку")?;
    writeln!(output, "  /exit, /выход           завершить работу")?;
    writeln!(output, "\nРедактор строки:")?;
    writeln!(output, "  ↑/↓                     история запросов")?;
    writeln!(output, "  Ctrl+R                  поиск по истории")?;
    writeln!(
        output,
        "  agi --edit-mode vim     запустить с Vim-клавишами"
    )
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::input::BufferedInput;
    use crate::ui::TerminalUi;

    #[tokio::test]
    async fn handles_local_commands_without_api_requests() {
        let client = NeuralDeepClient::new(
            "test-key".to_owned(),
            "http://127.0.0.1:1".to_owned(),
            "test-model".to_owned(),
        )
        .expect("client should be built");
        let mut settings = Settings::default();
        let mut input = BufferedInput::new(Cursor::new(
            "/help\n/settings\n2\n900\n0\n/unknown\n/exit\n",
        ));
        let mut output = Vec::new();
        let ui = TerminalUi::plain();

        run(&client, &mut settings, None, &mut input, &mut output, &ui)
            .await
            .expect("REPL should exit successfully");

        let output = String::from_utf8(output).expect("output should be UTF-8");
        assert!(output.contains("Доступные команды:"));
        assert!(output.contains("Неизвестная команда: /unknown"));
        assert_eq!(settings.max_tokens(), 900);
    }

    #[tokio::test]
    async fn clears_terminal_and_keeps_session_running() {
        let client = NeuralDeepClient::new(
            "test-key".to_owned(),
            "http://127.0.0.1:1".to_owned(),
            "test-model".to_owned(),
        )
        .expect("client should be built");
        let mut settings = Settings::default();
        let mut input = BufferedInput::new(Cursor::new("/clear\n/help\n/exit\n"));
        let mut output = Vec::new();
        let ui = TerminalUi::plain();

        run(&client, &mut settings, None, &mut input, &mut output, &ui)
            .await
            .expect("REPL should continue after clearing");

        let output = String::from_utf8(output).expect("output should be UTF-8");
        assert!(output.contains(CLEAR_SCREEN));
        assert!(output.contains("Доступные команды:"));
    }

    #[tokio::test]
    async fn accepts_commands_typed_in_russian_keyboard_layout() {
        let client = NeuralDeepClient::new(
            "test-key".to_owned(),
            "http://127.0.0.1:1".to_owned(),
            "test-model".to_owned(),
        )
        .expect("client should be built");
        let mut settings = Settings::default();
        let mut input = BufferedInput::new(Cursor::new("/ыуе/settings\n0\n/ыуеештп\n0\n/учше\n"));
        let mut output = Vec::new();
        let ui = TerminalUi::plain();

        run(&client, &mut settings, None, &mut input, &mut output, &ui)
            .await
            .expect("Russian-layout commands should work");

        assert!(
            String::from_utf8(output)
                .expect("output should be UTF-8")
                .contains("Настройки текущей сессии")
        );
    }

    #[tokio::test]
    async fn survives_invalid_utf8_terminal_input() {
        let client = NeuralDeepClient::new(
            "test-key".to_owned(),
            "http://127.0.0.1:1".to_owned(),
            "test-model".to_owned(),
        )
        .expect("client should be built");
        let mut settings = Settings::default();
        let mut input = BufferedInput::new(Cursor::new(vec![
            b'/', 0xff, b'\n', b'/', b'e', b'x', b'i', b't', b'\n',
        ]));
        let mut output = Vec::new();
        let ui = TerminalUi::plain();

        run(&client, &mut settings, None, &mut input, &mut output, &ui)
            .await
            .expect("invalid UTF-8 should not terminate the REPL");

        assert!(
            String::from_utf8(output)
                .expect("output should be UTF-8")
                .contains("Неизвестная команда")
        );
    }
}
