//! GitHub REST transport; no dependency on gh and no credential persistence.
use reqwest::{Client, Method, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

use crate::review::{Project, Result, ReviewError, ReviewRun, ReviewStore};

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PullRequest {
    pub(crate) number: u64,
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) draft: bool,
    #[serde(default)]
    pub(crate) state: String,
    pub(crate) head: GitRef,
    pub(crate) base: GitRef,
}
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GitRef {
    pub(crate) sha: String,
    pub(crate) repo: Option<Repository>,
}
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Repository {
    pub(crate) full_name: String,
}

#[derive(Clone)]
pub(crate) struct Github {
    client: Client,
    base: String,
    token: Option<String>,
    repository: String,
}
impl Github {
    pub(crate) fn new(project: &Project) -> Result<Self> {
        let name = if std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true") {
            "GITHUB_TOKEN"
        } else {
            &project.token_env
        };
        Self::with_endpoint(
            &project.repository,
            "https://api.github.com",
            std::env::var(name).ok(),
        )
    }
    pub(crate) fn with_endpoint(
        repository: &str,
        base: &str,
        token: Option<String>,
    ) -> Result<Self> {
        crate::review::validate_repository(repository)?;
        Ok(Self {
            client: Client::builder()
                .timeout(Duration::from_secs(60))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent("agi-review")
                .build()?,
            base: base.trim_end_matches('/').into(),
            repository: repository.into(),
            token: token.filter(|value| !value.trim().is_empty()),
        })
    }
    pub(crate) fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }
    async fn request(&self, method: Method, tail: &str, body: Option<Value>) -> Result<Value> {
        for attempt in 0..3 {
            let mut request = self
                .client
                .request(
                    method.clone(),
                    format!("{}/repos/{}/{}", self.base, self.repository, tail),
                )
                .header("Accept", "application/vnd.github+json");
            if let Some(token) = &self.token {
                request = request.bearer_auth(token);
            }
            if let Some(body) = &body {
                request = request.json(body);
            }
            match request.send().await {
                Ok(response) if response.status().is_success() => {
                    return Ok(response.json().await?);
                }
                Ok(response) => {
                    let status = response.status();
                    let retry = status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
                    if !retry || attempt == 2 {
                        // Do not echo server error bodies: they may reflect credentials or PR data.
                        return Err(ReviewError::Invalid(format!("GitHub HTTP {status}")));
                    }
                }
                Err(error) => {
                    if attempt == 2 || !(error.is_timeout() || error.is_connect()) {
                        return Err(error.into());
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
        }
        Err(ReviewError::Invalid("GitHub недоступен".into()))
    }
    pub(crate) async fn pull(&self, number: u64) -> Result<PullRequest> {
        if number == 0 {
            return Err(ReviewError::Invalid(
                "Номер PR должен быть больше нуля".into(),
            ));
        }
        Ok(serde_json::from_value(
            self.request(Method::GET, &format!("pulls/{number}"), None)
                .await?,
        )?)
    }
    pub(crate) async fn pulls(&self) -> Result<Vec<PullRequest>> {
        let mut all = Vec::new();
        for page in 1..=100 {
            let batch: Vec<PullRequest> = serde_json::from_value(
                self.request(
                    Method::GET,
                    &format!("pulls?state=open&per_page=100&page={page}"),
                    None,
                )
                .await?,
            )?;
            let done = batch.len() < 100;
            all.extend(batch);
            if done {
                return Ok(all);
            }
        }
        Err(ReviewError::Invalid("Слишком много открытых PR".into()))
    }
    async fn comments(&self, number: u64) -> Result<Vec<Value>> {
        let mut all = Vec::new();
        for page in 1..=100 {
            let batch: Vec<Value> = serde_json::from_value(
                self.request(
                    Method::GET,
                    &format!("issues/{number}/comments?per_page=100&page={page}"),
                    None,
                )
                .await?,
            )?;
            let done = batch.len() < 100;
            all.extend(batch);
            if done {
                return Ok(all);
            }
        }
        Err(ReviewError::Invalid("Слишком много комментариев PR".into()))
    }
    pub(crate) async fn publish_run(&self, run: &mut ReviewRun) -> Result<()> {
        let number = run
            .pr
            .ok_or_else(|| ReviewError::Invalid("Локальное ревью не публикуется".into()))?;
        if self.token.is_none() {
            return Err(ReviewError::Invalid(
                "Для публикации нужен GitHub-токен из переменной окружения".into(),
            ));
        }
        let current = self.pull(number).await?;
        if current.head.sha != run.head || current.state == "closed" {
            run.publication = "stale".into();
            return Ok(());
        }
        let marker = "<!-- agi-review -->";
        // Update only a comment from the current authenticated principal.
        let login = if std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true") {
            // GITHUB_TOKEN is an installation token; /user is unavailable for it.
            "github-actions[bot]".to_owned()
        } else {
            let me: Value = self
                .client
                .get(format!("{}/user", self.base))
                .bearer_auth(self.token.as_deref().unwrap_or_default())
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            me["login"]
                .as_str()
                .ok_or_else(|| ReviewError::Invalid("GitHub не вернул пользователя токена".into()))?
                .to_owned()
        };
        let comment = self.comments(number).await?.into_iter().find(|comment| {
            comment["user"]["login"].as_str() == Some(login.as_str())
                && comment["body"]
                    .as_str()
                    .is_some_and(|body| body.starts_with(marker))
        });
        let mut body = format!("{marker}\n{}", run.markdown);
        if body.chars().count() > 60_000 {
            body = body.chars().take(59_000).collect();
            body.push_str(
                "\n\nОтчёт сокращён для GitHub. Полная версия — в истории agi и артефактах Action.",
            );
        }
        // Recheck after fetching comments; a newer PR commit must not get an old report.
        let current = self.pull(number).await?;
        if current.head.sha != run.head || current.state == "closed" {
            run.publication = "stale".into();
            return Ok(());
        }
        if let Some(id) = comment.and_then(|c| c["id"].as_u64()) {
            self.request(
                Method::PATCH,
                &format!("issues/comments/{id}"),
                Some(json!({"body":body})),
            )
            .await?;
        } else {
            self.request(
                Method::POST,
                &format!("issues/{number}/comments"),
                Some(json!({"body":body})),
            )
            .await?;
        }
        run.publication = "published".into();
        Ok(())
    }
}

pub(crate) async fn publish(
    store: &ReviewStore,
    project: &Project,
    run: &mut ReviewRun,
) -> Result<()> {
    if run.status != "completed" {
        return Err(ReviewError::Invalid(
            "Можно публиковать только завершённое ревью".into(),
        ));
    }
    let result = match Github::new(project) {
        Ok(github) => github.publish_run(run).await,
        Err(error) => Err(error),
    };
    if let Err(error) = result {
        run.publication = "failed".into();
        run.error = Some(error.to_string());
    } else {
        run.error = None;
    }
    store.save_run(run)?;
    let path = store
        .directory
        .join("reviews")
        .join(run.id.to_string())
        .join("review.json");
    if path.parent().is_some_and(|parent| parent.is_dir()) {
        std::fs::write(path, serde_json::to_vec_pretty(run)?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, extract::Request, response::IntoResponse, routing::any};
    use std::sync::{Arc, Mutex};

    async fn fixture(
        existing: bool,
        stale: bool,
        fail_publish: bool,
    ) -> (
        Github,
        Arc<Mutex<Vec<(String, String, Value)>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let app = Router::new().fallback(any(move |request: Request<Body>| {
            let captured = captured.clone();
            async move {
                assert_eq!(request.headers().get("authorization").unwrap(), "Bearer fixture-token");
                let method = request.method().to_string();
                let path = request.uri().path().to_owned();
                let body = axum::body::to_bytes(request.into_body(), 100_000).await.unwrap();
                let body: Value = if body.is_empty() { Value::Null } else { serde_json::from_slice(&body).unwrap() };
                captured.lock().unwrap().push((method.clone(), path.clone(), body));
                let mut status = StatusCode::OK;
                let response = if path.ends_with("/pulls/1") {
                    json!({"number":1,"title":"fixture","head":{"sha":if stale {"new-head"} else {"abc"},"repo":{"full_name":"owner/repo"}},"base":{"sha":"base","repo":{"full_name":"owner/repo"}}})
                } else if path == "/user" { json!({"login":"review-user","id":1}) }
                else if method == "GET" && path.ends_with("/comments") {
                    if existing { json!([{"id":23,"body":"<!-- agi-review -->\nprevious","user":{"login":"review-user"}},{"id":24,"body":"<!-- agi-review -->\nother","user":{"login":"other"}}]) } else { json!([]) }
                } else if method == "POST" || method == "PATCH" {
                    if fail_publish { status = StatusCode::FORBIDDEN; }
                    json!({"id":23})
                } else { status = StatusCode::BAD_REQUEST; json!({}) };
                (status, axum::Json(response)).into_response()
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let worker = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (
            Github::with_endpoint("owner/repo", &endpoint, Some("fixture-token".into())).unwrap(),
            requests,
            worker,
        )
    }
    #[tokio::test]
    async fn creates_or_updates_only_own_comment_and_skips_stale_head() {
        for (existing, stale) in [(false, false), (true, false), (true, true)] {
            let (github, requests, worker) = fixture(existing, stale, false).await;
            let directory =
                std::env::temp_dir().join(format!("agi-review-github-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&directory).unwrap();
            let project = crate::review::tests::test_project(&directory, "http://localhost:1");
            let mut run = crate::review::tests::test_run(&project, "http://localhost:1");
            run.status = "completed".into();
            run.markdown = "fixture report".into();
            github.publish_run(&mut run).await.unwrap();
            assert_eq!(run.publication, if stale { "stale" } else { "published" });
            let requests = requests.lock().unwrap();
            let writes: Vec<_> = requests
                .iter()
                .filter(|(method, _, _)| method == "POST" || method == "PATCH")
                .collect();
            assert_eq!(writes.len(), if stale { 0 } else { 1 });
            if !stale {
                assert_eq!(writes[0].0, if existing { "PATCH" } else { "POST" });
                if existing {
                    assert!(writes[0].1.ends_with("/23"));
                }
                assert!(
                    writes[0].2["body"]
                        .as_str()
                        .unwrap()
                        .contains("fixture report")
                );
            }
            worker.abort();
            std::fs::remove_dir_all(directory).unwrap();
        }
    }
    #[tokio::test]
    async fn publication_error_keeps_the_report_intact() {
        let (github, _, worker) = fixture(false, false, true).await;
        let directory =
            std::env::temp_dir().join(format!("agi-review-publish-error-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let project = crate::review::tests::test_project(&directory, "http://localhost:1");
        let mut run = crate::review::tests::test_run(&project, "http://localhost:1");
        run.markdown = "intact report".into();
        assert!(github.publish_run(&mut run).await.is_err());
        assert_eq!(run.markdown, "intact report");
        worker.abort();
        std::fs::remove_dir_all(directory).unwrap();
    }
}
