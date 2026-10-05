use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct State(PathBuf);
impl Drop for State {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(state: &State, args: &[&str], input: &str) -> Output {
    let mut process = Command::new(env!("CARGO_BIN_EXE_agi"))
        .args(args)
        .env("XDG_STATE_HOME", &state.0)
        .env_remove("NEURALDEEP_API_KEY")
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .env("HTTP_PROXY", "http://127.0.0.1:1")
        .env("ALL_PROXY", "http://127.0.0.1:1")
        .env("NO_PROXY", "localhost,127.0.0.1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    process
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    process.wait_with_output().unwrap()
}

#[test]
fn repl_connects_http_models_through_the_interactive_menu_without_inference() {
    let state = State(std::env::temp_dir().join(format!("agi-llm-form-{}", uuid::Uuid::new_v4())));
    let result = run(
        &state,
        &[],
        "/llm\n1\nmine\nhttp://127.0.0.1:1/v1\nmy-model\n8192\n2\n1\n1\nesc\n/llm list\n/exit\n",
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let output = String::from_utf8_lossy(&result.stdout);
    assert!(output.contains("Добавить HTTP-подключение"));
    assert!(output.contains("Подключение mine сохранено и выбрано для всех чатов"));
    assert!(output.contains("* mine"));
    let db = rusqlite::Connection::open(state.0.join("agi/chats.sqlite3")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT value FROM metadata WHERE key='llm_default'",
            [],
            |row| row.get::<_, String>(0)
        )
        .unwrap(),
        "mine"
    );
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM chats", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        0
    );
    let profile: String = db
        .query_row(
            "SELECT profile_json FROM llm_profiles WHERE id='mine'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let profile: serde_json::Value = serde_json::from_str(&profile).unwrap();
    assert_eq!(profile["model"], "my-model");
    assert_eq!(profile["tools"], true);
    assert_eq!(profile["json_schema"], false);
}

#[test]
fn local_cli_runs_without_cloud_key_and_persists_global_selection() {
    let state = State(std::env::temp_dir().join(format!("agi-cli-llm-{}", uuid::Uuid::new_v4())));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let worker = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let count = stream.read(&mut buffer).unwrap();
                assert!(count > 0);
                request.extend_from_slice(&buffer[..count]);
                if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&request[..end]);
                    let length: usize = header
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse().unwrap())
                        })
                        .unwrap();
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let request = String::from_utf8(request).unwrap();
            assert!(request.starts_with("POST /v1/chat/completions"));
            assert!(!request.to_lowercase().contains("authorization:"));
            let body: serde_json::Value =
                serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
            assert_eq!(body["model"], "offline-model");
            assert!(body.get("chat_template_kwargs").is_none());
            let response = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Локальный ответ\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
        }
    });
    let added = run(
        &state,
        &[
            "llm",
            "add",
            "local",
            "--url",
            &url,
            "--model",
            "offline-model",
            "--context-tokens",
            "16384",
        ],
        "",
    );
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    assert!(
        run(&state, &["llm", "default", "local"], "")
            .status
            .success()
    );
    let answer = run(&state, &["llm", "ask", "Привет"], "");
    assert!(
        answer.status.success(),
        "{}",
        String::from_utf8_lossy(&answer.stderr)
    );
    assert!(String::from_utf8_lossy(&answer.stdout).contains("Локальный ответ"));
    let repl = run(&state, &[], "/clear\n/llm list\n/exit\n");
    assert!(repl.status.success());
    assert!(String::from_utf8_lossy(&repl.stdout).contains("* local"));
    assert!(
        !run(&state, &["llm", "remove", "local"], "")
            .status
            .success()
    );
    let db = rusqlite::Connection::open(state.0.join("agi/chats.sqlite3")).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT value FROM metadata WHERE key='llm_default'",
            [],
            |row| row.get::<_, String>(0)
        )
        .unwrap(),
        "local"
    );
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM chats", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        0
    );
    let ordinary = run(&state, &[], "Привет\n/exit\n");
    assert!(
        ordinary.status.success(),
        "{}",
        String::from_utf8_lossy(&ordinary.stderr)
    );
    assert!(String::from_utf8_lossy(&ordinary.stdout).contains("Локальный ответ"));
    worker.join().unwrap();
    let settings: String = db
        .query_row("SELECT settings_json FROM chats", [], |row| row.get(0))
        .unwrap();
    let settings: serde_json::Value = serde_json::from_str(&settings).unwrap();
    assert!(settings.get("model").is_none());
    assert!(settings.get("llm_profile_id").is_none());
}
