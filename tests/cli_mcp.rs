use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

struct StateDirectory(PathBuf);

impl Drop for StateDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn chat_menu_connects_to_demo_mcp_and_lists_tools() {
    let state =
        StateDirectory(std::env::temp_dir().join(format!("agi-mcp-cli-{}", uuid::Uuid::new_v4())));
    let mut process = Command::new(env!("CARGO_BIN_EXE_agi"))
        .env("NEURALDEEP_API_KEY", "test-key")
        .env("XDG_STATE_HOME", &state.0)
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
        .write_all(b"/mcp\n3\n4\n1\n\n5\n5\n/exit\n")
        .expect("commands");
    let output = process.wait_with_output().expect("CLI output");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8");
    assert!(stdout.contains("Демосервер добавлен и включён"));
    assert!(stdout.contains("Соединение установлено, инструментов: 1"));
    assert!(stdout.contains("echo"));

    let database = rusqlite::Connection::open(state.0.join("agi/chats.sqlite3")).expect("db");
    let row = database
        .query_row(
            "SELECT name, enabled, transport_json FROM mcp_servers",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .expect("saved MCP server");
    assert_eq!(row.0, "demo");
    assert!(row.1);
    assert!(!row.2.contains("test-key"));
}

#[cfg(unix)]
#[test]
fn stdio_mcp_stderr_does_not_escape_into_cli_terminal() {
    use std::os::unix::fs::PermissionsExt;

    const MARKER: &str = "MCP_STDERR_SHOULD_NOT_ESCAPE";
    let state =
        StateDirectory(std::env::temp_dir().join(format!("agi-mcp-cli-{}", uuid::Uuid::new_v4())));
    let mut setup = Command::new(env!("CARGO_BIN_EXE_agi"))
        .env("NEURALDEEP_API_KEY", "test-key")
        .env("XDG_STATE_HOME", &state.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start setup CLI");
    setup
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"/mcp\n3\n5\n/exit\n")
        .expect("setup commands");
    let setup_output = setup.wait_with_output().expect("setup output");
    assert!(setup_output.status.success());

    let wrapper = state.0.join("noisy-mcp.sh");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nprintf '%s\\n' {MARKER} >&2\nexec \"$AGI_TEST_BINARY\" --mcp-demo-server\n"
        ),
    )
    .expect("write MCP wrapper");
    let mut permissions = std::fs::metadata(&wrapper)
        .expect("wrapper metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&wrapper, permissions).expect("wrapper permissions");

    let database = rusqlite::Connection::open(state.0.join("agi/chats.sqlite3")).expect("db");
    let transport = serde_json::json!({
        "type": "stdio",
        "command": wrapper,
        "args": [],
        "working_directory": state.0,
    });
    database
        .execute(
            "UPDATE mcp_servers SET name = 'noisy', transport_json = ?1",
            [transport.to_string()],
        )
        .expect("replace transport");
    drop(database);

    let mut process = Command::new(env!("CARGO_BIN_EXE_agi"))
        .env("NEURALDEEP_API_KEY", "test-key")
        .env("XDG_STATE_HOME", &state.0)
        .env("AGI_TEST_BINARY", env!("CARGO_BIN_EXE_agi"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start CLI");
    process
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"/mcp\n4\n1\n\n5\n5\n/exit\n")
        .expect("commands");
    let output = process.wait_with_output().expect("CLI output");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(!stderr.contains(MARKER), "child stderr escaped: {stderr}");
    let stdout = String::from_utf8(output.stdout).expect("UTF-8");
    assert!(stdout.contains("Соединение установлено, инструментов: 1"));
}
