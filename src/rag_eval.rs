//! Проверка ответов на одинаковых вопросах в пустых чатах с RAG и без RAG.

use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;

use crate::agent::{Agent, AgentRequest};
use crate::api::NeuralDeepClient;
use crate::chat::Chat;
use crate::config::{Config, DEFAULT_MODEL};
use crate::rag::{Hit, RagError, RagService};
use crate::rag_chunk::Strategy;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvalCase {
    question: String,
    expected: String,
    expected_sources: Vec<String>,
    // Эти два поля также используются существующей командой `rag compare`.
    source: String,
    anchor: String,
}

pub(crate) const DEFAULT_EVAL_PATH: &str = "projetcDocs/day22_rag_eval.json";

pub(crate) fn default_report_path() -> Result<PathBuf, RagError> {
    crate::chat::state_directory()
        .map(|directory| directory.join("day22_rag_answers.md"))
        .ok_or_else(|| RagError::Document("Не удалось определить каталог состояния agi".into()))
}

pub(crate) async fn evaluate(service: &RagService, path: &Path) -> Result<String, RagError> {
    let cases = load_cases(path)?;
    let stats = service.stats()?;
    if stats.structure == 0 {
        return Err(RagError::Document(
            "Индекс structure пуст. Добавьте документы: agi rag add PATH".into(),
        ));
    }
    if service.stale_chunks()? > 0 {
        return Err(RagError::Document(
            "Индекс содержит чанки прежней модели. Выполните: agi rag reindex".into(),
        ));
    }
    let indexed = service.list()?;
    for case in &cases {
        for expected in &case.expected_sources {
            if !indexed
                .iter()
                .any(|source| Path::new(&source.path).ends_with(Path::new(expected)))
            {
                return Err(RagError::Document(format!(
                    "Источник {expected} отсутствует в индексе. Добавьте: agi rag add {expected}"
                )));
            }
        }
    }
    let config = Config::from_env().map_err(|error| RagError::Document(error.to_string()))?;
    let client = NeuralDeepClient::new(config.api_key, config.base_url)
        .map_err(|error| RagError::Api(error.to_string()))?;
    let agent = Agent::new(client);
    let mut report = format!(
        "# День 22 — сравнение ответов с RAG и без RAG\n\nМодель ответа: `{DEFAULT_MODEL}`. Эмбеддинги: `{}`. Стратегия: `structure`. Источников в индексе: {}. Вопросов: {}.\n\nКаждый ответ получен в новом пустом чате с одинаковыми настройками модели. Порядок: сначала без RAG, затем с RAG. Оцените каждый ответ по ожиданию: 0 — неверно, 1 — частично, 2 — полностью. Для RAG отдельно проверьте, что нужный источник попал в найденные фрагменты и модель сослалась на его номер `[n]`. Список «Найденные источники» сам по себе не является ссылкой модели.\n",
        service.embedding_config().model,
        stats.sources,
        cases.len()
    );
    for (index, case) in cases.iter().enumerate() {
        let hits = service.context(&case.question, Strategy::Structure).await?;
        if hits.is_empty() {
            return Err(RagError::Document(format!(
                "Для вопроса {} не найдено фрагментов: {}",
                index + 1,
                case.question
            )));
        }
        let (without, with) = answer_pair(&agent, &case.question, hits.clone()).await?;
        report.push_str(&format!(
            "\n## {}. {}\n\n**Ожидание:** {}\n\n**Нужные источники:** {}\n\n**Найденные фрагменты:**\n\n",
            index + 1,
            case.question,
            case.expected,
            case.expected_sources
                .iter()
                .map(|source| format!("`{source}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        for (number, hit) in hits.iter().enumerate() {
            report.push_str(&format!(
                "- [{}] `{}` · {} · {:.3}\n",
                number + 1,
                hit.source,
                hit.section,
                hit.score
            ));
        }
        report.push_str("\n**Ответ без RAG:**\n\n");
        append_answer(&mut report, &without);
        report.push_str("\n**Ответ с RAG:**\n\n");
        append_answer(&mut report, &with);
        report.push_str("\nОценка без RAG: __/2. Оценка с RAG: __/2. Нужный источник найден: __. Корректная ссылка в ответе: __.\n");
    }
    report.push_str(&format!(
        "\n## Итог после ручной проверки\n\n| Режим | Сумма баллов | Максимум |\n|---|---:|---:|\n| Без RAG | __ | {} |\n| С RAG | __ | {} |\n\nПосчитайте число ответов с корректной ссылкой на нужный источник. Зафиксируйте фактические ошибки и случаи, когда правильный фрагмент отсутствовал среди найденных.\n",
        cases.len() * 2,
        cases.len() * 2
    ));
    Ok(report)
}

fn load_cases(path: &Path) -> Result<Vec<EvalCase>, RagError> {
    let text = fs::read_to_string(path)?;
    let cases: Vec<EvalCase> = serde_json::from_str(&text)
        .map_err(|error| RagError::Document(format!("Некорректный --eval: {error}")))?;
    validate_cases(&cases)?;
    Ok(cases)
}

fn validate_cases(cases: &[EvalCase]) -> Result<(), RagError> {
    if cases.is_empty() {
        return Err(RagError::Document("Набор вопросов пуст".into()));
    }
    let mut seen = HashSet::new();
    for (index, case) in cases.iter().enumerate() {
        if case.question.trim().is_empty()
            || case.expected.trim().is_empty()
            || case.source.trim().is_empty()
            || case.anchor.trim().is_empty()
            || case.expected_sources.is_empty()
            || case
                .expected_sources
                .iter()
                .any(|source| source.trim().is_empty())
        {
            return Err(RagError::Document(format!(
                "Вопрос {}: заполните question, expected, expected_sources, source и anchor",
                index + 1
            )));
        }
        if !seen.insert(case.question.as_str()) {
            return Err(RagError::Document(format!(
                "Повторяется вопрос: {}",
                case.question
            )));
        }
        if !case.expected_sources.contains(&case.source) {
            return Err(RagError::Document(format!(
                "Вопрос {}: source должен входить в expected_sources",
                index + 1
            )));
        }
        for source in &case.expected_sources {
            fs::metadata(source).map_err(|error| {
                RagError::Document(format!("Вопрос {}: источник {source}: {error}", index + 1))
            })?;
        }
    }
    Ok(())
}

async fn answer_pair(
    agent: &Agent,
    question: &str,
    hits: Vec<Hit>,
) -> Result<(String, String), RagError> {
    let without = answer(agent, question, Vec::new(), false).await?;
    let with = answer(agent, question, hits, true).await?;
    Ok((without, with))
}

async fn answer(
    agent: &Agent,
    question: &str,
    hits: Vec<Hit>,
    rag_enabled: bool,
) -> Result<String, RagError> {
    let mut chat = Chat::new();
    chat.settings_mut().set_rag_enabled(rag_enabled);
    let request = AgentRequest::new(&chat, question.to_owned(), Vec::new()).with_rag(hits);
    let answer = agent
        .respond_streaming(request, |_| Ok(()))
        .await
        .map_err(|error| RagError::Api(error.to_string()))?;
    if answer.truncated {
        return Err(RagError::Api(format!("Ответ обрезан: {}", question)));
    }
    Ok(answer.content)
}

fn append_answer(report: &mut String, answer: &str) {
    for line in answer.lines() {
        report.push_str("    ");
        report.push_str(line);
        report.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{MockServer, text_response};

    #[test]
    fn canonical_set_has_ten_distinct_questions_and_valid_sources() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let cases = load_cases(&root.join("projetcDocs/day22_rag_eval.json")).expect("eval set");
        assert_eq!(cases.len(), 10);
        for case in cases {
            let source = fs::read_to_string(root.join(&case.source)).expect("Markdown source");
            assert!(source.contains(&case.anchor), "{}", case.question);
        }
    }

    #[tokio::test]
    async fn paired_answers_use_clean_chats_and_only_rag_receives_evidence() {
        let server = MockServer::new(|request| {
            let messages = request["messages"].as_array().expect("messages");
            let evidence = messages.iter().any(|message| {
                message["content"]
                    .as_str()
                    .is_some_and(|text| text.contains("Ниже найдены фрагменты документов"))
            });
            assert_eq!(
                messages
                    .iter()
                    .filter(|message| message["role"] == "user")
                    .count(),
                if evidence { 2 } else { 1 }
            );
            text_response(if evidence {
                "Ответ [1]"
            } else {
                "Ответ без источника"
            })
        });
        let client = NeuralDeepClient::new("test-key".into(), server.url.clone()).expect("client");
        let agent = Agent::new(client);
        let hit = Hit {
            source: "manual.md".into(),
            title: "manual".into(),
            section: "Раздел".into(),
            chunk_id: "chunk-1".into(),
            text: "Факт из документа".into(),
            score: 0.9,
            rerank_score: None,
        };
        let (without, with) = answer_pair(&agent, "Вопрос?", vec![hit])
            .await
            .expect("answers");
        assert_eq!(without, "Ответ без источника");
        assert!(with.starts_with("Ответ [1]"));
        assert!(with.contains("Найденные источники:"));
        assert_eq!(server.requests().len(), 2);
    }
}
