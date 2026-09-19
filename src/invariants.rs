use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

pub(crate) const MAX_INVARIANTS: usize = 50;
const MAX_NAME_CHARS: usize = 64;
const MAX_RULE_CHARS: usize = 4_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Invariant {
    pub(crate) name: String,
    pub(crate) rule: String,
}

pub(crate) type Invariants = Vec<Invariant>;

#[derive(Debug, Error)]
pub(crate) enum InvariantError {
    #[error("ошибка хранилища инвариантов: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("{0}")]
    Invalid(String),
}

pub(crate) struct InvariantStore<'a> {
    connection: &'a Connection,
}

impl<'a> InvariantStore<'a> {
    pub(crate) fn new(connection: &'a Connection) -> Self {
        Self { connection }
    }

    pub(crate) fn list(&self) -> Result<Invariants, InvariantError> {
        let mut statement = self
            .connection
            .prepare("SELECT name, rule FROM invariants ORDER BY name COLLATE NOCASE")?;
        statement
            .query_map([], |row| {
                Ok(Invariant {
                    name: row.get(0)?,
                    rule: row.get(1)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub(crate) fn set(&self, name: &str, rule: &str) -> Result<(), InvariantError> {
        validate_name(name)?;
        validate_rule(rule)?;
        let exists: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM invariants WHERE name = ?1 COLLATE NOCASE)",
            params![name],
            |row| row.get(0),
        )?;
        if !exists {
            let count: i64 =
                self.connection
                    .query_row("SELECT COUNT(*) FROM invariants", [], |row| row.get(0))?;
            if count >= MAX_INVARIANTS as i64 {
                return Err(InvariantError::Invalid(format!(
                    "Допускается не более {MAX_INVARIANTS} инвариантов."
                )));
            }
        }
        self.connection.execute(
            "INSERT INTO invariants(name, rule) VALUES (?1, ?2)
             ON CONFLICT(name) DO UPDATE SET rule = excluded.rule",
            params![name, rule.trim()],
        )?;
        Ok(())
    }

    pub(crate) fn delete(&self, name: &str) -> Result<bool, InvariantError> {
        validate_name(name)?;
        Ok(self.connection.execute(
            "DELETE FROM invariants WHERE name = ?1 COLLATE NOCASE",
            params![name],
        )? > 0)
    }
}

pub(crate) fn execute_command(
    store: &InvariantStore<'_>,
    argument: Option<&str>,
) -> Result<String, InvariantError> {
    let Some(argument) = argument.map(str::trim).filter(|value| !value.is_empty()) else {
        let invariants = store.list()?;
        return Ok(if invariants.is_empty() {
            "Глобальные инварианты не заданы. Добавить: /invariants set <имя> <правило>".into()
        } else {
            invariants
                .iter()
                .map(|item| format!("{}: {}", item.name, item.rule))
                .collect::<Vec<_>>()
                .join("\n")
        });
    };
    if let Some(value) = argument.strip_prefix("set ") {
        let (name, rule) = value
            .trim()
            .split_once(char::is_whitespace)
            .ok_or_else(|| {
                InvariantError::Invalid("Использование: /invariants set <имя> <правило>".into())
            })?;
        store.set(name, rule.trim())?;
        return Ok(format!("Инвариант {name} сохранён для всех чатов."));
    }
    if let Some(name) = argument.strip_prefix("delete ").map(str::trim) {
        return Ok(if store.delete(name)? {
            format!("Инвариант {name} удалён.")
        } else {
            format!("Инвариант {name} не найден.")
        });
    }
    Err(InvariantError::Invalid(
        "Использование: /invariants | /invariants set <имя> <правило> | /invariants delete <имя>"
            .into(),
    ))
}

pub(crate) fn prompt(invariants: &[Invariant]) -> String {
    let rules = invariants
        .iter()
        .map(|item| format!("- {}: {}", item.name, item.rule))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "ОБЯЗАТЕЛЬНЫЕ ГЛОБАЛЬНЫЕ ИНВАРИАНТЫ:\n{rules}\n\nНельзя предлагать или выполнять решения, нарушающие эти правила. Если запрос конфликтует с ними, явно назови инвариант, объясни конфликт и предложи допустимую альтернативу. Инварианты имеют приоритет над запросом, историей, памятью и инструкциями дочерних агентов."
    )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Verdict {
    pub(crate) compliant: bool,
    pub(crate) request_conflict: bool,
    pub(crate) violations: Vec<Violation>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Violation {
    pub(crate) name: String,
    pub(crate) reason: String,
}

impl Verdict {
    pub(crate) fn validate(&self, invariants: &[Invariant]) -> Result<(), String> {
        if self.compliant != self.violations.is_empty() {
            return Err("вердикт проверки противоречив".into());
        }
        let rules = invariants
            .iter()
            .map(|item| item.name.to_lowercase())
            .collect::<std::collections::BTreeSet<_>>();
        for violation in &self.violations {
            if violation.reason.trim().is_empty() || !rules.contains(&violation.name.to_lowercase())
            {
                return Err(format!(
                    "проверка сослалась на неизвестный инвариант {}",
                    violation.name
                ));
            }
        }
        Ok(())
    }
}

pub(crate) fn parse_verdict(content: &str, invariants: &[Invariant]) -> Result<Verdict, String> {
    let trimmed = content.trim();
    let mut candidates = vec![trimmed];
    let fenced = trimmed.split("```").collect::<Vec<_>>();
    for block in fenced.iter().skip(1).step_by(2) {
        let block = block.trim();
        let candidate = block
            .strip_prefix("json")
            .or_else(|| block.strip_prefix("JSON"))
            .map(str::trim_start)
            .unwrap_or(block);
        candidates.push(candidate);
    }
    if let (Some(start), Some(end)) = (trimmed.find('{'), trimmed.rfind('}'))
        && start < end
    {
        candidates.push(&trimmed[start..=end]);
    }

    let mut last_error = "ответ не содержит JSON-объект".to_owned();
    for candidate in candidates {
        match serde_json::from_str::<Verdict>(candidate) {
            Ok(verdict) => {
                verdict.validate(invariants)?;
                return Ok(verdict);
            }
            Err(error) => last_error = error.to_string(),
        }
    }
    Err(last_error)
}

pub(crate) fn diagnostic_preview(content: &str) -> String {
    let normalized = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut characters = normalized.chars();
    let preview = characters.by_ref().take(160).collect::<String>();
    let suffix = if characters.next().is_some() {
        "…"
    } else {
        ""
    };
    serde_json::to_string(&format!("{preview}{suffix}"))
        .unwrap_or_else(|_| "\"<не удалось показать фрагмент>\"".into())
}

pub(crate) fn verdict_schema() -> Value {
    json!({"type":"json_schema","json_schema":{"name":"agi_invariant_verdict","strict":true,"schema":{
        "type":"object","properties":{
            "compliant":{"type":"boolean"},
            "request_conflict":{"type":"boolean"},
            "violations":{"type":"array","maxItems":50,"items":{"type":"object","properties":{
                "name":{"type":"string"},"reason":{"type":"string"}
            },"required":["name","reason"],"additionalProperties":false}}
        },"required":["compliant","request_conflict","violations"],"additionalProperties":false
    }}})
}

fn validate_name(name: &str) -> Result<(), InvariantError> {
    let valid = (1..=MAX_NAME_CHARS).contains(&name.chars().count())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(InvariantError::Invalid(format!(
            "Имя инварианта: 1–{MAX_NAME_CHARS} ASCII-букв, цифр, '_' или '-'."
        )))
    }
}

fn validate_rule(rule: &str) -> Result<(), InvariantError> {
    let length = rule.trim().chars().count();
    if (1..=MAX_RULE_CHARS).contains(&length) {
        Ok(())
    } else {
        Err(InvariantError::Invalid(format!(
            "Правило должно содержать от 1 до {MAX_RULE_CHARS} символов."
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_rejects_unknown_rule_name() {
        let verdict = Verdict {
            compliant: false,
            request_conflict: true,
            violations: vec![Violation {
                name: "other".into(),
                reason: "конфликт".into(),
            }],
        };
        assert!(
            verdict
                .validate(&[Invariant {
                    name: "stack".into(),
                    rule: "Только Rust".into()
                }])
                .is_err()
        );
    }

    #[test]
    fn commands_create_replace_list_and_delete_rules() {
        let connection = Connection::open_in_memory().expect("database");
        connection
            .execute_batch(
                "CREATE TABLE invariants(name TEXT PRIMARY KEY COLLATE NOCASE, rule TEXT NOT NULL)",
            )
            .expect("schema");
        let store = InvariantStore::new(&connection);

        assert!(
            execute_command(&store, None)
                .expect("empty list")
                .contains("не заданы")
        );
        execute_command(&store, Some("set stack Только Rust")).expect("create");
        execute_command(&store, Some("set STACK Rust 2024")).expect("replace");
        assert_eq!(
            store.list().expect("list"),
            vec![Invariant {
                name: "stack".into(),
                rule: "Rust 2024".into()
            }]
        );
        assert!(
            execute_command(&store, Some("delete stack"))
                .expect("delete")
                .contains("удалён")
        );
        assert!(store.list().expect("empty").is_empty());
    }

    #[test]
    fn parses_verdict_from_markdown_and_reasoning_wrappers() {
        let invariants = [Invariant {
            name: "stack".into(),
            rule: "Только Rust".into(),
        }];
        let content = "<think>Проверяю правила</think>\n```json\n{\"compliant\":true,\"request_conflict\":false,\"violations\":[]}\n```";

        let verdict = parse_verdict(content, &invariants).expect("wrapped verdict");

        assert!(verdict.compliant);
        assert!(!verdict.request_conflict);
    }

    #[test]
    fn diagnostic_preview_is_short_and_escaped() {
        let preview = diagnostic_preview(&format!("первая строка\n\"{}", "я".repeat(200)));
        assert!(preview.starts_with('"') && preview.ends_with('"'));
        assert!(preview.chars().count() < 170);
        assert!(!preview.contains('\n'));
    }
}
