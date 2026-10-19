use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use axum::{Json, Router, http::HeaderMap, routing::post};
use serde_json::{Value, json};

struct Workspace(PathBuf);

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(state: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agi"))
        .args(args)
        .env("XDG_STATE_HOME", state)
        .env_remove("NEURALDEEP_API_KEY")
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .env("HTTP_PROXY", "http://127.0.0.1:1")
        .env("ALL_PROXY", "http://127.0.0.1:1")
        .env("NO_PROXY", "localhost,127.0.0.1")
        .output()
        .unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert_success(&output);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn release_cli_uses_active_profile_and_preserves_existing_outputs() {
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = requests.clone();
    let app = Router::new().route("/v1/chat/completions", post(move |headers: HeaderMap, Json(body): Json<Value>| {
        let captured = captured.clone();
        async move {
            assert!(!headers.contains_key("authorization"));
            captured.lock().unwrap().push(body);
            Json(json!({"choices":[{"message":{"content":"## Новые возможности\nДобавлен экспорт отчёта.\n## Исправления\nНет подтверждённых изменений.\n## Прочие изменения\nОбновлена документация."},"finish_reason":"stop"}]}))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::task::spawn_blocking(move || {
        let workspace = Workspace(
            std::env::temp_dir().join(format!("agi-cli-release-{}", uuid::Uuid::new_v4())),
        );
        let repo = workspace.0.join("repository with spaces");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "--template="]);
        for (text, message) in [
            ("# Initial\n", "Initial"),
            ("# Export reports\n", "Add export"),
        ] {
            std::fs::write(repo.join("README.md"), text).unwrap();
            git(&repo, &["add", "."]);
            git(
                &repo,
                &[
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.test",
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "-m",
                    message,
                ],
            );
        }
        assert_success(&run(
            &workspace.0,
            &[
                "llm",
                "add",
                "release-local",
                "--url",
                &endpoint,
                "--model",
                "release-model",
                "--context-tokens",
                "32768",
            ],
        ));
        assert_success(&run(&workspace.0, &["llm", "default", "release-local"]));
        assert!(requests.lock().unwrap().is_empty());
        let output = workspace.0.join("release output");
        let args = [
            "release",
            "prepare",
            "--repo",
            repo.to_str().unwrap(),
            "--from",
            "HEAD~1",
            "--output",
            output.to_str().unwrap(),
        ];
        let completed = run(&workspace.0, &args);
        assert_success(&completed);
        assert!(String::from_utf8_lossy(&completed.stdout).contains("Черновик релиза сохранён"));
        let notes = std::fs::read_to_string(output.join("RELEASE_NOTES.md")).unwrap();
        assert!(notes.contains("Добавлен экспорт отчёта"));
        let report: Value =
            serde_json::from_slice(&std::fs::read(output.join("release.json")).unwrap()).unwrap();
        assert_eq!(report["model"], "release-model");
        assert_eq!(report["profile"], "release-local");
        assert_eq!(report["dirty_worktree"], false);
        assert_eq!(report["usage"], Value::Null);
        assert_eq!(requests.lock().unwrap().len(), 1);
        let repeated = run(&workspace.0, &args);
        assert!(!repeated.status.success());
        assert!(String::from_utf8_lossy(&repeated.stderr).contains("уже существует"));
        assert_eq!(
            std::fs::read_to_string(output.join("RELEASE_NOTES.md")).unwrap(),
            notes
        );
        assert_eq!(requests.lock().unwrap().len(), 1);
        let empty_output = workspace.0.join("empty");
        let empty = run(
            &workspace.0,
            &[
                "release",
                "prepare",
                "--repo",
                repo.to_str().unwrap(),
                "--from",
                "HEAD",
                "--output",
                empty_output.to_str().unwrap(),
            ],
        );
        assert_success(&empty);
        assert!(String::from_utf8_lossy(&empty.stdout).contains("Нет изменений"));
        assert!(!empty_output.exists());
        assert_eq!(requests.lock().unwrap().len(), 1);
    })
    .await
    .unwrap();
    server.abort();
}
