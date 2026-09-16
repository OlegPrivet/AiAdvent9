use super::*;

fn update(operation: TaskOperation, steps: &[&str]) -> TaskUpdate {
    TaskUpdate {
        operation,
        steps: steps.iter().map(|s| (*s).into()).collect(),
        question: "Какой формат нужен?".into(),
        reason: "Изменились требования".into(),
    }
}

fn execution() -> TaskState {
    let mut task = TaskState::new("Подготовить описание проекта").unwrap();
    task = task
        .apply(
            update(TaskOperation::Plan, &["Структура", "Текст"]),
            "План работы",
        )
        .unwrap();
    task.approve().unwrap();
    task
}

#[test]
fn requires_approval_and_completes_only_after_validation() {
    let task = TaskState::new("Описание проекта").unwrap();
    assert!(
        task.apply(update(TaskOperation::StepCompleted, &[]), "Пропущен план")
            .is_err()
    );
    let mut task = task
        .apply(update(TaskOperation::Plan, &["Текст"]), "План")
        .unwrap();
    assert_eq!(task.expected_action(), ExpectedAction::Approve);
    assert!(
        task.apply(update(TaskOperation::StepCompleted, &[]), "Самоутверждение")
            .is_err()
    );
    task.approve().unwrap();
    assert!(
        task.apply(
            update(TaskOperation::ValidationPassed, &[]),
            "Пропущено выполнение"
        )
        .is_err()
    );
    task = task
        .apply(update(TaskOperation::StepCompleted, &[]), "Готовый текст")
        .unwrap();
    assert_eq!(task.stage, TaskStage::Validation);
    task = task
        .apply(
            update(TaskOperation::ValidationPassed, &[]),
            "Текст соответствует плану; код не запускался",
        )
        .unwrap();
    assert_eq!(task.stage, TaskStage::Done);
    assert!(
        task.apply(update(TaskOperation::Continue, &[]), "Ещё")
            .is_err()
    );
    task.pause("Пауза");
    assert!(!task.paused);
}

#[test]
fn pause_preserves_progress_at_every_stage() {
    let execution = execution();
    let validation = execution
        .apply(
            update(TaskOperation::StepCompleted, &[]),
            "Структура готова",
        )
        .unwrap()
        .apply(update(TaskOperation::StepCompleted, &[]), "Текст готов")
        .unwrap();
    for mut task in [
        TaskState::new("Планирование").unwrap(),
        execution,
        validation,
    ] {
        let original = task.clone();
        task.pause("Пользователь");
        assert!(!task.runnable());
        assert!(
            task.apply(update(TaskOperation::Continue, &[]), "Поздний ответ")
                .is_err()
        );
        task.paused = false;
        task.pause_reason = None;
        assert_eq!(task, original);
    }
}

#[test]
fn validation_repairs_and_replanning_keep_completed_results() {
    let mut task = execution()
        .apply(
            update(TaskOperation::StepCompleted, &[]),
            "Структура готова",
        )
        .unwrap();
    let replanned = task
        .apply(update(TaskOperation::Replan, &[]), "Нужен другой план")
        .unwrap();
    assert_eq!(replanned.stage, TaskStage::Planning);
    assert!(!replanned.plan_approved);
    assert!(
        replanned
            .context
            .iter()
            .any(|s| s.contains("Структура готова"))
    );
    task = task
        .apply(update(TaskOperation::StepCompleted, &[]), "Текст")
        .unwrap();
    task = task
        .apply(
            update(TaskOperation::ValidationFailed, &["Исправить заголовок"]),
            "Заголовок не соответствует цели",
        )
        .unwrap();
    assert_eq!(task.stage, TaskStage::Execution);
    assert_eq!(task.current_step(), Some(2));
    assert_eq!(task.steps[0].result.as_deref(), Some("Структура готова"));
}

#[test]
fn clarification_waits_and_keeps_answer_for_the_next_request() {
    let mut task = execution()
        .apply(update(TaskOperation::Clarify, &[]), "Какой формат нужен?")
        .unwrap();
    assert_eq!(task.expected_action(), ExpectedAction::Clarify);
    assert!(!task.runnable());
    task.pending_input = Some("Markdown".into());
    assert!(task.runnable());
    task = task
        .apply(
            update(TaskOperation::StepCompleted, &[]),
            "Структура в Markdown",
        )
        .unwrap();
    assert!(task.question.is_none());
    assert!(task.pending_input.is_none());
    assert!(task.context.iter().any(|s| s.contains("Markdown")));
}

#[test]
fn automatic_run_stops_after_three_iterations_without_progress() {
    let mut task = execution();
    let mut budget = RunBudget::default();
    for index in 0..3 {
        let next = task
            .apply(
                update(TaskOperation::Continue, &[]),
                "Работа ещё не закончена",
            )
            .unwrap();
        assert_eq!(budget.observe(&task, &next).is_some(), index == 2);
        task = next;
    }
}

struct TestDirectory(std::path::PathBuf);

impl TestDirectory {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("agi-task-test-{}", uuid::Uuid::new_v4())))
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn resumes_second_step_after_restart_without_replaying_completed_work() {
    use crate::agent::{Agent, AgentRequest};
    let directory = TestDirectory::new();
    let server = crate::test_http::MockServer::new(crate::test_http::task_response);
    let agent = Agent::new(
        crate::api::NeuralDeepClient::new("test-key".into(), server.url.clone()).unwrap(),
    );
    let store = ChatStore::for_tests(directory.0.clone()).unwrap();
    let mut chat = Chat::new();
    chat.settings_mut().set_context_window("2").unwrap();
    let id = chat.id();
    command(
        &store,
        &mut chat,
        Some("start Подготовить описание проекта"),
    )
    .unwrap();
    assert!(chat.is_persisted());
    assert!(chat.messages().is_empty());
    let mut budget = RunBudget::default();
    let prices = PriceCatalog::default();
    for iteration in 0..2 {
        let question = next_question(&chat).unwrap();
        let answer = agent
            .respond_streaming(AgentRequest::new(&chat, question.clone(), vec![]), |_| {
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(answer.calls.len(), 2);
        commit_answer(&store, &mut chat, question, answer, &prices, &mut budget).unwrap();
        if iteration == 0 {
            assert!(
                next_question(&chat).is_none(),
                "must stop for plan approval"
            );
            command(&store, &mut chat, Some("approve")).unwrap();
        }
    }
    assert_eq!(chat.task().unwrap().current_step(), Some(1));
    assert_eq!(chat.messages().len(), 2, "older history was pruned");
    prepare_input(&store, &mut chat, "Продолжай в том же стиле").unwrap();
    pause(&store, &mut chat, "Пауза во втором шаге").unwrap();
    drop(store);
    let store = ChatStore::for_tests(directory.0.clone()).unwrap();
    let mut chat = store.load(id).unwrap();
    assert!(next_question(&chat).is_none());
    assert_eq!(
        chat.task().unwrap().pending_input.as_deref(),
        Some("Продолжай в том же стиле")
    );
    command(&store, &mut chat, Some("resume")).unwrap();
    while let Some(question) = next_question(&chat) {
        let answer = agent
            .respond_streaming(AgentRequest::new(&chat, question.clone(), vec![]), |_| {
                Ok(())
            })
            .await
            .unwrap();
        commit_answer(&store, &mut chat, question, answer, &prices, &mut budget).unwrap();
    }
    assert_eq!(chat.task().unwrap().stage, TaskStage::Done);
    let restored = store.load(id).unwrap();
    assert_eq!(restored.task(), chat.task());
    let requests = server.requests();
    assert_eq!(
        requests.len(),
        8,
        "plan, step one, step two, validation; each has metadata call"
    );
    let resumed_request = requests[4]["messages"].to_string();
    assert!(resumed_request.contains("Структура: введение"));
    assert!(resumed_request.contains("Подготовить описание проекта"));
    assert_eq!(restored.cumulative_api_usage().unwrap().total_tokens, 120);
}

#[tokio::test]
async fn invalid_updates_and_service_errors_do_not_change_task() {
    use crate::agent::{Agent, AgentRequest};
    for (status, metadata) in [
        (503, "{}".into()),
        (200, "not json".into()),
        (
            200,
            json!({"operation":"validation_passed","steps":[],"question":"","reason":""})
                .to_string(),
        ),
        (
            200,
            json!({"operation":"plan","steps":[],"question":"","reason":""}).to_string(),
        ),
    ] {
        let server = crate::test_http::MockServer::new(move |request| {
            if request["stream"] == true {
                crate::test_http::text_response("Предложение плана")
            } else {
                (
                    status,
                    json!({"choices":[{"message":{"content":metadata},"finish_reason":"stop"}]})
                        .to_string(),
                )
            }
        });
        let agent = Agent::new(
            crate::api::NeuralDeepClient::new("test-key".into(), server.url.clone()).unwrap(),
        );
        let mut chat = Chat::new();
        chat.set_task(TaskState::new("Задача").unwrap());
        let before = chat.task().cloned();
        let result = agent
            .respond_streaming(
                AgentRequest::new(&chat, "Составь план".into(), vec![]),
                |_| Ok(()),
            )
            .await;
        assert!(result.is_err());
        assert_eq!(chat.task(), before.as_ref());
        assert!(chat.messages().is_empty());
    }
}

#[test]
fn bounded_run_stops_even_if_replanning_keeps_making_progress() {
    let mut budget = RunBudget::default();
    let execution = execution();
    let planning = execution
        .apply(update(TaskOperation::Replan, &[]), "Новый план")
        .unwrap();
    for index in 0..50 {
        let (before, after) = if index % 2 == 0 {
            (&execution, &planning)
        } else {
            (&planning, &execution)
        };
        assert_eq!(budget.observe(before, after).is_some(), index == 49);
    }
}
