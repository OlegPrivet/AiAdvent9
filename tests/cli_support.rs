use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use axum::{Json, Router, http::HeaderMap, response::IntoResponse, routing::post};
use serde_json::{Value, json};

struct State(PathBuf);
impl State {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("agi-support-cli-{}", uuid::Uuid::new_v4())))
    }
}
impl Drop for State {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn run(state: &State, args: &[&str], input: &str) -> String {
    let directory = state.0.clone();
    let args = args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let input = input.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut process = Command::new(env!("CARGO_BIN_EXE_agi"))
            .args(args)
            .env("XDG_STATE_HOME", directory)
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
        let result = process.wait_with_output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8(result.stdout).unwrap()
    })
    .await
    .unwrap()
}

struct Mock {
    url: String,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
    embedding_response: Arc<Mutex<Option<(u16, Value)>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Mock {
    async fn new() -> Self {
        let requests = Arc::new(Mutex::new(vec![]));
        let embeddings = requests.clone();
        let chat = requests.clone();
        let embedding_response = Arc::new(Mutex::new(None::<(u16, Value)>));
        let response = embedding_response.clone();
        let router = Router::new()
            .route("/v1/embeddings", post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let captured = embeddings.clone();
                let response = response.clone();
                async move {
                    assert!(headers.get("authorization").is_none());
                    captured.lock().unwrap().push(("embedding".into(), body.clone()));
                    if let Some((status, body)) = response.lock().unwrap().clone() {
                        return (axum::http::StatusCode::from_u16(status).unwrap(), Json(body)).into_response();
                    }
                    Json(json!({"data": body["input"].as_array().unwrap().iter().enumerate()
                        .map(|(index, _)| json!({"index":index,"embedding":[1.0,0.0]})).collect::<Vec<_>>()})).into_response()
                }
            }))
            .route("/v1/chat/completions", post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let captured = chat.clone();
                async move {
                    assert!(headers.get("authorization").is_none());
                    assert!(body.get("tools").is_none());
                    assert!(body.get("response_format").is_none());
                    let messages = body["messages"].as_array().unwrap();
                    let context = messages.iter().find_map(|message| {
                        let value: Value = serde_json::from_str(message["content"].as_str()?).ok()?;
                        value.get("support_context").cloned()
                    }).expect("MCP data in LLM request");
                    let documented = messages.iter().any(|m| m["content"].as_str().unwrap_or("").contains("support_faq.md"));
                    let id = context["ticket"]["id"].as_str().unwrap_or("");
                    let answer = if !documented {
                        assert!(messages.iter().any(|m| m["content"].as_str().unwrap_or("").contains("Подтверждающие фрагменты документов не найдены")));
                        "Недостаточно документации. Уточните текст ошибки."
                    } else { match id {
                        "T-103" => "Проверьте базовый URL с /v1 по тикету T-103 [1].",
                        "T-104" => "Задайте NEURALDEEP_API_KEY в окружении по тикету T-104 [1].",
                        _ => "Проверьте поддержку tools по тикету T-105 [1].",
                    }};
                    captured.lock().unwrap().push(("chat".into(), body));
                    let body = format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                        json!({"choices":[{"index":0,"delta":{"content":answer},"finish_reason":"stop"}]}),
                        json!({"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120}}));
                    ([("content-type", "text/event-stream")], body).into_response()
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            url,
            requests,
            embedding_response,
            task,
        }
    }
}

async fn configure(state: &State, mock: &Mock, index: bool) {
    run(
        state,
        &[
            "llm",
            "add",
            "support-mock",
            "--url",
            &mock.url,
            "--model",
            "mock",
            "--context-tokens",
            "8192",
        ],
        "",
    )
    .await;
    run(state, &["llm", "default", "support-mock"], "").await;
    run(
        state,
        &[
            "rag",
            "embeddings",
            "set",
            "--url",
            &format!("{}/embeddings", mock.url),
            "--model",
            "mock-embed",
        ],
        "",
    )
    .await;
    if index {
        let faq = Path::new(env!("CARGO_MANIFEST_DIR")).join("projetcDocs/support_faq.md");
        run(state, &["rag", "add", faq.to_str().unwrap()], "").await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn support_uses_mcp_then_rag_preserves_followups_and_isolates_tickets() {
    let state = State::new();
    let mock = Mock::new().await;
    configure(&state, &mock, true).await;
    mock.requests.lock().unwrap().clear();
    let output = run(&state, &[], "/rag chat on\n/support setup\n/support ticket T-103\nПочему не работает подключение?\nЯ уже изменил URL, но ошибка осталась\n/support ticket T-104\nПочему не работает подключение?\n/support ticket T-105\nПочему не работают инструменты?\n/support off\n/exit\n").await;
    assert!(!output.contains("Ошибка запроса"), "{output}");
    assert!(output.contains("Проверьте базовый URL"), "{output}");
    assert!(output.contains("Задайте NEURALDEEP_API_KEY"));
    assert!(output.contains("Проверьте поддержку tools"));
    assert!(output.contains("Данные MCP: тикет T-103, пользователь U-17"));
    assert!(output.contains("Найденные источники:"));
    let requests = mock.requests.lock().unwrap();
    let chats = requests
        .iter()
        .filter(|(kind, _)| kind == "chat")
        .map(|(_, v)| v)
        .collect::<Vec<_>>();
    assert_eq!(chats.len(), 4, "{output}");
    assert!(
        chats[1]
            .to_string()
            .contains("Почему не работает подключение?")
    );
    assert!(chats[1].to_string().contains("Проверьте базовый URL"));
    assert!(!chats[2].to_string().contains("T-103"));
    assert!(!chats[2].to_string().contains("Я уже изменил URL"));
    assert!(!chats[3].to_string().contains("T-104"));
    assert_eq!(
        requests
            .iter()
            .filter(|(kind, body)| kind == "embedding" && body["input"].to_string().contains("404"))
            .count(),
        2
    );
    let database = rusqlite::Connection::open(state.0.join("agi/chats.sqlite3")).unwrap();
    let settings: String = database
        .query_row("SELECT settings_json FROM chats", [], |row| row.get(0))
        .unwrap();
    let settings: Value = serde_json::from_str(&settings).unwrap();
    assert_eq!(settings["rag_chat_enabled"], true);
    assert_eq!(settings["rag_options"]["strict"], true);
    assert_eq!(settings["support"]["enabled"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_and_mcp_errors_never_call_llm_or_embeddings() {
    let state = State::new();
    let mock = Mock::new().await;
    configure(&state, &mock, false).await;
    mock.requests.lock().unwrap().clear();
    let output = run(&state, &[], "/support setup\n/support ticket T-103\n/support context\n/support ticket missing\nПочему не работает?\n/exit\n").await;
    assert!(output.contains("404 при POST"), "{output}");
    assert!(output.contains("Тикет missing не найден"), "{output}");
    assert!(mock.requests.lock().unwrap().is_empty());
    std::fs::write(state.0.join("agi/support-demo/tickets.json"), "{").unwrap();
    let database = rusqlite::Connection::open(state.0.join("agi/chats.sqlite3")).unwrap();
    let id: String = database
        .query_row("SELECT id FROM chats", [], |row| row.get(0))
        .unwrap();
    let output = run(
        &state,
        &["--restore", &id],
        "/support ticket T-103\nПочему?\n/exit\n",
    )
    .await;
    assert!(output.contains("Некорректный JSON"), "{output}");
    assert!(mock.requests.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rag_errors_stop_before_answer_and_empty_results_request_clarification() {
    let state = State::new();
    let mock = Mock::new().await;
    configure(&state, &mock, false).await;
    mock.requests.lock().unwrap().clear();
    let output = run(&state, &[], "/support on\nПочему не работает?\n/exit\n").await;
    assert!(output.contains("Индекс пуст"), "{output}");
    assert!(mock.requests.lock().unwrap().is_empty());
    let faq = Path::new(env!("CARGO_MANIFEST_DIR")).join("projetcDocs/support_faq.md");
    run(&state, &["rag", "add", faq.to_str().unwrap()], "").await;
    for (status, body, expected) in [
        (200, json!({"data":[]}), "эмбеддингов"),
        (400, json!({"error":"unavailable"}), "HTTP 400"),
    ] {
        *mock.embedding_response.lock().unwrap() = Some((status, body));
        mock.requests.lock().unwrap().clear();
        let output = run(&state, &[], "/support on\nПочему не работает?\n/exit\n").await;
        assert!(output.contains("RAG поддержки"), "{output}");
        assert!(output.contains(expected), "{output}");
        assert_eq!(
            mock.requests
                .lock()
                .unwrap()
                .iter()
                .filter(|(kind, _)| kind == "chat")
                .count(),
            0
        );
    }
    *mock.embedding_response.lock().unwrap() = None;
    let rag = rusqlite::Connection::open(state.0.join("agi/rag.sqlite3")).unwrap();
    let perpendicular = [0.0_f32, 1.0]
        .into_iter()
        .flat_map(f32::to_le_bytes)
        .collect::<Vec<_>>();
    rag.execute("UPDATE chunks SET vector=?1", [perpendicular])
        .unwrap();
    mock.requests.lock().unwrap().clear();
    let output = run(
        &state,
        &[],
        "/rag filter similarity\n/support on\nПочему не работает?\n/exit\n",
    )
    .await;
    assert!(
        output.contains("Недостаточно документации. Уточните текст ошибки."),
        "{output}"
    );
    assert!(!output.contains("Найденные источники:"));
    assert_eq!(
        mock.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(kind, _)| kind == "chat")
            .count(),
        1
    );
}
