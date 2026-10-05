use crate::agent::{Agent, AgentEvent, AgentRequest};
use crate::agent_catalog::AgentDefinition;
use crate::api::NeuralDeepClient;
use crate::chat::{Chat, ChatStore};
use crate::llm::{LlmProfile, Provider, sync_chat};
use crate::metrics::ResponseMetrics;
use crate::pricing::PriceCatalog;
use crate::test_http::{MockServer, text_response};

fn local(url: &str) -> LlmProfile {
    LlmProfile {
        id: "local".into(),
        provider: Provider::Local,
        base_url: url.into(),
        model: "local-model".into(),
        context_tokens: 16384,
        tools: true,
        json_schema: true,
    }
}

#[tokio::test]
async fn one_request_routes_main_and_children_to_three_endpoints() {
    let main = MockServer::new_local(|body| {
        assert_eq!(body["model"], "main-model");
        assert!(body.get("chat_template_kwargs").is_none());
        let messages = body["messages"].as_array().unwrap();
        assert!(messages.iter().any(|message| {
            message["role"] == "tool"
                && message["content"]
                    .as_str()
                    .unwrap()
                    .contains("Локальный результат")
        }));
        assert!(messages.iter().any(|message| {
            message["role"] == "tool"
                && message["content"]
                    .as_str()
                    .unwrap()
                    .contains("Облачный результат")
        }));
        text_response("Общий итог")
    });
    let child_local = MockServer::new_local(|body| {
        assert_eq!(body["model"], "local-model");
        assert!(body.get("chat_template_kwargs").is_none());
        text_response("Локальный результат")
    });
    let child_cloud = MockServer::new(|body| {
        assert_eq!(body["model"], crate::config::DEFAULT_MODEL);
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
        text_response("Облачный результат")
    });
    let mut chat = Chat::new();
    let mut main_profile = local(&main.url);
    main_profile.model = "main-model".into();
    main_profile.id = "main".into();
    chat.settings_mut().apply_profile(main_profile);
    let mut first = AgentDefinition::draft();
    first.handle = "local".into();
    first.settings.apply_profile(local(&child_local.url));
    let mut second = AgentDefinition::draft();
    second.handle = "cloud".into();
    let mut cloud_profile = LlmProfile::neuraldeep(crate::config::DEFAULT_MODEL);
    cloud_profile.base_url = child_cloud.url.clone();
    second.settings.apply_profile(cloud_profile);
    let agent =
        Agent::new(NeuralDeepClient::new("test-key".into(), child_cloud.url.clone()).unwrap());
    let request = AgentRequest::new(
        &chat,
        "@local @cloud Решите задачу".into(),
        vec![first, second],
    );
    let mut completed = 0;
    let answer = agent
        .respond_streaming(request, |event| {
            if matches!(event, AgentEvent::ChildCompleted { .. }) {
                completed += 1;
            }
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(answer.content, "Общий итог");
    assert_eq!(completed, 2);
    assert_eq!(main.requests().len(), 1);
    assert_eq!(child_local.requests().len(), 1);
    assert_eq!(child_cloud.requests().len(), 1);
    assert!(
        answer
            .calls
            .iter()
            .any(|call| call.provider == Some(Provider::Local)
                && call.profile_id.as_deref() == Some("local"))
    );
    assert!(
        answer
            .calls
            .iter()
            .any(|call| call.provider == Some(Provider::NeuralDeep))
    );
}

#[test]
fn restore_and_checkpoint_never_change_application_llm() {
    let path = std::env::temp_dir().join(format!("agi-llm-history-{}", uuid::Uuid::new_v4()));
    let store = ChatStore::for_tests(path.clone()).unwrap();
    store
        .llms()
        .add(local("http://localhost:11434/v1"))
        .unwrap();
    let mut chat = Chat::new();
    chat.settings_mut()
        .select_context_strategy(crate::context::ContextStrategyKind::Branching);
    chat.settings_mut().select_model(2);
    chat.record_exchange("Вопрос".into(), "Ответ".into());
    store.save(&mut chat).unwrap();
    store.create_checkpoint(&chat, "before").unwrap();
    store.llms().set_default("local").unwrap();
    let mut restored = store.load(chat.id()).unwrap();
    sync_chat(&store, &mut restored).unwrap();
    assert_eq!(restored.settings().model(), "local-model");
    let mut branch = store
        .create_branch(chat.branch_group_id(), "before", "after")
        .unwrap();
    sync_chat(&store, &mut branch).unwrap();
    assert_eq!(branch.settings().profile_id(), Some("local"));
    assert_eq!(branch.messages(), chat.messages());
    let snapshot = serde_json::to_value(&branch).unwrap();
    assert!(snapshot["settings"].get("model").is_none());
    assert!(snapshot["settings"].get("llm_profile_id").is_none());
    drop(store);
    let reopened = ChatStore::for_tests(path.clone()).unwrap();
    assert_eq!(reopened.llms().active().unwrap().id, "local");
    drop(reopened);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn agent_keeps_its_profile_when_global_default_changes() {
    let path = std::env::temp_dir().join(format!("agi-llm-agent-{}", uuid::Uuid::new_v4()));
    let store = ChatStore::for_tests(path.clone()).unwrap();
    store
        .llms()
        .add(local("http://localhost:11434/v1"))
        .unwrap();
    let mut agent = AgentDefinition::draft();
    agent.name = "Локальный".into();
    agent.handle = "local".into();
    agent.description = "Проверяет код".into();
    agent
        .settings
        .set_system_prompt("Проверяй код".into())
        .unwrap();
    agent
        .settings
        .apply_profile(store.llms().get("local").unwrap());
    store.agents().save(&agent, true).unwrap();
    store
        .llms()
        .set_default(&LlmProfile::neuraldeep("gpt-oss-20b").id)
        .unwrap();
    assert_eq!(
        store
            .agents()
            .get(agent.id)
            .unwrap()
            .settings
            .profile()
            .unwrap()
            .id,
        "local"
    );
    assert!(store.llms().remove("local").is_err());
    drop(store);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn local_metrics_use_profile_context_and_zero_api_cost() {
    let server = MockServer::new_local(|_| text_response("Ответ"));
    let mut chat = Chat::new();
    chat.settings_mut().apply_profile(local(&server.url));
    let agent = Agent::new(
        NeuralDeepClient::new(String::new(), crate::config::DEFAULT_BASE_URL.into()).unwrap(),
    );
    let answer = agent
        .respond_streaming(
            AgentRequest::new(&chat, "Вопрос".into(), vec![]),
            |_| Ok(()),
        )
        .await
        .unwrap();
    let metrics = ResponseMetrics::from_calls(
        "local-model",
        answer.elapsed_ms,
        answer.calls,
        &PriceCatalog::default(),
    );
    assert_eq!(metrics.estimated_cost_microrubles, Some(0));
    assert_eq!(metrics.calls[0].context.unwrap().limit, 16384);
    assert_eq!(metrics.profile_id.as_deref(), Some("local"));
    assert!(crate::metrics::metric_lines(Some(&metrics))[1].contains("16 384"));
}

#[tokio::test]
async fn local_capability_errors_happen_before_network() {
    let server = MockServer::new_local(|_| panic!("unsupported request must not be sent"));
    let mut profile = local(&server.url);
    profile.json_schema = false;
    let mut chat = Chat::new();
    chat.settings_mut().apply_profile(profile);
    chat.settings_mut().set_response_format(true);
    let agent = Agent::new(
        NeuralDeepClient::new(String::new(), crate::config::DEFAULT_BASE_URL.into()).unwrap(),
    );
    let result = agent
        .respond_streaming(
            AgentRequest::new(&chat, "Вопрос".into(), vec![]),
            |_| Ok(()),
        )
        .await;
    assert!(result.unwrap_err().to_string().contains("JSON Schema"));
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn granite_native_tool_wrapper_is_normalized() {
    let server = MockServer::new_local(|_| {
        let args = serde_json::json!({"function":"delegate_task","arguments":{"handle":"aa","task":"one"}}).to_string();
        (
            200,
            format!(
                "data: {}\n\ndata: [DONE]\n\n",
                serde_json::json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"delegate_task","arguments":args}}]},"finish_reason":"tool_calls"}]})
            ),
        )
    });
    let mut profile = local(&server.url);
    profile.model = "agi-granite3.3:8b".into();
    let mut settings = crate::settings::Settings::default();
    settings.apply_profile(profile);
    let client =
        NeuralDeepClient::new("test-key".into(), crate::config::DEFAULT_BASE_URL.into()).unwrap();
    let turn = client
        .complete_streaming(&[], uuid::Uuid::new_v4(), &settings, None, |_| Ok(()))
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&turn.tool_calls[0].function.arguments).unwrap(),
        serde_json::json!({"handle":"aa","task":"one"})
    );
}

#[tokio::test]
async fn local_errors_are_reported_without_cloud_fallback() {
    for (status, body) in [
        (503, "{\"error\":\"local unavailable\"}"),
        (200, "invalid json"),
    ] {
        let server = MockServer::new_local(move |_| (status, body.into()));
        let client =
            NeuralDeepClient::new("test-key".into(), crate::config::DEFAULT_BASE_URL.into())
                .unwrap()
                .with_profile(&local(&server.url));
        assert!(client.complete_text(&[], "local-model").await.is_err());
        assert_eq!(server.requests().len(), 1);
    }
    let client = NeuralDeepClient::new(String::new(), crate::config::DEFAULT_BASE_URL.into())
        .unwrap()
        .with_profile(&local("http://127.0.0.1:1"));
    assert!(client.complete_text(&[], "local-model").await.is_err());
}

#[tokio::test]
async fn cancelling_mixed_provider_request_prevents_synthesis() {
    let main = MockServer::new_local(|_| panic!("cancelled request must not synthesize"));
    let first = MockServer::new_local(|_| {
        std::thread::sleep(std::time::Duration::from_millis(150));
        text_response("local")
    });
    let second = MockServer::new(|_| {
        std::thread::sleep(std::time::Duration::from_millis(150));
        text_response("cloud")
    });
    let mut chat = Chat::new();
    chat.settings_mut().apply_profile(local(&main.url));
    let mut a = AgentDefinition::draft();
    a.handle = "aa".into();
    a.settings.apply_profile(local(&first.url));
    let mut b = AgentDefinition::draft();
    b.handle = "bb".into();
    let mut profile = LlmProfile::neuraldeep(crate::config::DEFAULT_MODEL);
    profile.base_url = second.url.clone();
    b.settings.apply_profile(profile);
    let agent = Agent::new(NeuralDeepClient::new("test-key".into(), second.url.clone()).unwrap());
    let request = AgentRequest::new(&chat, "@aa @bb задача".into(), vec![a, b]);
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(60),
        agent.respond_streaming(request, |_| Ok(())),
    )
    .await;
    assert!(result.is_err());
    assert!(main.requests().is_empty());
    assert_eq!(first.requests().len(), 1);
    assert_eq!(second.requests().len(), 1);
}
