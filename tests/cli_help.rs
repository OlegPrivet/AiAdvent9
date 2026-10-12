use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::{Json, Router, routing::post};
use serde_json::{Value, json};

struct State(PathBuf);

impl Drop for State {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(state: &State, args: &[&str], input: &str) -> String {
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
    let output = process.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn sse(delta: Value, finish: &str) -> axum::response::Response {
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":delta,"finish_reason":finish}]}),
        json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}})
    );
    ([("content-type", "text/event-stream")], body).into_response()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_help_retrieves_documents_uses_mcp_and_reports_errors() {
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = requests.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let router = Router::new().route("/v1/{*path}", post(move |headers: HeaderMap, Json(body): Json<Value>| {
        let captured = captured.clone();
        async move {
            assert!(headers.get("authorization").is_none());
            captured.lock().unwrap().push(body.clone());
            if let Some(input) = body.get("input") {
                let texts = input.as_array().unwrap();
                if texts.iter().any(|text| text == "Ошибка эмбеддингов") {
                    return (StatusCode::SERVICE_UNAVAILABLE, "embeddings offline").into_response();
                }
                return Json(json!({"data":texts.iter().enumerate().map(|(index, _)|
                    json!({"index":index,"embedding":[1.0,0.0]})).collect::<Vec<_>>()})).into_response();
            }
            let messages = body["messages"].as_array().unwrap();
            let question = messages.iter().rev().find(|message| message["role"] == "user").unwrap()["content"].as_str().unwrap();
            assert!(messages.iter().any(|message| message["role"] == "system" &&
                message["content"].as_str().unwrap().contains("Ты ассистент разработчика")));
            match question {
                "Без RAG" => {
                    assert!(messages.iter().any(|message| message["content"].as_str().is_some_and(|text| text.contains("RAG выключен"))));
                    assert!(!messages.iter().any(|message| message["content"].as_str().is_some_and(|text| text.contains("Ниже найдены фрагменты"))));
                    return sse(json!({"content":"Документация не загружена; включите /rag on."}), "stop");
                }
                "Ошибка LLM" => return (StatusCode::SERVICE_UNAVAILABLE, "llm offline").into_response(),
                "Некорректный ответ" => return ([("content-type", "text/event-stream")], "data: invalid JSON\n\n").into_response(),
                "Где хранятся чаты и какая ветка?" => {},
                other => panic!("unexpected question: {other}"),
            }
            assert!(messages.iter().any(|message| message["content"].as_str().is_some_and(|text|
                text.contains("Ниже найдены фрагменты") && text.contains("Хранилище проекта — chats.sqlite3"))));
            if let Some(result) = messages.iter().find(|message| message["role"] == "tool") {
                assert!(result["content"].as_str().unwrap().contains("feature/help-test"));
                return sse(json!({"content":"Чаты: chats.sqlite3 [1]. MCP вернул ветку feature/help-test."}), "stop");
            }
            let tool = &body["tools"][0]["function"]["name"];
            sse(json!({"tool_calls":[{"index":0,"id":"branch_mock","type":"function","function":{
                "name":tool,"arguments":"{\"text\":\"feature/help-test\"}"
            }}]}), "tool_calls")
        }
    }));
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let observed = requests.clone();
    tokio::task::spawn_blocking(move || {
        let state = State(std::env::temp_dir().join(format!("agi-help-cli-{}", uuid::Uuid::new_v4())));
        run(&state, &["llm", "add", "offline", "--url", &url, "--model", "offline-model", "--context-tokens", "16384", "--tools"], "");
        run(&state, &["llm", "default", "offline"], "");
        let embeddings_url = format!("{url}/embeddings");
        run(&state, &["rag", "embeddings", "set", "--url", &embeddings_url, "--model", "offline-embeddings"], "");
        let before = observed.lock().unwrap().len();
        let output = run(&state, &[], "/help\n/help Без RAG\n/exit\n");
        assert!(output.contains("Доступные команды:"));
        assert!(output.contains("Документация не загружена"));
        assert_eq!(observed.lock().unwrap().len(), before + 1);
        let before = observed.lock().unwrap().len();
        let output = run(&state, &[], "/rag on\n/help Без документов\n/exit\n");
        assert!(output.contains("Индекс пуст"));
        assert_eq!(observed.lock().unwrap().len(), before);
        let document = state.0.join("structure.md");
        std::fs::write(&document, "# Хранение\nХранилище проекта — chats.sqlite3.").unwrap();
        run(&state, &["rag", "add", document.to_str().unwrap()], "");
        let output = run(&state, &[], "/mcp\n3\n5\n/rag on\n/помощь Где хранятся чаты и какая ветка?\n/help Ошибка эмбеддингов\n/help Ошибка LLM\n/help Некорректный ответ\n/exit\n");
        assert!(output.contains("feature/help-test"), "{output}");
        assert!(output.contains("chats.sqlite3"));
        assert!(output.contains("сервис эмбеддингов вернул HTTP 503"), "{output}");
        assert!(output.contains("Ошибка запроса"));
        let db = rusqlite::Connection::open(state.0.join("agi/chats.sqlite3")).unwrap();
        let questions = db.prepare("SELECT content FROM messages WHERE role='user'").unwrap()
            .query_map([], |row| row.get::<_, String>(0)).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(questions.len(), 2);
        assert!(questions.contains(&"/help Без RAG".into()));
        assert!(questions.contains(&"/помощь Где хранятся чаты и какая ветка?".into()));
        let settings: Vec<String> = db.prepare("SELECT settings_json FROM chats").unwrap()
            .query_map([], |row| row.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        assert!(settings.iter().map(|raw| serde_json::from_str::<Value>(raw).unwrap()).all(|settings|
            settings["rag_options"]["strict"] == false));
    }).await.unwrap();
    let captured = requests.lock().unwrap();
    assert!(
        captured
            .iter()
            .any(|body| body["input"][0] == "Где хранятся чаты и какая ветка?")
    );
    assert!(
        !captured
            .iter()
            .filter_map(|body| body["input"].as_array())
            .flatten()
            .any(|text| text.as_str().unwrap().starts_with("/help")
                || text.as_str().unwrap().starts_with("/помощь"))
    );
    server.abort();
}
