use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

struct State(PathBuf);
impl Drop for State {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn run(state: &Path, args: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_agi"))
        .args(args)
        .env("XDG_STATE_HOME", state)
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
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}
fn git(path: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
#[tokio::test]
async fn configures_project_in_repl_and_reviews_local_diff_with_shared_model() {
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let observed = requests.clone();
    let app = axum::Router::new().route("/{*path}", axum::routing::post(move |headers: axum::http::HeaderMap, axum::Json(body): axum::Json<Value>| {
        let observed = observed.clone();
        async move {
            assert!(!headers.contains_key("authorization"));
            observed.lock().unwrap().push(body.clone());
            if let Some(input) = body["input"].as_array() {
                return axum::Json(json!({"data":input.iter().enumerate().map(|(i,_)| json!({"index":i,"embedding":[1.0,0.2]})).collect::<Vec<_>>() }));
            }
            axum::Json(json!({"choices":[{"message":{"content":"## Потенциальные баги\n`a.rs`: return 2 нарушает правило README.\n## Архитектурные проблемы\nНе обнаружено.\n## Рекомендации\nДобавить тест."},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":10,"total_tokens":20}}))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let worker = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let captured = requests.clone();
    tokio::task::spawn_blocking(move || {
        let state =
            State(std::env::temp_dir().join(format!("agi-cli-review-{}", uuid::Uuid::new_v4())));
        let repo = state.0.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "--template="]);
        git(
            &repo,
            &["remote", "add", "origin", "git@github.com:owner/repo.git"],
        );
        std::fs::write(repo.join("a.rs"), "fn a() { return 1; }\n").unwrap();
        std::fs::write(
            repo.join("README.md"),
            "# Правило\nФункция a должна возвращать 1.\n",
        )
        .unwrap();
        git(&repo, &["add", "."]);
        git(
            &repo,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "initial",
            ],
        );
        run(
            &state.0,
            &[
                "llm",
                "add",
                "review-local",
                "--url",
                &url,
                "--model",
                "review-test",
                "--context-tokens",
                "32768",
            ],
            "",
        );
        run(&state.0, &["llm", "default", "review-local"], "");
        run(
            &state.0,
            &[
                "rag",
                "embeddings",
                "set",
                "--url",
                &format!("{url}/embeddings"),
                "--model",
                "review-embeddings",
            ],
            "",
        );
        let before = captured.lock().unwrap().len();
        let output = run(
            &state.0,
            &[],
            &format!("/review\n1\n{}\nowner/repo\n11\n5\n/exit\n", repo.display()),
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("AI-ревью кода"));
        assert_eq!(
            captured.lock().unwrap().len(),
            before,
            "saving project contacted endpoints"
        );
        let db = rusqlite::Connection::open(state.0.join("agi/review.sqlite3")).unwrap();
        let stored: String = db
            .query_row("SELECT project_json FROM review_projects", [], |r| r.get(0))
            .unwrap();
        let project: Value = serde_json::from_str(&stored).unwrap();
        let id = project["id"].as_str().unwrap();
        std::fs::write(repo.join("a.rs"), "fn a() { return 2; }\n").unwrap();
        let output = state.0.join("output");
        run(
            &state.0,
            &[
                "review",
                "run",
                "--project",
                id,
                "--local",
                "--output",
                output.to_str().unwrap(),
            ],
            "",
        );
        let report: Value =
            serde_json::from_slice(&std::fs::read(output.join("review.json")).unwrap()).unwrap();
        assert_eq!(report["status"], "completed");
        assert_eq!(report["model"]["model"], "review-test");
        assert_eq!(report["publication"], "not_published");
        assert!(
            std::fs::read_to_string(output.join("review.md"))
                .unwrap()
                .contains("Потенциальные баги")
        );
        let request = captured
            .lock()
            .unwrap()
            .iter()
            .find(|r| r["messages"].is_array())
            .cloned()
            .unwrap();
        let prompt = request["messages"][1]["content"].as_str().unwrap();
        assert!(prompt.contains("+fn a() { return 2; }"));
        assert!(prompt.contains("должна возвращать 1"));
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM review_runs", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
    })
    .await
    .unwrap();
    worker.abort();
}

#[test]
fn background_watcher_survives_start_command_and_stops_without_runner() {
    let state = State(std::env::temp_dir().join(format!("agi-watch-cli-{}", uuid::Uuid::new_v4())));
    let started = run(&state.0, &["review", "automation", "start"], "");
    assert!(String::from_utf8_lossy(&started.stdout).contains("Фоновая проверка запущена"));
    let first = run(&state.0, &["review", "automation", "status"], "");
    assert!(String::from_utf8_lossy(&first.stdout).contains("Фоновый процесс работает"));
    let db = rusqlite::Connection::open(state.0.join("agi/review.sqlite3")).unwrap();
    let first_pid: i64 = db
        .query_row("SELECT pid FROM review_daemon", [], |row| row.get(0))
        .unwrap();
    let started_again = run(&state.0, &["review", "automation", "start"], "");
    assert!(String::from_utf8_lossy(&started_again.stdout).contains("уже работает"));
    assert_eq!(
        first_pid,
        db.query_row("SELECT pid FROM review_daemon", [], |row| row
            .get::<_, i64>(0))
            .unwrap()
    );
    let stopped = run(&state.0, &["review", "automation", "stop"], "");
    assert!(String::from_utf8_lossy(&stopped.stdout).contains("Остановка запрошена"));
    let mut ended = false;
    for _ in 0..40 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let output = run(&state.0, &["review", "automation", "status"], "");
        if String::from_utf8_lossy(&output.stdout).contains("процесс остановлен") {
            ended = true;
            break;
        }
    }
    assert!(ended, "watcher did not stop");
    run(&state.0, &["review", "watch", "--once"], "");
}
