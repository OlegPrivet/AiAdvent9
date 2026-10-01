use std::env;
use std::fs;
use std::path::PathBuf;

use serde::Deserialize;

use crate::chat::{Chat, ChatStore};
use crate::cli::{EmbeddingsCommand, RagCommand, RootCommand};
use crate::config::DEFAULT_BASE_URL;
use crate::rag::{RagError, RagService};
use crate::rag_chunk::Strategy;
use crate::rag_pipeline::RagOptions;

pub(crate) async fn slash(
    argument: Option<&str>,
    chat: &mut Chat,
    store: &ChatStore,
) -> Result<String, RagError> {
    let mut service =
        RagService::open(env::var("NEURALDEEP_API_KEY").ok(), DEFAULT_BASE_URL.into())?;
    let input = argument.unwrap_or("status").trim();
    let (action, tail) = input
        .split_once(char::is_whitespace)
        .map_or((input, ""), |(a, b)| (a, b.trim()));
    match action {
        "on" | "off" => {
            chat.settings_mut().set_rag_enabled(action == "on");
            chat.mark_changed();
            store
                .save(chat)
                .map_err(|error| RagError::Document(error.to_string()))?;
            Ok(format!(
                "RAG {}",
                if action == "on" {
                    "включён"
                } else {
                    "выключен"
                }
            ))
        }
        "filter" | "rewrite" | "topk" | "threshold" => {
            let options = chat.settings().rag_options().command(action, tail).map_err(RagError::Document)?;
            let message = options.status();
            chat.settings_mut().set_rag_options(options).map_err(RagError::Document)?;
            chat.mark_changed();
            store.save(chat).map_err(|e| RagError::Document(e.to_string()))?;
            Ok(message)
        }
        "strategy" => {
            let strategy = tail.parse::<Strategy>().map_err(RagError::Document)?;
            chat.settings_mut().set_rag_strategy(strategy);
            chat.mark_changed();
            store
                .save(chat)
                .map_err(|error| RagError::Document(error.to_string()))?;
            Ok(format!("Стратегия RAG: {}", strategy.as_str()))
        }
        "status" => {
            let stats = service.stats()?;
            Ok(format!(
                "RAG: {}; стратегия: {}; источников: {}; слов: {}; чанков fixed/structure: {}/{}\n{}",
                if chat.settings().rag_enabled() {
                    "включён"
                } else {
                    "выключен"
                },
                chat.settings().rag_strategy().as_str(),
                stats.sources,
                stats.words,
                stats.fixed,
                stats.structure,
                format_args!("{}\n{}", embedding_status(&service)?, chat.settings().rag_options().status())
            ))
        }
        "embeddings" => embeddings_slash(&mut service, tail).await,
        "list" => {
            let sources = service.list()?;
            if sources.is_empty() {
                Ok("Документов нет. Добавьте: /rag add <путь>".into())
            } else {
                Ok(sources
                    .into_iter()
                    .map(|source| {
                        format!(
                            "{} · {} · {} · {} слов",
                            source.id, source.path, source.title, source.words
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
        }
        "add" => {
            if tail.is_empty() {
                return Err(RagError::Document("Использование: /rag add <путь>".into()));
            }
            let path = expand_tilde(tail);
            let paths = RagService::collect_paths(&[path])?;
            if paths.is_empty() {
                return Err(RagError::Document("Файлы не найдены".into()));
            }
            let mut lines = Vec::new();
            for path in paths {
                match service.add(&path).await {
                    Ok((words, chunks)) => {
                        lines.push(format!("{}: {words} слов, {chunks} чанков", path.display()))
                    }
                    Err(error) => lines.push(format!("{}: {error}", path.display())),
                }
            }
            Ok(lines.join("\n"))
        }
        "remove" => {
            if service.remove(tail)? {
                Ok(format!("Источник {tail} удалён"))
            } else {
                Err(RagError::Document(format!("Источник {tail} не найден")))
            }
        }
        "refresh" => Ok(service.refresh().await?.join("\n")),
        "reindex" => Ok(service.reindex().await?.join("\n")),
        "search" => {
            if tail.is_empty() {
                return Err(RagError::Document(
                    "Использование: /rag search <вопрос>".into(),
                ));
            }
            Ok(service.retrieve(tail, chat.settings().rag_strategy(), chat.settings().rag_options()).await?.display())
        }
        "compare" => compare(&service, None, None).await,
        "evaluate" => evaluate_inside_agi(&service, tail).await,
        "report" if tail.is_empty() => read_answer_report(),
        "report" if tail == "day23" => crate::rag_day23::read_report(),
        _ => Err(RagError::Document(
            "Команды: /rag add|list|remove|refresh|reindex|search|compare|evaluate|report|embeddings|on|off|status|strategy|filter|rewrite|topk|threshold".into(),
        )),
    }
}

fn expand_tilde(value: &str) -> PathBuf {
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(value)
}

pub(crate) async fn background(
    input: String,
    strategy: Strategy,
    options: RagOptions,
) -> Result<String, RagError> {
    let mut service =
        RagService::open(env::var("NEURALDEEP_API_KEY").ok(), DEFAULT_BASE_URL.into())?;
    let (action, tail) = input
        .split_once(char::is_whitespace)
        .map_or((input.as_str(), ""), |(a, b)| (a, b.trim()));
    match action {
        "add" => {
            if tail.is_empty() {
                return Err(RagError::Document("Использование: /rag add <путь>".into()));
            }
            let paths = RagService::collect_paths(&[expand_tilde(tail)])?;
            if paths.is_empty() {
                return Err(RagError::Document("Файлы не найдены".into()));
            }
            let mut lines = Vec::new();
            for path in paths {
                match service.add(&path).await {
                    Ok((words, chunks)) => {
                        lines.push(format!("{}: {words} слов, {chunks} чанков", path.display()))
                    }
                    Err(error) => lines.push(format!("{}: {error}", path.display())),
                }
            }
            Ok(lines.join("\n"))
        }
        "refresh" => Ok(service.refresh().await?.join("\n")),
        "reindex" => Ok(service.reindex().await?.join("\n")),
        "embeddings" => embeddings_slash(&mut service, tail).await,
        "search" => {
            if tail.is_empty() {
                return Err(RagError::Document(
                    "Использование: /rag search <вопрос>".into(),
                ));
            }
            Ok(service.retrieve(tail, strategy, &options).await?.display())
        }
        "compare" => compare(&service, None, None).await,
        "evaluate" => evaluate_inside_agi(&service, tail).await,
        "report" if tail.is_empty() => read_answer_report(),
        "report" if tail == "day23" => crate::rag_day23::read_report(),
        _ => Err(RagError::Document(
            "Команды: add|refresh|reindex|search|compare|evaluate|report|embeddings".into(),
        )),
    }
}

async fn evaluate_inside_agi(service: &RagService, input: &str) -> Result<String, RagError> {
    if input == "day23" || input.starts_with("day23 ") {
        let path = input.strip_prefix("day23").unwrap_or_default().trim();
        let path = if path.is_empty() {
            PathBuf::from(crate::rag_day23::DEFAULT_EVAL_PATH)
        } else {
            expand_tilde(path)
        };
        return crate::rag_day23::evaluate(
            service,
            &path,
            &crate::rag_day23::default_report_path()?,
        )
        .await;
    }
    let path = if input.is_empty() {
        PathBuf::from(crate::rag_eval::DEFAULT_EVAL_PATH)
    } else {
        expand_tilde(input)
    };
    if !path.is_file() {
        return Err(RagError::Document(format!(
            "Набор вопросов {} не найден. Откройте agi из корня проекта или укажите /rag evaluate <путь>",
            path.display()
        )));
    }
    let report = crate::rag_eval::evaluate(service, &path).await?;
    fs::write(crate::rag_eval::default_report_path()?, &report)?;
    Ok(report)
}

fn read_answer_report() -> Result<String, RagError> {
    let path = crate::rag_eval::default_report_path()?;
    fs::read_to_string(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            RagError::Document("Отчёт ещё не создан. Выполните /rag evaluate".into())
        } else {
            RagError::Io(error)
        }
    })
}

async fn embeddings_slash(service: &mut RagService, input: &str) -> Result<String, RagError> {
    let (action, tail) = input
        .split_once(char::is_whitespace)
        .map_or((input, ""), |(action, tail)| (action, tail.trim()));
    let stale = match action {
        "" | "show" if tail.is_empty() => return embedding_status(service),
        "reset" if tail.is_empty() => service.reset_embeddings()?,
        "set" => {
            let (url, model, api_key_env) = parse_embedding_set(tail)?;
            service
                .configure_embeddings(url, model, api_key_env)
                .await?
        }
        _ => {
            return Err(RagError::Document(
                "Использование: /rag embeddings set URL MODEL [KEY_ENV] | show | reset".into(),
            ));
        }
    };
    let mut status = embedding_status(service)?;
    if stale > 0 {
        status.push_str("\nВыполните /rag reindex перед поиском");
    }
    Ok(status)
}

fn parse_embedding_set(input: &str) -> Result<(String, String, Option<String>), RagError> {
    let args = input.split_whitespace().collect::<Vec<_>>();
    if args.first().is_some_and(|arg| arg.starts_with("--")) {
        let mut url = None;
        let mut model = None;
        let mut key_env = None;
        let (pairs, remainder) = args.as_chunks::<2>();
        for pair in pairs {
            match pair[0] {
                "--url" if url.is_none() => url = Some(pair[1].to_owned()),
                "--model" if model.is_none() => model = Some(pair[1].to_owned()),
                "--api-key-env" if key_env.is_none() => key_env = Some(pair[1].to_owned()),
                _ => return Err(embedding_set_usage()),
            }
        }
        if !remainder.is_empty() {
            return Err(embedding_set_usage());
        }
        return Ok((
            url.ok_or_else(embedding_set_usage)?,
            model.ok_or_else(embedding_set_usage)?,
            key_env,
        ));
    }
    match args.as_slice() {
        [url, model] => Ok(((*url).into(), (*model).into(), None)),
        [url, model, key_env] => Ok(((*url).into(), (*model).into(), Some((*key_env).into()))),
        _ => Err(embedding_set_usage()),
    }
}

fn embedding_set_usage() -> RagError {
    RagError::Document(
        "Использование: /rag embeddings set URL MODEL [KEY_ENV] или set --url URL --model MODEL [--api-key-env KEY_ENV]".into(),
    )
}

pub(crate) async fn run(root: RootCommand) -> Result<(), RagError> {
    let RootCommand::Rag { command } = root;
    if let RagCommand::Add {
        paths,
        dry_run: true,
    } = &command
    {
        let paths = RagService::collect_paths(paths)?;
        if paths.is_empty() {
            return Err(RagError::Document("Подходящие файлы не найдены".into()));
        }
        for path in paths {
            let bytes = fs::metadata(&path)?.len();
            let words = fs::read(&path)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .map(|text| text.split_whitespace().count());
            println!(
                "{}: {} байт, {}",
                path.display(),
                bytes,
                words.map_or_else(
                    || "текст после извлечения/OCR".into(),
                    |words| format!("{words} слов")
                )
            );
        }
        return Ok(());
    }
    let mut service =
        RagService::open(env::var("NEURALDEEP_API_KEY").ok(), DEFAULT_BASE_URL.into())?;
    match command {
        RagCommand::Embeddings { command } => match command {
            EmbeddingsCommand::Show => println!("{}", embedding_status(&service)?),
            EmbeddingsCommand::Set {
                url,
                model,
                api_key_env,
            } => {
                let stale = service
                    .configure_embeddings(url, model, api_key_env)
                    .await?;
                println!("{}", embedding_status(&service)?);
                if stale > 0 {
                    println!("Выполните agi rag reindex перед поиском");
                }
            }
            EmbeddingsCommand::Reset => {
                let stale = service.reset_embeddings()?;
                println!("{}", embedding_status(&service)?);
                if stale > 0 {
                    println!("Выполните agi rag reindex перед поиском");
                }
            }
        },
        RagCommand::Reindex => {
            for line in service.reindex().await? {
                println!("{line}");
            }
            if service.stale_chunks()? > 0 {
                return Err(RagError::Document(
                    "Переиндексация не завершена; исправьте ошибки и повторите agi rag reindex"
                        .into(),
                ));
            }
        }
        RagCommand::Add { paths, dry_run: _ } => {
            let paths = RagService::collect_paths(&paths)?;
            if paths.is_empty() {
                return Err(RagError::Document("Подходящие файлы не найдены".into()));
            }
            let mut failures = 0;
            for path in paths {
                match service.add(&path).await {
                    Ok((words, chunks)) => {
                        println!(
                            "Добавлено {}: {words} слов, {chunks} чанков",
                            path.display()
                        );
                    }
                    Err(error) => {
                        failures += 1;
                        eprintln!("{}: {error}", path.display());
                    }
                }
            }
            if failures > 0 {
                return Err(RagError::Document(format!(
                    "Не обработано файлов: {failures}"
                )));
            }
        }
        RagCommand::List => {
            for source in service.list()? {
                println!(
                    "{}\t{}\t{}\t{} слов",
                    source.id, source.path, source.title, source.words
                );
            }
        }
        RagCommand::Status => {
            let stats = service.stats()?;
            println!("Источники: {}; слов: {}", stats.sources, stats.words);
            println!(
                "Чанки: fixed={}, structure={}",
                stats.fixed, stats.structure
            );
            println!("{}", embedding_status(&service)?);
        }
        RagCommand::Remove { id } => {
            if service.remove(&id)? {
                println!("Источник {id} удалён");
            } else {
                return Err(RagError::Document(format!("Источник {id} не найден")));
            }
        }
        RagCommand::Refresh => {
            for line in service.refresh().await? {
                println!("{line}");
            }
        }
        RagCommand::Search {
            query,
            strategy,
            limit,
            filter,
            rewrite,
            candidate_k,
            similarity_threshold,
            rerank_threshold,
        } => {
            let options = RagOptions {
                filter,
                rewrite,
                candidate_k,
                context_k: limit,
                similarity_threshold,
                rerank_threshold,
            };
            if filter == crate::rag_pipeline::RelevanceMode::Off && !rewrite {
                print_hits(
                    &service
                        .search(&query, strategy.into(), limit.clamp(1, 20))
                        .await?,
                );
            } else {
                options.validate().map_err(RagError::Document)?;
                println!(
                    "{}",
                    service
                        .retrieve(&query, strategy.into(), &options)
                        .await?
                        .display()
                );
            }
        }
        RagCommand::Compare {
            queries,
            eval,
            report,
        } => {
            let text = compare(&service, queries, eval).await?;
            print!("{text}");
            if let Some(path) = report {
                fs::write(&path, text)?;
                println!("Отчёт: {}", path.display());
            }
        }
        RagCommand::Evaluate {
            eval,
            report,
            suite,
        } => {
            if suite == "day23" {
                crate::rag_day23::evaluate(&service, &eval, &report).await?;
                println!("Отчёт: {}", report.display());
                return Ok(());
            }
            let text = crate::rag_eval::evaluate(&service, &eval).await?;
            fs::write(&report, text)?;
            println!("Отчёт: {}", report.display());
        }
    }
    Ok(())
}

pub(crate) fn embedding_status(service: &RagService) -> Result<String, RagError> {
    let config = service.embedding_config();
    let auth = config.api_key_env.as_deref().unwrap_or("не требуется");
    let stale = service.stale_chunks()?;
    Ok(format!(
        "Эмбеддинги: {} · {} · размерность: {} · ключ: {} · чанков старой модели: {}",
        config.model, config.url, config.dimensions, auth, stale
    ))
}

fn print_hits(hits: &[crate::rag::Hit]) {
    for (index, hit) in hits.iter().enumerate() {
        println!(
            "{}. {:.3} {} · {} · {}\n   {}\n   {}",
            index + 1,
            hit.score,
            hit.source,
            hit.title,
            hit.section,
            hit.chunk_id,
            hit.text
                .chars()
                .take(180)
                .collect::<String>()
                .replace('\n', " ")
        );
    }
}

#[derive(Deserialize)]
struct EvalCase {
    question: String,
    source: String,
    anchor: String,
}

async fn compare(
    service: &RagService,
    queries: Option<PathBuf>,
    eval: Option<PathBuf>,
) -> Result<String, RagError> {
    if service.stale_chunks()? > 0 {
        return Err(RagError::Document(
            "Индекс содержит чанки прежней модели. Выполните: agi rag reindex".into(),
        ));
    }
    let stats = service.stats()?;
    let mut report = format!(
        "# Сравнение chunking\n\nИсточники: {}, слов: {}. Модель: {}.\n\n| Стратегия | Чанков | Средняя длина | Медиана | P95 | <200 символов | Повторено символов |\n|---|---:|---:|---:|---:|---:|---:|\n",
        stats.sources,
        stats.words,
        service.embedding_config().model
    );
    for (strategy, count) in [
        (Strategy::Fixed, stats.fixed),
        (Strategy::Structure, stats.structure),
    ] {
        let mut lengths = service.lengths(strategy)?;
        lengths.sort_unstable();
        let average = if lengths.is_empty() {
            0
        } else {
            lengths.iter().sum::<usize>() / lengths.len()
        };
        let median = lengths.get(lengths.len() / 2).copied().unwrap_or(0);
        let p95 = lengths
            .get(lengths.len().saturating_sub(1) * 95 / 100)
            .copied()
            .unwrap_or(0);
        let short = lengths.iter().filter(|value| **value < 200).count();
        let repeated = lengths.iter().sum::<usize>().saturating_sub(stats.chars);
        report.push_str(&format!(
            "| {} | {count} | {average} | {median} | {p95} | {short} | {repeated} |\n",
            strategy.as_str()
        ));
    }
    let cases = if let Some(path) = eval {
        let text = fs::read_to_string(path)?;
        serde_json::from_str::<Vec<EvalCase>>(&text)
            .map_err(|error| RagError::Document(format!("Некорректный --eval: {error}")))?
    } else {
        Vec::new()
    };
    let mut questions = if let Some(path) = queries {
        fs::read_to_string(path)?
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    questions.extend(cases.iter().map(|case| case.question.clone()));
    if !questions.is_empty() {
        report.push_str("\n## Проверочные вопросы\n");
    }
    let mut hits = [0usize; 2];
    let mut reciprocal = [0.0f32; 2];
    for question in questions {
        report.push_str(&format!("\n### {}\n", question.replace('\n', " ")));
        for (index, strategy) in [Strategy::Fixed, Strategy::Structure]
            .into_iter()
            .enumerate()
        {
            let found = service.search(&question, strategy, 3).await?;
            report.push_str(&format!("\n**{}**\n\n", strategy.as_str()));
            for (rank, hit) in found.iter().enumerate() {
                report.push_str(&format!(
                    "{}. `{}` · {} · {:.3}\n",
                    rank + 1,
                    display_source(&hit.source),
                    hit.section,
                    hit.score
                ));
            }
            if let Some(case) = cases.iter().find(|case| case.question == question)
                && let Some(rank) = found.iter().position(|hit| {
                    hit.source.ends_with(&case.source) && hit.text.contains(&case.anchor)
                })
            {
                hits[index] += 1;
                reciprocal[index] += 1.0 / (rank + 1) as f32;
            }
        }
    }
    if !cases.is_empty() {
        report.push_str(&format!(
            "\n## Метрики по {} эталонам\n\n| Стратегия | hit@3 | MRR@3 |\n|---|---:|---:|\n| fixed | {:.3} | {:.3} |\n| structure | {:.3} | {:.3} |\n",
            cases.len(),
            hits[0] as f32 / cases.len() as f32,
            reciprocal[0] / cases.len() as f32,
            hits[1] as f32 / cases.len() as f32,
            reciprocal[1] / cases.len() as f32
        ));
    }
    Ok(report)
}

fn display_source(source: &str) -> String {
    let path = std::path::Path::new(source);
    std::env::current_dir()
        .ok()
        .and_then(|directory| {
            path.strip_prefix(directory)
                .ok()
                .map(|path| path.display().to_string())
        })
        .unwrap_or_else(|| source.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_positional_and_named_embedding_settings_inside_agi() {
        assert_eq!(
            parse_embedding_set("http://localhost:8000/v1/embeddings my-model KEY")
                .expect("positional settings"),
            (
                "http://localhost:8000/v1/embeddings".into(),
                "my-model".into(),
                Some("KEY".into())
            )
        );
        assert_eq!(
            parse_embedding_set("--model my-model --url http://localhost:8000/v1/embeddings")
                .expect("named settings"),
            (
                "http://localhost:8000/v1/embeddings".into(),
                "my-model".into(),
                None
            )
        );
        assert!(parse_embedding_set("--url http://localhost:8000/v1/embeddings").is_err());
        assert!(parse_embedding_set("one two three four").is_err());
    }
}
