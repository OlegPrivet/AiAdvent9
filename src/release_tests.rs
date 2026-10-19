use super::*;
use crate::test_http::MockServer;
use serde_json::{Value, json};
use std::process::Command;

struct Fixture {
    directory: PathBuf,
    args: ReleaseArgs,
}

impl Fixture {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!("agi-release-{}", uuid::Uuid::new_v4()));
        let repo = directory.join("repo");
        fs::create_dir_all(&repo).unwrap();
        let fixture = Self {
            args: ReleaseArgs {
                repo,
                from: "HEAD~1".into(),
                to: "HEAD".into(),
                output: directory.join("output"),
            },
            directory,
        };
        fixture.git(&["init", "--template="]);
        fs::write(
            fixture.args.repo.join("notes.md"),
            "# Первоначальная версия\n",
        )
        .unwrap();
        fixture.commit("Начальная версия");
        fs::write(
            fixture.args.repo.join("notes.md"),
            "# Новая возможность: экспорт\n",
        )
        .unwrap();
        fs::write(
            fixture.args.repo.join(".env"),
            "PRIVATE_SECRET_DO_NOT_SEND=secret\n",
        )
        .unwrap();
        fixture.commit("Добавить экспорт");
        fixture
    }

    fn git(&self, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.args.repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn commit(&self, message: &str) {
        self.git(&["add", "."]);
        self.git(&[
            "-c",
            "user.name=Release Test",
            "-c",
            "user.email=release@example.test",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            message,
        ]);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn profile(server: &MockServer) -> LlmProfile {
    LlmProfile {
        id: "release-test".into(),
        provider: Provider::Local,
        base_url: server.url.clone(),
        model: "test-model".into(),
        context_tokens: 32768,
        tools: false,
        json_schema: false,
    }
}

fn success(_: &Value) -> (u16, String) {
    (200, json!({
        "choices": [{"message": {"content": "## Новые возможности\nДобавлен экспорт.\n## Исправления\nНет подтверждённых изменений.\n## Прочие изменения\nНет подтверждённых изменений."}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 100, "completion_tokens": 40, "total_tokens": 140}
    }).to_string())
}

#[tokio::test]
async fn prepares_committed_changes_with_shared_model_and_writes_traceable_draft() {
    let fixture = Fixture::new();
    // These worktree contents must not replace the committed snapshot sent to AI.
    fs::write(
        fixture.args.repo.join("notes.md"),
        "UNCOMMITTED_DO_NOT_SEND",
    )
    .unwrap();
    fs::write(
        fixture.args.repo.join("untracked.md"),
        "UNTRACKED_DO_NOT_SEND",
    )
    .unwrap();
    let server = MockServer::new_local(success);
    let (notes, report) = prepare(&fixture.args, &profile(&server))
        .await
        .unwrap()
        .unwrap();
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["model"], "test-model");
    assert_eq!(requests[0]["max_tokens"], 4096);
    assert!(requests[0].get("tools").is_none());
    let evidence = requests[0]["messages"][1]["content"].as_str().unwrap();
    assert!(evidence.contains("Новая возможность: экспорт"));
    assert!(evidence.contains("Добавить экспорт"));
    for excluded in [
        "PRIVATE_SECRET_DO_NOT_SEND",
        "UNCOMMITTED_DO_NOT_SEND",
        "UNTRACKED_DO_NOT_SEND",
    ] {
        assert!(!evidence.contains(excluded));
    }
    assert!(notes.contains("Добавлен экспорт"));
    assert!(notes.contains("не выполняются"));
    assert!(report.dirty_worktree);
    assert_eq!(report.files, ["notes.md"]);
    assert!(report.skipped.iter().any(|item| item.contains(".env")));
    assert_eq!(report.base_sha.len(), 40);
    assert_eq!(report.head_sha.len(), 40);
    assert_ne!(report.base_sha, report.head_sha);
    write_artifacts(&fixture.args.output, &notes, &report).unwrap();
    let stored: Value =
        serde_json::from_slice(&fs::read(fixture.args.output.join(REPORT_FILE)).unwrap()).unwrap();
    assert_eq!(stored["status"], "draft");
    assert_eq!(stored["model"], "test-model");
    assert_eq!(stored["usage"]["total_tokens"], 140);
    assert_eq!(
        fs::read_to_string(fixture.args.output.join(NOTES_FILE)).unwrap(),
        notes
    );
    assert!(write_artifacts(&fixture.args.output, "replacement", &report).is_err());
    assert_eq!(
        fs::read_to_string(fixture.args.output.join(NOTES_FILE)).unwrap(),
        notes
    );
}

#[tokio::test]
async fn empty_ranges_and_invalid_revisions_do_not_call_model_or_create_output() {
    let mut fixture = Fixture::new();
    let server = MockServer::new_local(success);
    fixture.args.from = "HEAD".into();
    assert!(
        prepare(&fixture.args, &profile(&server))
            .await
            .unwrap()
            .is_none()
    );
    fixture.args.from = "missing-tag".into();
    assert!(
        prepare(&fixture.args, &profile(&server))
            .await
            .unwrap_err()
            .to_string()
            .contains("Не найден")
    );
    fixture.args.from = "--help".into();
    assert!(prepare(&fixture.args, &profile(&server)).await.is_err());
    assert!(server.requests().is_empty());
    assert!(!fixture.args.output.exists());
}

#[tokio::test]
async fn rejects_large_diff_and_small_context_without_contacting_model() {
    let fixture = Fixture::new();
    fs::write(
        fixture.args.repo.join("notes.md"),
        "more changes\n".repeat(1500),
    )
    .unwrap();
    fixture.commit("Большое изменение");
    let server = MockServer::new_local(success);
    let mut selected = profile(&server);
    selected.context_tokens = 8192;
    assert!(
        prepare(&fixture.args, &selected)
            .await
            .unwrap_err()
            .to_string()
            .contains("меньший диапазон")
    );
    selected.context_tokens = 4096;
    assert!(
        prepare(&fixture.args, &selected)
            .await
            .unwrap_err()
            .to_string()
            .contains("8192")
    );
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn model_errors_and_incomplete_answers_do_not_produce_release_files() {
    let fixture = Fixture::new();
    for (status, body) in [
        (
            500,
            json!({"error":{"message":"test service failure"}}).to_string(),
        ),
        (200, "not JSON".into()),
        (
            200,
            json!({"choices":[{"message":{"content":"   "},"finish_reason":"stop"}]}).to_string(),
        ),
        (
            200,
            json!({"choices":[{"message":{"content":"Начало ответа"},"finish_reason":"length"}]})
                .to_string(),
        ),
    ] {
        let server = MockServer::new_local(move |_| (status, body.clone()));
        assert!(prepare(&fixture.args, &profile(&server)).await.is_err());
        assert!(!fixture.args.output.exists());
    }
}

#[test]
fn rejects_existing_artifacts_and_non_directory_output() {
    let fixture = Fixture::new();
    fs::create_dir_all(&fixture.args.output).unwrap();
    fs::write(fixture.args.output.join(REPORT_FILE), "keep").unwrap();
    assert!(check_output(&fixture.args.output).is_err());
    assert_eq!(
        fs::read_to_string(fixture.args.output.join(REPORT_FILE)).unwrap(),
        "keep"
    );
    assert!(check_output(&fixture.args.output.join(REPORT_FILE)).is_err());
}

#[cfg(unix)]
#[test]
fn rejects_dangling_output_symlinks() {
    let fixture = Fixture::new();
    fs::create_dir_all(&fixture.args.output).unwrap();
    std::os::unix::fs::symlink("missing", fixture.args.output.join(NOTES_FILE)).unwrap();
    assert!(check_output(&fixture.args.output).is_err());
}
