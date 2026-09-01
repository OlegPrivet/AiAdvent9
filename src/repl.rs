use std::io::{self, Write};

use crate::api::NeuralDeepClient;
use crate::input::LineInput;
use crate::settings::Settings;

const CLEAR_SCREEN: &str = "\x1b[2J\x1b[H";
const MAIN_PROMPT: &str = "> ";

pub(crate) async fn run<I: LineInput, W: Write>(
    client: &NeuralDeepClient,
    settings: &mut Settings,
    initial_question: Option<String>,
    input: &mut I,
    output: &mut W,
) -> io::Result<()> {
    writeln!(output, "agi — интерактивный клиент NeuralDeep")?;
    writeln!(output, "Введите вопрос или /help для списка команд.\n")?;

    if let Some(question) = initial_question {
        ask(client, settings, &question, output).await?;
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
            question => ask(client, settings, question, output).await?,
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
) -> io::Result<()> {
    match client.ask(question, settings).await {
        Ok(answer) => {
            writeln!(output, "\n{}", answer.content)?;
            if answer.truncated {
                writeln!(
                    output,
                    "\n[Ответ обрезан лимитом max_tokens. Увеличьте пункт 2 в /settings или задайте более узкий вопрос.]"
                )?;
            }
            writeln!(output)
        }
        Err(error) => writeln!(output, "\nОшибка запроса: {error}\n"),
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
    writeln!(output, "  /exit, /выход           завершить работу")
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::input::BufferedInput;

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

        run(&client, &mut settings, None, &mut input, &mut output)
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

        run(&client, &mut settings, None, &mut input, &mut output)
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

        run(&client, &mut settings, None, &mut input, &mut output)
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

        run(&client, &mut settings, None, &mut input, &mut output)
            .await
            .expect("invalid UTF-8 should not terminate the REPL");

        assert!(
            String::from_utf8(output)
                .expect("output should be UTF-8")
                .contains("Неизвестная команда")
        );
    }
}
