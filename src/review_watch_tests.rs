use super::*;

#[test]
fn automation_defaults_are_offline_and_off_by_default() {
    let directory = std::env::temp_dir().join(format!("agi-watch-defaults-{}", Uuid::new_v4()));
    let store = ReviewStore::at(directory.clone()).unwrap();
    let project = crate::review::tests::test_project(&directory, "http://127.0.0.1:1");
    store.save_project(&project).unwrap();
    let mut settings = store.automation(project.id).unwrap();
    assert!(!settings.enabled);
    assert!(settings.publish);
    assert_eq!(settings.interval_secs, 60);
    settings.interval_secs = 1;
    assert!(store.save_automation(project.id, &settings).is_err());
    settings.interval_secs = 90;
    settings.enabled = true;
    store.save_automation(project.id, &settings).unwrap();
    let reopened = ReviewStore::at(directory.clone()).unwrap();
    assert!(reopened.automation(project.id).unwrap().enabled);
    assert_eq!(reopened.automation(project.id).unwrap().interval_secs, 90);
    let lock = acquire(&store).unwrap().unwrap();
    assert!(acquire(&store).unwrap().is_none());
    drop(lock);
    assert!(acquire(&store).unwrap().is_some());
    std::fs::remove_dir_all(directory).unwrap();
}

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
struct Fixture {
    directory: std::path::PathBuf,
    store: ReviewStore,
    project: Project,
}
impl Fixture {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!("agi-watch-test-{}", Uuid::new_v4()));
        let store = ReviewStore::at(directory.clone()).unwrap();
        let project = crate::review::tests::test_project(&directory, "http://127.0.0.1:1");
        store.save_project(&project).unwrap();
        store
            .save_automation(
                project.id,
                &Automation {
                    enabled: true,
                    ..Default::default()
                },
            )
            .unwrap();
        Self {
            directory,
            store,
            project,
        }
    }
    fn pull(&self, head: &str) -> PullRequest {
        serde_json::from_value(serde_json::json!({"number":1,"title":"test","state":"open","head":{"sha":head,"repo":{"full_name":"external/fork"}},"base":{"sha":"base","repo":{"full_name":self.project.repository}}})).unwrap()
    }
    fn backend(&self) -> MockBackend {
        MockBackend {
            store: self.store.clone(),
            analyzed: Arc::new(AtomicUsize::new(0)),
            published: Arc::new(AtomicUsize::new(0)),
            failures: Arc::new(AtomicUsize::new(0)),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}
struct MockBackend {
    store: ReviewStore,
    analyzed: Arc<AtomicUsize>,
    published: Arc<AtomicUsize>,
    failures: Arc<AtomicUsize>,
}
impl Backend for MockBackend {
    async fn analyze(&self, id: Uuid, project: &Project, pull: &PullRequest) -> Result<ReviewRun> {
        self.analyzed.fetch_add(1, Ordering::SeqCst);
        let mut run = crate::review::tests::test_run(project, "http://127.0.0.1:1");
        run.id = id;
        run.head = pull.head.sha.clone();
        run.pr = Some(pull.number);
        run.status = "completed".into();
        run.markdown = "# Готовое ревью".into();
        self.store.save_run(&run)?;
        Ok(run)
    }
    async fn publish(&self, _project: &Project, mut run: ReviewRun) -> Result<ReviewRun> {
        self.published.fetch_add(1, Ordering::SeqCst);
        if self.failures.load(Ordering::SeqCst) > 0 {
            self.failures.fetch_sub(1, Ordering::SeqCst);
            run.publication = "failed".into();
            run.error = Some("GitHub временно недоступен".into());
        } else {
            run.publication = "published".into();
            run.error = None;
        }
        self.store.save_run(&run)?;
        Ok(run)
    }
}
#[tokio::test]
async fn reviews_new_heads_once_and_deduplicates_after_reopening_state() {
    let fixture = Fixture::new();
    let backend = fixture.backend();
    let pull = fixture.pull("first");
    process_pull(&fixture.store, &fixture.project, &pull, &backend)
        .await
        .unwrap();
    process_pull(&fixture.store, &fixture.project, &pull, &backend)
        .await
        .unwrap();
    let reopened = ReviewStore::at(fixture.directory.clone()).unwrap();
    process_pull(&reopened, &fixture.project, &pull, &backend)
        .await
        .unwrap();
    assert_eq!(backend.analyzed.load(Ordering::SeqCst), 1);
    assert_eq!(backend.published.load(Ordering::SeqCst), 1);
    process_pull(
        &reopened,
        &fixture.project,
        &fixture.pull("second"),
        &backend,
    )
    .await
    .unwrap();
    assert_eq!(backend.analyzed.load(Ordering::SeqCst), 2);
    assert_eq!(backend.published.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.store.history(None).unwrap().len(), 2);
}
#[tokio::test]
async fn retries_publication_from_cached_report_without_new_model_analysis() {
    let fixture = Fixture::new();
    let backend = fixture.backend();
    let pull = fixture.pull("first");
    backend.failures.store(1, Ordering::SeqCst);
    process_pull(&fixture.store, &fixture.project, &pull, &backend)
        .await
        .unwrap();
    let failed = fixture.store.checkpoint(&fixture.project, &pull).unwrap();
    assert_eq!(failed.status, "failed");
    assert!(failed.retry_at > now());
    process_pull(&fixture.store, &fixture.project, &pull, &backend)
        .await
        .unwrap();
    assert_eq!(backend.published.load(Ordering::SeqCst), 1);
    fixture.store.retry_automation(fixture.project.id).unwrap();
    process_pull(&fixture.store, &fixture.project, &pull, &backend)
        .await
        .unwrap();
    assert_eq!(backend.analyzed.load(Ordering::SeqCst), 1);
    assert_eq!(backend.published.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.store.history(None).unwrap().len(), 1);
    assert_eq!(
        fixture
            .store
            .checkpoint(&fixture.project, &pull)
            .unwrap()
            .status,
        "completed"
    );
}
#[tokio::test]
async fn skips_drafts_and_disabled_projects_and_resumes_completed_interrupted_job() {
    let fixture = Fixture::new();
    let backend = fixture.backend();
    let mut pull = fixture.pull("first");
    pull.draft = true;
    process_pull(&fixture.store, &fixture.project, &pull, &backend)
        .await
        .unwrap();
    pull.draft = false;
    fixture
        .store
        .save_automation(fixture.project.id, &Automation::default())
        .unwrap();
    process_pull(&fixture.store, &fixture.project, &pull, &backend)
        .await
        .unwrap();
    assert_eq!(backend.analyzed.load(Ordering::SeqCst), 0);
    fixture
        .store
        .save_automation(
            fixture.project.id,
            &Automation {
                enabled: true,
                ..Default::default()
            },
        )
        .unwrap();
    let mut checkpoint = fixture.store.checkpoint(&fixture.project, &pull).unwrap();
    let id = Uuid::new_v4();
    backend.analyze(id, &fixture.project, &pull).await.unwrap();
    checkpoint.run = Some(id);
    checkpoint.status = "running".into();
    fixture.store.save_checkpoint(&checkpoint).unwrap();
    process_pull(&fixture.store, &fixture.project, &pull, &backend)
        .await
        .unwrap();
    assert_eq!(backend.analyzed.load(Ordering::SeqCst), 1);
    assert_eq!(backend.published.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn local_only_mode_can_later_publish_existing_report() {
    let fixture = Fixture::new();
    let backend = fixture.backend();
    let pull = fixture.pull("first");
    fixture
        .store
        .save_automation(
            fixture.project.id,
            &Automation {
                enabled: true,
                publish: false,
                ..Default::default()
            },
        )
        .unwrap();
    process_pull(&fixture.store, &fixture.project, &pull, &backend)
        .await
        .unwrap();
    assert_eq!(backend.published.load(Ordering::SeqCst), 0);
    fixture
        .store
        .save_automation(
            fixture.project.id,
            &Automation {
                enabled: true,
                publish: true,
                ..Default::default()
            },
        )
        .unwrap();
    process_pull(&fixture.store, &fixture.project, &pull, &backend)
        .await
        .unwrap();
    assert_eq!(backend.analyzed.load(Ordering::SeqCst), 1);
    assert_eq!(backend.published.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn polls_mock_github_and_keeps_project_error_visible() {
    use axum::{Json, Router, routing::get};
    let fixture = Fixture::new();
    let backend = fixture.backend();
    let pull: serde_json::Value = serde_json::to_value(serde_json::json!({"number":1,"title":"from HTTP","head":{"sha":"first","repo":{"full_name":"external/fork"}},"base":{"sha":"base","repo":{"full_name":"owner/repo"}}})).unwrap();
    let app = Router::new().route(
        "/repos/owner/repo/pulls",
        get(move || {
            let pull = pull.clone();
            async move { Json(serde_json::json!([pull])) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let github = Github::with_endpoint(&fixture.project.repository, &endpoint, None).unwrap();
    backend.failures.store(1, Ordering::SeqCst);
    assert!(
        scan_project(&fixture.store, &fixture.project, &github, &backend)
            .await
            .is_err()
    );
    let settings = fixture.store.automation(fixture.project.id).unwrap();
    assert!(settings.last_poll > 0);
    assert!(
        settings
            .error
            .unwrap()
            .contains("GitHub временно недоступен")
    );
    fixture.store.retry_automation(fixture.project.id).unwrap();
    scan_project(&fixture.store, &fixture.project, &github, &backend)
        .await
        .unwrap();
    assert!(
        fixture
            .store
            .automation(fixture.project.id)
            .unwrap()
            .error
            .is_none()
    );
    assert_eq!(backend.analyzed.load(Ordering::SeqCst), 1);
    server.abort();
}
#[tokio::test]
async fn stop_request_terminates_idle_daemon_and_releases_singleton_lock() {
    let fixture = Fixture::new();
    fixture
        .store
        .save_automation(fixture.project.id, &Automation::default())
        .unwrap();
    let store = fixture.store.clone();
    let daemon = tokio::spawn(async move { watch(&store, false).await });
    for _ in 0..50 {
        if status(&fixture.store).unwrap().running {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(status(&fixture.store).unwrap().running);
    assert!(watch(&fixture.store, true).await.is_err());
    stop(&fixture.store).unwrap();
    tokio::time::timeout(Duration::from_secs(5), daemon)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!status(&fixture.store).unwrap().running);
}

#[test]
fn requires_local_generation_and_embeddings_without_contacting_endpoints() {
    let fixture = Fixture::new();
    let settings = Automation {
        enabled: true,
        publish: false,
        ..Default::default()
    };
    assert!(validate_setup(&fixture.store, &fixture.project, &settings).is_err());
    let chats = crate::chat::ChatStore::with_directory(fixture.directory.clone()).unwrap();
    let model = crate::review::tests::test_run(&fixture.project, "http://127.0.0.1:1").model;
    chats.llms().add(model.clone()).unwrap();
    chats.llms().set_default(&model.id).unwrap();
    validate_setup(&fixture.store, &fixture.project, &settings).unwrap();
    let mut project = fixture.project.clone();
    project.embedding.url = format!("{}/embeddings", config::DEFAULT_BASE_URL);
    assert!(validate_setup(&fixture.store, &project, &settings).is_err());
}
