use super::*;
use crate::test_http::{MockServer, delegate, text_response, tool_response};

#[tokio::test]
async fn generates_system_prompt_with_one_non_streaming_request_without_delegation() {
    let server = MockServer::new(|_| {
        (200, json!({"choices":[{"message":{"content":"Проверяет текст\n и предлагает правки."},"finish_reason":"stop"}]}).to_string())
    });
    let result = runner(&server)
        .generate_system_prompt(SystemPromptRequest {
            name: "Редактор".into(),
            handle: "editor".into(),
            description: "Проверяет текст".into(),
            instructions: "Исправляй ошибки".into(),
            request: "Описание помощника по текстам".into(),
            model: "gpt-oss-20b".into(),
        })
        .await
        .expect("description");
    assert_eq!(result, "Проверяет текст\n и предлагает правки.");
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let req = &requests[0];
    assert_eq!(req["stream"], false);
    assert_eq!(req["model"], "gpt-oss-20b");
    assert_eq!(req["max_tokens"], 4096);
    for field in [
        "tools",
        "tool_choice",
        "response_format",
        "stop",
        "stream_options",
    ] {
        assert!(req.get(field).is_none(), "{field}");
    }
    assert_eq!(req["messages"].as_array().expect("messages").len(), 2);
    let context: Value =
        serde_json::from_str(req["messages"][1]["content"].as_str().expect("content"))
            .expect("context");
    assert_eq!(context["system_prompt"], "Исправляй ошибки");
    assert_eq!(context["description"], "Проверяет текст");
    assert_eq!(context["request"], "Описание помощника по текстам");
}

#[tokio::test]
async fn rejects_invalid_system_prompt_responses_without_retrying() {
    for (status, body) in [
        (503, r#"{"detail":"Недоступно"}"#.to_owned()),
        (200,"not json".into()),
        (200,json!({"choices":[]}).to_string()),
        (200,json!({"choices":[{"message":{"content":" "},"finish_reason":"stop"}]}).to_string()),
        (200,json!({"choices":[{"message":{"content":"я".repeat(8001)},"finish_reason":"stop"}]}).to_string()),
        (200,json!({"choices":[{"message":{"content":"Короткий обрезанный ответ"},"finish_reason":"length"}]}).to_string()),
    ] {
        let server=MockServer::new(move |_| (status,body.clone()));
        assert!(runner(&server).generate_system_prompt(SystemPromptRequest {name:"Редактор".into(),handle:"editor".into(),description:"Проверяет текст".into(),instructions:String::new(),request:"Помоги".into(),model:"gpt-oss-20b".into()}).await.is_err());
        assert_eq!(server.requests().len(),1);
    }
}

#[tokio::test]
async fn cancellation_closes_all_child_connections_and_stops_main_synthesis() {
    use std::io::Read;
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let url = format!("http://{}", listener.local_addr().expect("address"));
    let started = Arc::new(AtomicUsize::new(0));
    let observed = started.clone();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut workers = Vec::new();
        while workers.len() < 2 && Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let observed = observed.clone();
                    workers.push(std::thread::spawn(move || {
                        stream.set_nonblocking(false).expect("blocking");
                        stream
                            .set_read_timeout(Some(Duration::from_secs(5)))
                            .expect("timeout");
                        crate::test_http::read_request(&mut stream);
                        observed.fetch_add(1, Ordering::SeqCst);
                        let read = stream.read(&mut [0; 1]);
                        assert!(
                            matches!(read, Ok(0))
                                || read.is_err_and(
                                    |error| error.kind() == io::ErrorKind::ConnectionReset
                                ),
                            "cancelled child should disconnect"
                        );
                    }));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1))
                }
                Err(error) => panic!("accept: {error}"),
            }
        }
        assert_eq!(workers.len(), 2);
        for worker in workers {
            worker.join().expect("child disconnected");
        }
        assert!(listener.accept().is_err(), "main synthesis must not start");
    });
    let runner = Agent::new(NeuralDeepClient::new("test-key".into(), url).expect("client"));
    let task = tokio::spawn(async move {
        runner
            .respond_streaming(
                request("@aa @bb задача", vec![definition("aa"), definition("bb")]),
                |_| Ok(()),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while started.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("children started");
    task.abort();
    assert!(task.await.expect_err("aborted").is_cancelled());
    tokio::task::spawn_blocking(move || server.join().expect("server"))
        .await
        .expect("join");
}

#[tokio::test]
async fn final_service_error_is_propagated_after_successful_child() {
    let server = MockServer::new(|req| {
        if req.get("tools").is_some() {
            (503, r#"{"detail":"main unavailable"}"#.into())
        } else {
            text_response("Дочерний результат")
        }
    });
    let result = runner(&server)
        .respond_streaming(
            request("@editor задача", vec![definition("editor")]),
            |_| Ok(()),
        )
        .await;
    assert!(
        result
            .expect_err("main failure")
            .to_string()
            .contains("main unavailable")
    );
    assert_eq!(server.requests().len(), 2);
}
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

fn definition(handle: &str) -> AgentDefinition {
    let mut agent = AgentDefinition::draft();
    agent.handle = handle.into();
    agent.name = format!("Агент {handle}");
    agent.description = format!("Описание {handle}");
    agent
        .settings
        .set_system_prompt(format!("Собственная инструкция {handle}"))
        .expect("prompt");
    agent
}

fn request(question: &str, agents: Vec<AgentDefinition>) -> AgentRequest {
    AgentRequest::new(&Chat::new(), question.into(), agents)
}

fn runner(server: &MockServer) -> Agent {
    Agent::new(NeuralDeepClient::new("test-key".into(), server.url.clone()).expect("client"))
}

#[tokio::test]
async fn simple_agent_returns_streamed_answer_without_catalog_or_tools() {
    let server = MockServer::new(|_| text_response("Привет"));
    let mut events = Vec::new();
    let answer = runner(&server)
        .respond_streaming(request("Привет", vec![]), |event| {
            events.push(event);
            Ok(())
        })
        .await
        .expect("answer");
    assert_eq!(answer.content, "Привет");
    assert_eq!(answer.calls.len(), 1);
    assert!(matches!(&events[1], AgentEvent::MainDelta(text) if text == "Привет"));
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].get("tools").is_none());
    assert_eq!(
        requests[0]["messages"].as_array().expect("messages").len(),
        1
    );
}

#[tokio::test]
async fn automatically_delegates_and_returns_result_with_matching_tool_id() {
    let server = MockServer::new(|req| {
        if req.get("tools").is_none() {
            return text_response("Проверено дочерним");
        }
        if req["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .any(|msg| msg["role"] == "tool")
        {
            return text_response("Окончательный итог");
        }
        tool_response(vec![delegate(0, "call_editor", "editor")])
    });
    let mut req = request("Проверь решение", vec![definition("editor")]);
    req.settings
        .set_system_prompt("Пиши по-русски".into())
        .expect("prompt");
    let chat_id = req.chat_id.to_string();
    let mut events = Vec::new();
    let answer = runner(&server)
        .respond_streaming(req, |event| {
            events.push(event);
            Ok(())
        })
        .await
        .expect("answer");
    assert_eq!(answer.content, "Окончательный итог");
    assert_eq!(answer.calls.len(), 3);
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    let system = requests[0]["messages"][0]["content"]
        .as_str()
        .expect("system");
    assert!(system.contains("@editor"));
    assert!(system.contains("Описание editor"));
    assert!(system.contains("delegate_task(handle, task)"));
    assert!(!system.contains("Собственная инструкция"));
    assert_eq!(
        requests[0]["tools"][0]["function"]["parameters"]["properties"]["handle"]["enum"],
        json!(["editor"])
    );
    assert_eq!(requests[0]["user"], chat_id);
    assert_eq!(requests[2]["user"], chat_id);
    assert_ne!(requests[1]["user"], chat_id);
    let child_prompt = requests[1]["messages"][0]["content"]
        .as_str()
        .expect("prompt");
    assert!(child_prompt.contains("Пиши по-русски"));
    assert!(child_prompt.contains("Собственная инструкция editor"));
    let messages = requests[2]["messages"].as_array().expect("messages");
    let tool = messages
        .iter()
        .find(|msg| msg["role"] == "tool")
        .expect("tool result");
    assert_eq!(tool["tool_call_id"], "call_editor");
    assert!(
        tool["content"]
            .as_str()
            .expect("content")
            .contains("Проверено дочерним")
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::ChildCompleted {id, ..} if id=="call_editor"))
    );
}

#[tokio::test]
async fn explicit_agents_run_concurrently_with_own_settings_and_deduplicated_handles() {
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let server_active = active.clone();
    let server_peak = peak.clone();
    let server = MockServer::new(move |req| {
        if req.get("tools").is_some() {
            return text_response("Итог");
        }
        let current = server_active.fetch_add(1, Ordering::SeqCst) + 1;
        server_peak.fetch_max(current, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(80));
        server_active.fetch_sub(1, Ordering::SeqCst);
        if req.get("response_format").is_some() {
            text_response(r#"{"short_answer":"Да","details":["Факт"],"conclusion":"Итог"}"#)
        } else {
            text_response("Результат")
        }
    });
    let mut editor = definition("editor");
    editor.settings.select_model(0);
    editor.settings.set_temperature("0.8").expect("temperature");
    editor.settings.set_max_tokens("900").expect("tokens");
    editor.settings.set_response_format(true);
    editor
        .settings
        .set_stop_sequence("<END>".into())
        .expect("stop");
    let answer = runner(&server)
        .respond_streaming(
            request(
                "@EDITOR, @reviewer @editor Проверь",
                vec![editor, definition("reviewer")],
            ),
            |_| Ok(()),
        )
        .await
        .expect("answer");
    assert_eq!(answer.calls.len(), 3);
    assert_eq!(peak.load(Ordering::SeqCst), 2);
    let requests = server.requests();
    let child = requests
        .iter()
        .find(|req| req["model"] == "gpt-oss-20b")
        .expect("own model");
    assert_eq!(child["max_tokens"], 900);
    assert!((child["temperature"].as_f64().expect("temperature") - 0.8).abs() < 0.001);
    assert_eq!(child["stop"], "<END>");
    assert_eq!(child["response_format"]["type"], "json_schema");
    let sessions: HashSet<_> = requests
        .iter()
        .map(|req| req["user"].as_str().expect("session"))
        .collect();
    assert_eq!(sessions.len(), 3);
}

#[tokio::test]
async fn stops_after_two_waves_and_forces_final_without_tools() {
    let main_count = Arc::new(AtomicUsize::new(0));
    let count = main_count.clone();
    let server = MockServer::new(move |req| {
        if req.get("tools").is_some() {
            let wave = count.fetch_add(1, Ordering::SeqCst);
            tool_response(vec![delegate(0, &format!("wave_{wave}"), "editor")])
        } else {
            text_response("Итог")
        }
    });
    let answer = runner(&server)
        .respond_streaming(request("Проверь", vec![definition("editor")]), |_| Ok(()))
        .await
        .expect("answer");
    assert_eq!(main_count.load(Ordering::SeqCst), 2);
    assert_eq!(answer.calls.len(), 5);
    let requests = server.requests();
    let final_request = requests.last().expect("final");
    assert!(final_request.get("tools").is_none());
    assert!(
        final_request["messages"]
            .as_array()
            .expect("messages")
            .last()
            .expect("final instruction")["content"]
            .as_str()
            .expect("instruction")
            .contains("Новые делегирования запрещены")
    );
}

#[tokio::test]
async fn child_failure_is_returned_to_main_and_does_not_cancel_other_children() {
    let server = MockServer::new(|req| {
        if req["model"] == "gpt-oss-20b" {
            return (429, r#"{"error":{"message":"rate limit"}}"#.into());
        }
        text_response("Доступный результат")
    });
    let mut failing = definition("failing");
    failing.settings.select_model(0);
    let mut events = Vec::new();
    let answer = runner(&server)
        .respond_streaming(
            request(
                "@failing @editor задача",
                vec![failing, definition("editor")],
            ),
            |event| {
                events.push(event);
                Ok(())
            },
        )
        .await
        .expect("partial success");
    assert_eq!(answer.calls.len(), 3);
    assert!(answer.calls.iter().any(|call| call.usage.is_none()));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ChildFailed { .. }))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ChildCompleted { .. }))
    );
    let requests = server.requests();
    assert!(
        requests.last().expect("main")["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .any(|message| message["role"] == "tool"
                && message["content"]
                    .as_str()
                    .is_some_and(|text| text.contains("rate limit")))
    );
}

#[tokio::test]
async fn rejects_invalid_and_excess_tool_calls_without_executing_them() {
    let server = MockServer::new(|req| {
        if req.get("tools").is_none() {
            return text_response("Дочерний результат");
        }
        if req["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .any(|msg| msg["role"] == "tool")
        {
            return text_response("Итог");
        }
        let mut invalid = delegate(0, "bad", "editor");
        invalid["function"]["arguments"] = json!("{bad");
        let mut unknown = delegate(1, "unknown", "editor");
        unknown["function"]["name"] = json!("shell");
        tool_response(vec![
            invalid,
            unknown,
            delegate(2, "valid", "editor"),
            delegate(3, "excess", "editor"),
        ])
    });
    let mut events = Vec::new();
    let answer = runner(&server)
        .respond_streaming(
            request("Проверь", vec![definition("editor")]),
            |event| {
                events.push(event);
                Ok(())
            },
        )
        .await
        .expect("answer");
    assert_eq!(answer.calls.len(), 3);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::ChildFailed { .. }))
            .count(),
        3
    );
    let requests = server.requests();
    let tool_results = requests.last().expect("main")["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|message| message["role"] == "tool")
        .collect::<Vec<_>>();
    assert_eq!(tool_results.len(), 4);
    assert_eq!(
        tool_results
            .iter()
            .map(|message| message["tool_call_id"].as_str().expect("id"))
            .collect::<Vec<_>>(),
        ["bad", "unknown", "valid", "excess"]
    );
}

#[tokio::test]
async fn structured_main_uses_schema_only_for_final_generation() {
    let server = MockServer::new(|req| {
        if req.get("tools").is_some() {
            assert!(req.get("response_format").is_none());
            text_response("Черновик")
        } else {
            assert_eq!(req["response_format"]["type"], "json_schema");
            text_response(r#"{"short_answer":"Ответ","details":["Факт"],"conclusion":"Итог"}"#)
        }
    });
    let mut req = request("Вопрос", vec![definition("editor")]);
    req.settings.set_response_format(true);
    let mut events = Vec::new();
    let answer = runner(&server)
        .respond_streaming(req, |event| {
            events.push(event);
            Ok(())
        })
        .await
        .expect("answer");
    assert!(answer.content.contains("short_answer"));
    assert_eq!(answer.calls.len(), 2);
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::MainDelta(_))));
}

#[tokio::test]
async fn invalid_mentions_and_unsupported_main_model_fail_before_network() {
    let server = MockServer::new(|_| panic!("No HTTP request expected"));
    let agent = runner(&server);
    let defs = vec![
        definition("aa"),
        definition("bb"),
        definition("cc"),
        definition("dd"),
    ];
    for question in ["@unknown задача", "@aa @bb @cc @dd задача", "   "] {
        assert!(
            agent
                .respond_streaming(request(question, defs.clone()), |_| Ok(()))
                .await
                .is_err()
        );
    }
    let mut req = request("Вопрос", defs);
    req.settings.select_model(0);
    let error = agent
        .respond_streaming(req, |_| Ok(()))
        .await
        .expect_err("tools model required");
    assert!(error.to_string().contains("/settings"));
    assert!(server.requests().is_empty());
}
