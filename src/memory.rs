use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

pub(crate) const MAX_WORKING_ENTRIES: usize = 50;
pub(crate) const MAX_WORKING_KEY_CHARS: usize = 64;
pub(crate) const MAX_WORKING_VALUE_CHARS: usize = 1000;
pub(crate) const MAX_LONG_TERM_BYTES: usize = 32 * 1024;

pub(crate) type WorkingMemory = BTreeMap<String, String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LongTermKind {
    Decision,
    Knowledge,
}

impl LongTermKind {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "decision" => Some(Self::Decision),
            "knowledge" => Some(Self::Knowledge),
            _ => None,
        }
    }

    fn directory(self) -> &'static str {
        match self {
            Self::Decision => "decisions",
            Self::Knowledge => "knowledge",
        }
    }
}

pub(crate) fn execute_command(
    store: &crate::chat::ChatStore,
    chat: &mut crate::chat::Chat,
    argument: Option<&str>,
) -> Result<String, String> {
    let memory = store.memory();
    let Some(argument) = argument.map(str::trim).filter(|value| !value.is_empty()) else {
        let decisions = memory
            .list(LongTermKind::Decision)
            .map_err(|error| error.to_string())?;
        let knowledge = memory
            .list(LongTermKind::Knowledge)
            .map_err(|error| error.to_string())?;
        let selected = chat
            .memory_selection()
            .entries
            .iter()
            .map(|entry| format!("{}/{}", entry.kind, entry.name))
            .collect::<Vec<_>>();
        return Ok(format!(
            "Слои памяти:\n  short-term: {} сообщений{}\n  working: {} записей\n  long-term: профиль {}, decisions {}, knowledge {}\n  подключено: профиль {}; {}\n  Markdown: {}",
            chat.messages().len(),
            if chat.summary().is_some() {
                " + резюме"
            } else {
                ""
            },
            chat.working_memory().len(),
            if memory.profile_path().exists() {
                "есть"
            } else {
                "не создан"
            },
            decisions.len(),
            knowledge.len(),
            if chat.memory_selection().profile {
                "да"
            } else {
                "нет"
            },
            if selected.is_empty() {
                "нет выбранных записей".into()
            } else {
                selected.join(", ")
            },
            memory.directory().display()
        ));
    };

    if argument == "short" {
        let mut lines = Vec::new();
        if let Some(summary) = chat.summary() {
            lines.push(format!("Резюме:\n{}", summary.content));
        }
        lines.extend(
            chat.messages()
                .iter()
                .map(|message| format!("{}: {}", message.role.as_api_str(), message.content)),
        );
        return Ok(if lines.is_empty() {
            "Краткосрочная память пуста.".into()
        } else {
            lines.join("\n\n")
        });
    }
    if argument == "context" {
        return memory
            .load_context(chat.working_memory(), chat.memory_selection())
            .map(|context| context.display())
            .map_err(|error| error.to_string());
    }
    if argument == "working" {
        return Ok(display_working(chat.working_memory()));
    }
    if let Some(rest) = argument.strip_prefix("working ") {
        if let Some(rest) = rest.strip_prefix("set ") {
            let (key, value) =
                split_once_required(rest, "Использование: /memory working set <ключ> <текст>")?;
            chat.set_working(key.into(), value.into())?;
            store.save(chat).map_err(|error| error.to_string())?;
            return Ok(format!("Рабочая запись {key} сохранена."));
        }
        if let Some(key) = rest.strip_prefix("delete ").map(str::trim) {
            if key.is_empty() {
                return Err("Использование: /memory working delete <ключ>".into());
            }
            let message = if chat.delete_working(key) {
                store.save(chat).map_err(|error| error.to_string())?;
                format!("Рабочая запись {key} удалена.")
            } else {
                format!("Рабочая запись {key} не найдена.")
            };
            return Ok(message);
        }
        if rest == "clear" {
            let changed = chat.clear_working();
            if changed {
                store.save(chat).map_err(|error| error.to_string())?;
            }
            return Ok(if changed {
                "Рабочая память очищена. Данные в истории диалога не изменены.".into()
            } else {
                "Рабочая память уже пуста.".into()
            });
        }
    }
    if argument == "profile init" {
        return memory
            .initialize_profile()
            .map(|created| {
                if created {
                    format!("Шаблон профиля создан: {}", memory.profile_path().display())
                } else {
                    format!(
                        "Профиль уже существует: {}",
                        memory.profile_path().display()
                    )
                }
            })
            .map_err(|error| error.to_string());
    }
    if argument == "profile show" {
        return memory
            .show_profile()
            .map(|profile| {
                profile.map_or_else(
                    || format!("Профиль не создан: {}", memory.profile_path().display()),
                    |content| {
                        format!(
                            "{}\n\nФайл: {}",
                            content.trim(),
                            memory.profile_path().display()
                        )
                    },
                )
            })
            .map_err(|error| error.to_string());
    }
    if let Some(rest) = argument.strip_prefix("profile set ") {
        let (section, value) = split_once_required(
            rest,
            "Использование: /memory profile set <style|constraints|context> <текст>",
        )?;
        memory
            .set_profile_section(section, value)
            .map_err(|error| error.to_string())?;
        return Ok(format!("Раздел профиля {section} обновлён."));
    }
    if argument == "long" {
        let decisions = memory
            .list(LongTermKind::Decision)
            .map_err(|error| error.to_string())?;
        let knowledge = memory
            .list(LongTermKind::Knowledge)
            .map_err(|error| error.to_string())?;
        return Ok(format!(
            "Decisions: {}\nKnowledge: {}",
            display_names(&decisions),
            display_names(&knowledge)
        ));
    }
    if let Some(rest) = argument.strip_prefix("long ") {
        let (action, rest) = split_once_required(
            rest,
            "Использование: /memory long <set|show|delete> <decision|knowledge> <имя> [текст]",
        )?;
        let (kind_text, rest) = split_once_required(rest, "Укажите категорию и имя записи.")?;
        let kind = LongTermKind::parse(kind_text)
            .ok_or_else(|| "Категория: decision или knowledge.".to_owned())?;
        if action == "set" {
            let (name, content) = split_once_required(
                rest,
                "Использование: /memory long set <decision|knowledge> <имя> <текст>",
            )?;
            memory
                .set_entry(kind, name, content)
                .map_err(|error| error.to_string())?;
            return Ok(format!("Долговременная запись {kind}/{name} сохранена."));
        }
        let name = rest.trim();
        if action == "show" {
            return memory
                .show_entry(kind, name)
                .map_err(|error| error.to_string());
        }
        if action == "delete" {
            let reference = MemoryRef {
                kind,
                name: name.into(),
            };
            let deleted = memory
                .delete_entry(kind, name)
                .map_err(|error| error.to_string())?;
            if chat.unuse_memory(&reference) {
                store.save(chat).map_err(|error| error.to_string())?;
            }
            return Ok(if deleted {
                format!("Долговременная запись {kind}/{name} удалена.")
            } else {
                format!("Долговременная запись {kind}/{name} не найдена.")
            });
        }
    }
    if argument == "use profile" || argument == "unuse profile" {
        let enabled = argument.starts_with("use ");
        let changed = chat.set_profile_enabled(enabled);
        if changed {
            store.save(chat).map_err(|error| error.to_string())?;
        }
        return Ok(format!(
            "Профиль {} для текущего чата.",
            if enabled {
                "подключён"
            } else {
                "отключён"
            }
        ));
    }
    for (prefix, enabled) in [("use ", true), ("unuse ", false)] {
        if let Some(rest) = argument.strip_prefix(prefix) {
            let (kind, name) = parse_reference(rest)?;
            let reference = MemoryRef {
                kind,
                name: name.into(),
            };
            if enabled {
                memory
                    .show_entry(kind, name)
                    .map_err(|error| error.to_string())?;
                chat.use_memory(reference);
            } else {
                chat.unuse_memory(&reference);
            }
            store.save(chat).map_err(|error| error.to_string())?;
            return Ok(format!(
                "Запись {kind}/{name} {} для текущего чата.",
                if enabled {
                    "подключена"
                } else {
                    "отключена"
                }
            ));
        }
    }
    Err("Использование: /memory | short | working ... | profile ... | long ... | use ... | unuse ... | context".into())
}

fn split_once_required<'a>(value: &'a str, usage: &str) -> Result<(&'a str, &'a str), String> {
    value
        .trim()
        .split_once(char::is_whitespace)
        .map(|(first, rest)| (first, rest.trim()))
        .filter(|(_, rest)| !rest.is_empty())
        .ok_or_else(|| usage.to_owned())
}

fn parse_reference(value: &str) -> Result<(LongTermKind, &str), String> {
    let (kind, name) = split_once_required(
        value,
        "Использование: /memory <use|unuse> <decision|knowledge> <имя>",
    )?;
    let kind =
        LongTermKind::parse(kind).ok_or_else(|| "Категория: decision или knowledge.".to_owned())?;
    validate_name(name).map_err(|error| error.to_string())?;
    Ok((kind, name))
}

fn display_working(memory: &WorkingMemory) -> String {
    if memory.is_empty() {
        "Рабочая память пуста.".into()
    } else {
        memory
            .iter()
            .map(|(key, value)| format!("{key} = {value}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn display_names(names: &[String]) -> String {
    if names.is_empty() {
        "нет".into()
    } else {
        names.join(", ")
    }
}

impl fmt::Display for LongTermKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Decision => "decision",
            Self::Knowledge => "knowledge",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct MemoryRef {
    pub(crate) kind: LongTermKind,
    pub(crate) name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MemorySelection {
    pub(crate) profile: bool,
    pub(crate) entries: BTreeSet<MemoryRef>,
}

impl Default for MemorySelection {
    fn default() -> Self {
        Self {
            profile: true,
            entries: BTreeSet::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MemoryContext {
    pub(crate) profile: Option<String>,
    pub(crate) long_term: Vec<(MemoryRef, String)>,
    pub(crate) working: WorkingMemory,
}

impl MemoryContext {
    pub(crate) fn prompt_blocks(&self) -> Vec<String> {
        let mut blocks = Vec::new();
        if let Some(profile) = &self.profile {
            blocks.push(format!("[ДОЛГОВРЕМЕННАЯ ПАМЯТЬ: ПРОФИЛЬ]\n{profile}"));
        }
        for (reference, content) in &self.long_term {
            blocks.push(format!(
                "[ДОЛГОВРЕМЕННАЯ ПАМЯТЬ: {} / {}]\n{content}",
                reference.kind, reference.name
            ));
        }
        if !self.working.is_empty() {
            let values = self
                .working
                .iter()
                .map(|(key, value)| format!("{key}: {value}"))
                .collect::<Vec<_>>()
                .join("\n");
            blocks.push(format!("[РАБОЧАЯ ПАМЯТЬ ТЕКУЩЕЙ ЗАДАЧИ]\n{values}"));
        }
        blocks
    }

    pub(crate) fn display(&self) -> String {
        let blocks = self.prompt_blocks();
        if blocks.is_empty() {
            "Контекст памяти пуст.".into()
        } else {
            blocks.join("\n\n")
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct MemoryStore {
    directory: PathBuf,
}

#[derive(Debug, Error)]
pub(crate) enum MemoryError {
    #[error("не удалось {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{0}")]
    Invalid(String),
}

impl MemoryStore {
    pub(crate) fn new(state_directory: &Path) -> Self {
        Self {
            directory: state_directory.join("memory"),
        }
    }

    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }

    pub(crate) fn profile_path(&self) -> PathBuf {
        self.directory.join("profile.md")
    }

    pub(crate) fn initialize_profile(&self) -> Result<bool, MemoryError> {
        let path = self.profile_path();
        if path.exists() {
            return Ok(false);
        }
        self.write_atomic(&path, profile_template())?;
        Ok(true)
    }

    pub(crate) fn load_context(
        &self,
        working: &WorkingMemory,
        selection: &MemorySelection,
    ) -> Result<MemoryContext, MemoryError> {
        let profile = if selection.profile {
            self.read_optional(&self.profile_path())?
        } else {
            None
        };
        let mut long_term = Vec::new();
        for reference in &selection.entries {
            let path = self.entry_path(reference.kind, &reference.name)?;
            let content = self.read_required(&path)?;
            long_term.push((reference.clone(), content));
        }
        let bytes = profile.as_ref().map_or(0, String::len)
            + long_term
                .iter()
                .map(|(_, content)| content.len())
                .sum::<usize>();
        if bytes > MAX_LONG_TERM_BYTES {
            return Err(MemoryError::Invalid(format!(
                "Выбранная долговременная память занимает {bytes} байт; допустимо не более {MAX_LONG_TERM_BYTES}. Отключите часть записей через /memory unuse."
            )));
        }
        Ok(MemoryContext {
            profile,
            long_term,
            working: working.clone(),
        })
    }

    pub(crate) fn show_profile(&self) -> Result<Option<String>, MemoryError> {
        self.read_optional(&self.profile_path())
    }

    pub(crate) fn set_profile_section(
        &self,
        section: &str,
        value: &str,
    ) -> Result<(), MemoryError> {
        let heading = match section {
            "style" => "Style",
            "constraints" => "Constraints",
            "context" => "Context",
            _ => {
                return Err(MemoryError::Invalid(
                    "Раздел профиля: style, constraints или context.".into(),
                ));
            }
        };
        if value.trim().is_empty() {
            return Err(MemoryError::Invalid(
                "Текст профиля не должен быть пустым.".into(),
            ));
        }
        let current = self
            .show_profile()?
            .unwrap_or_else(|| profile_template().to_owned());
        let updated = replace_profile_section(&current, heading, value.trim())?;
        self.write_atomic(&self.profile_path(), &updated)
    }

    pub(crate) fn set_entry(
        &self,
        kind: LongTermKind,
        name: &str,
        content: &str,
    ) -> Result<(), MemoryError> {
        validate_name(name)?;
        if content.trim().is_empty() {
            return Err(MemoryError::Invalid(
                "Текст записи не должен быть пустым.".into(),
            ));
        }
        let path = self.entry_path(kind, name)?;
        self.write_atomic(&path, content.trim())
    }

    pub(crate) fn show_entry(&self, kind: LongTermKind, name: &str) -> Result<String, MemoryError> {
        self.read_required(&self.entry_path(kind, name)?)
    }

    pub(crate) fn delete_entry(&self, kind: LongTermKind, name: &str) -> Result<bool, MemoryError> {
        let path = self.entry_path(kind, name)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(MemoryError::Io {
                action: "удалить",
                path,
                source,
            }),
        }
    }

    pub(crate) fn list(&self, kind: LongTermKind) -> Result<Vec<String>, MemoryError> {
        let directory = self.directory.join(kind.directory());
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(MemoryError::Io {
                    action: "прочитать каталог",
                    path: directory,
                    source,
                });
            }
        };
        let mut names = entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                (path.extension().and_then(|value| value.to_str()) == Some("md"))
                    .then(|| path.file_stem()?.to_str().map(str::to_owned))
                    .flatten()
            })
            .collect::<Vec<_>>();
        names.sort();
        Ok(names)
    }

    fn entry_path(&self, kind: LongTermKind, name: &str) -> Result<PathBuf, MemoryError> {
        validate_name(name)?;
        Ok(self
            .directory
            .join(kind.directory())
            .join(format!("{name}.md")))
    }

    fn read_optional(&self, path: &Path) -> Result<Option<String>, MemoryError> {
        match fs::read_to_string(path) {
            Ok(content) => Ok((!content.trim().is_empty()).then_some(content)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(MemoryError::Io {
                action: "прочитать",
                path: path.to_owned(),
                source,
            }),
        }
    }

    fn read_required(&self, path: &Path) -> Result<String, MemoryError> {
        self.read_optional(path)?.ok_or_else(|| {
            MemoryError::Invalid(format!(
                "Выбранная запись памяти не найдена: {}",
                path.display()
            ))
        })
    }

    fn write_atomic(&self, path: &Path, content: &str) -> Result<(), MemoryError> {
        let parent = path.parent().ok_or_else(|| {
            MemoryError::Invalid(format!("Некорректный путь памяти: {}", path.display()))
        })?;
        fs::create_dir_all(parent).map_err(|source| MemoryError::Io {
            action: "создать каталог",
            path: parent.to_owned(),
            source,
        })?;
        let temporary = parent.join(format!(".memory-{}.tmp", Uuid::new_v4().simple()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|source| MemoryError::Io {
                    action: "создать временный файл",
                    path: temporary.clone(),
                    source,
                })?;
            file.write_all(content.as_bytes())
                .and_then(|_| file.sync_all())
                .map_err(|source| MemoryError::Io {
                    action: "записать",
                    path: temporary.clone(),
                    source,
                })?;
            fs::rename(&temporary, path).map_err(|source| MemoryError::Io {
                action: "заменить",
                path: path.to_owned(),
                source,
            })
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

pub(crate) fn validate_working_entry(key: &str, value: &str) -> Result<(), String> {
    if key.is_empty()
        || key.chars().count() > MAX_WORKING_KEY_CHARS
        || key
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(format!(
            "Ключ рабочей памяти должен содержать 1–{MAX_WORKING_KEY_CHARS} символа без пробелов."
        ));
    }
    if value.trim().is_empty() || value.chars().count() > MAX_WORKING_VALUE_CHARS {
        return Err(format!(
            "Значение рабочей памяти должно содержать 1–{MAX_WORKING_VALUE_CHARS} символов."
        ));
    }
    Ok(())
}

pub(crate) fn validate_working_memory(memory: &WorkingMemory) -> Result<(), String> {
    if memory.len() > MAX_WORKING_ENTRIES {
        return Err(format!(
            "Допускается не более {MAX_WORKING_ENTRIES} записей рабочей памяти."
        ));
    }
    for (key, value) in memory {
        validate_working_entry(key, value)?;
    }
    Ok(())
}

pub(crate) fn validate_name(name: &str) -> Result<(), MemoryError> {
    let valid = (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(MemoryError::Invalid(
            "Имя должно содержать 1–64 ASCII-символа: буквы, цифры, '_' или '-'.".into(),
        ))
    }
}

fn profile_template() -> &'static str {
    "# Профиль пользователя\n\n## Style\n\n## Constraints\n\n## Context\n"
}

fn replace_profile_section(
    profile: &str,
    section: &str,
    value: &str,
) -> Result<String, MemoryError> {
    let heading = format!("## {section}");
    let start = profile
        .lines()
        .enumerate()
        .filter(|(_, line)| line.trim() == heading)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if start.len() != 1 {
        return Err(MemoryError::Invalid(format!(
            "Профиль должен содержать ровно один раздел `{heading}`. Исходный файл сохранён."
        )));
    }
    let mut lines = profile.lines().map(str::to_owned).collect::<Vec<_>>();
    let start = start[0];
    let end = lines[start + 1..]
        .iter()
        .position(|line| line.starts_with("## "))
        .map_or(lines.len(), |offset| start + 1 + offset);
    lines.splice(
        start + 1..end,
        [String::new(), value.to_owned(), String::new()],
    );
    Ok(format!("{}\n", lines.join("\n").trim_end()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{Chat, ChatStore};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("agi-memory-test-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).expect("test directory");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn profile_section_update_preserves_other_sections() {
        let updated =
            replace_profile_section(profile_template(), "Style", "Кратко").expect("valid template");
        assert!(updated.contains("## Style\n\nКратко"));
        assert!(updated.contains("## Constraints"));
        assert!(updated.contains("## Context"));
    }

    #[test]
    fn unsafe_entry_names_are_rejected() {
        assert!(validate_name("../secret").is_err());
        assert!(validate_name("rust-guides_1").is_ok());
    }

    #[test]
    fn explicit_commands_keep_layers_separate_and_persist_memory_only_chat() {
        let directory = TestDirectory::new();
        let store = ChatStore::for_tests(directory.0.clone()).expect("store");
        let mut chat = Chat::new();

        execute_command(&store, &mut chat, Some("profile init")).expect("profile init");
        execute_command(&store, &mut chat, Some("profile set style Отвечай кратко"))
            .expect("profile update");
        execute_command(
            &store,
            &mut chat,
            Some("working set goal Реализовать память"),
        )
        .expect("working memory");
        execute_command(
            &store,
            &mut chat,
            Some("long set knowledge rust Rust использует ownership"),
        )
        .expect("long term memory");

        let before = store
            .memory()
            .load_context(chat.working_memory(), chat.memory_selection())
            .expect("context before use");
        assert!(
            before
                .profile
                .as_deref()
                .is_some_and(|value| value.contains("Отвечай кратко"))
        );
        assert_eq!(
            before.working.get("goal").map(String::as_str),
            Some("Реализовать память")
        );
        assert!(before.long_term.is_empty());
        assert!(chat.messages().is_empty());

        execute_command(&store, &mut chat, Some("use knowledge rust")).expect("select");
        let id = chat.id();
        let mut restored = store.load(id).expect("memory-only chat persists");
        let after = store
            .memory()
            .load_context(restored.working_memory(), restored.memory_selection())
            .expect("restored context");
        assert_eq!(after.long_term.len(), 1);
        assert!(after.long_term[0].1.contains("ownership"));
        assert!(restored.messages().is_empty());

        execute_command(&store, &mut restored, Some("unuse knowledge rust")).expect("unselect");
        execute_command(&store, &mut restored, Some("working clear")).expect("clear working");
        let cleared = store.load(id).expect("cleared chat still loads");
        assert!(cleared.working_memory().is_empty());
        assert!(cleared.memory_selection().entries.is_empty());
    }

    #[test]
    fn manual_markdown_edits_are_loaded_for_every_request() {
        let directory = TestDirectory::new();
        let memory = MemoryStore::new(&directory.0);
        memory.initialize_profile().expect("profile");
        fs::write(
            memory.profile_path(),
            "# Профиль\n\n## Style\nНовый стиль\n\n## Constraints\n\n## Context\n",
        )
        .expect("manual edit");
        let context = memory
            .load_context(&WorkingMemory::new(), &MemorySelection::default())
            .expect("reload");
        assert!(context.profile.expect("profile").contains("Новый стиль"));
    }

    #[test]
    fn missing_selected_entry_and_oversized_context_are_reported() {
        let directory = TestDirectory::new();
        let memory = MemoryStore::new(&directory.0);
        let reference = MemoryRef {
            kind: LongTermKind::Knowledge,
            name: "missing".into(),
        };
        let mut selection = MemorySelection {
            profile: false,
            ..MemorySelection::default()
        };
        selection.entries.insert(reference);
        assert!(
            memory
                .load_context(&WorkingMemory::new(), &selection)
                .is_err()
        );

        memory
            .set_entry(
                LongTermKind::Knowledge,
                "large",
                &"x".repeat(MAX_LONG_TERM_BYTES + 1),
            )
            .expect("write large entry");
        selection.entries.clear();
        selection.entries.insert(MemoryRef {
            kind: LongTermKind::Knowledge,
            name: "large".into(),
        });
        assert!(
            memory
                .load_context(&WorkingMemory::new(), &selection)
                .is_err()
        );
    }
}
