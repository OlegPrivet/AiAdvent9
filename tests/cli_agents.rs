use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

struct StateDirectory(PathBuf);

impl Drop for StateDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(state: &StateDirectory, input: &str) -> String {
    let mut process = Command::new(env!("CARGO_BIN_EXE_agi"))
        .env("NEURALDEEP_API_KEY", "test-key")
        .env("XDG_STATE_HOME", &state.0)
        // The CLI refreshes public prices at startup. Keep the smoke test offline.
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .env("HTTP_PROXY", "http://127.0.0.1:1")
        .env("ALL_PROXY", "http://127.0.0.1:1")
        .env("NO_PROXY", "")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start CLI");
    process
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("commands");
    let output = process.wait_with_output().expect("CLI output");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("UTF-8")
}

#[test]
fn piped_cli_manages_global_agents_without_a_completed_chat() {
    let state =
        StateDirectory(std::env::temp_dir().join(format!("agi-cli-{}", uuid::Uuid::new_v4())));
    let output = run(
        &state,
        "/agents\n1\nРедактор\neditor\nПроверяет текст\nИсправляй ошибки\n12\n0.5\n1\n500\n1\n1\nesc\n/clear\n/agents\n2\n\n1\n3\nНовое описание\n10\nesc\n/exit\n",
    );
    assert!(output.contains("Агент создан"));
    assert!(output.contains("Агент сохранён"));
    assert!(output.contains("Вызов: @editor"));
    assert!(output.contains("Новое описание"));
    let db = rusqlite::Connection::open(state.0.join("agi/chats.sqlite3")).expect("db");
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM chats", [], |row| row.get::<_, i64>(0))
            .expect("chats"),
        0
    );
    assert_eq!(
        db.query_row(
            "SELECT description FROM agents WHERE handle='editor'",
            [],
            |row| row.get::<_, String>(0)
        )
        .expect("agent"),
        "Новое описание"
    );
    let cancelled = run(&state, "/agents\n2\n\n2\n1\nesc\n/exit\n");
    assert!(!cancelled.contains("Агент удалён окончательно"));
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM agents", [], |row| row
            .get::<_, i64>(0))
            .expect("agents"),
        1
    );
    let deleted = run(&state, "/agents\n2\n\n2\n2\nesc\n/exit\n");
    assert!(deleted.contains("Агент удалён окончательно"));
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM agents", [], |row| row
            .get::<_, i64>(0))
            .expect("agents"),
        0
    );
}
