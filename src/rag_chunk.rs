use serde::{Deserialize, Serialize};

pub(crate) const CHUNK_SIZE: usize = 1200;
const OVERLAP: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Strategy {
    Fixed,
    Structure,
}

impl Strategy {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Fixed => "fixed",
            Self::Structure => "structure",
        }
    }
}

impl std::str::FromStr for Strategy {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "fixed" => Ok(Self::Fixed),
            "structure" => Ok(Self::Structure),
            _ => Err("Стратегия: fixed или structure".into()),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Chunk {
    pub(crate) strategy: Strategy,
    pub(crate) ordinal: usize,
    pub(crate) section: String,
    pub(crate) text: String,
}

pub(crate) fn split(text: &str, title: &str, strategy: Strategy) -> Vec<Chunk> {
    let sections = sections(text, title);
    let parts = match strategy {
        Strategy::Fixed => {
            let (texts, _) = windows(text, CHUNK_SIZE, OVERLAP);
            texts
                .into_iter()
                .map(|(start, value)| {
                    let section = sections
                        .iter()
                        .rev()
                        .find(|part| part.start <= start)
                        .map_or_else(|| title.to_owned(), |part| part.heading.clone());
                    (section, value)
                })
                .collect::<Vec<_>>()
        }
        Strategy::Structure => sections
            .into_iter()
            .flat_map(|part| {
                let prefix = format!("{}\n", part.heading);
                let capacity = CHUNK_SIZE.saturating_sub(prefix.chars().count()).max(1);
                let (values, _) = windows(part.content.trim(), capacity, OVERLAP.min(capacity / 4));
                values
                    .into_iter()
                    .map(move |(_, value)| (part.heading.clone(), format!("{prefix}{value}")))
            })
            .collect::<Vec<_>>(),
    };
    parts
        .into_iter()
        .enumerate()
        .filter_map(|(ordinal, (section, text))| {
            (!text.trim().is_empty()).then_some(Chunk {
                strategy,
                ordinal,
                section,
                text,
            })
        })
        .collect()
}

struct Section<'a> {
    heading: String,
    start: usize,
    content: &'a str,
}

fn sections<'a>(text: &'a str, title: &str) -> Vec<Section<'a>> {
    let mut boundaries = vec![(0, title.to_owned())];
    let mut fence = false;
    let mut offset = 0;
    let code = [".rs", ".py", ".js", ".ts", ".go", ".java", ".c", ".cpp"]
        .iter()
        .any(|extension| title.ends_with(extension));
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if !code && (trimmed.starts_with("```") || trimmed.starts_with("~~~")) {
            fence = !fence;
        } else if !fence
            && let Some(heading) = if code {
                code_heading(line)
            } else {
                markdown_heading(trimmed)
            }
        {
            if offset > 0 {
                boundaries.push((offset, heading));
            } else {
                boundaries[0].1 = heading;
            }
        }
        offset += line.len();
    }
    boundaries
        .iter()
        .enumerate()
        .filter_map(|(index, (start, heading))| {
            let end = boundaries
                .get(index + 1)
                .map_or(text.len(), |(next, _)| *next);
            let content = &text[*start..end];
            (!content.trim().is_empty()).then(|| Section {
                heading: heading.clone(),
                start: text[..*start].chars().count(),
                content,
            })
        })
        .collect()
}

fn code_heading(line: &str) -> Option<String> {
    if line.starts_with(char::is_whitespace) {
        return None;
    }
    let text = line.trim();
    [
        "fn ",
        "pub fn ",
        "pub(crate) fn ",
        "struct ",
        "pub struct ",
        "enum ",
        "pub enum ",
        "impl ",
        "trait ",
        "pub trait ",
        "def ",
        "class ",
        "function ",
        "export function ",
        "func ",
        "type ",
    ]
    .iter()
    .any(|prefix| text.starts_with(prefix))
    .then(|| text.chars().take(80).collect())
}

fn markdown_heading(line: &str) -> Option<String> {
    let count = line.bytes().take_while(|byte| *byte == b'#').count();
    if !(1..=6).contains(&count)
        || !line
            .as_bytes()
            .get(count)
            .is_some_and(u8::is_ascii_whitespace)
    {
        return None;
    }
    let heading = line[count..].trim().trim_end_matches('#').trim();
    (!heading.is_empty()).then(|| heading.to_owned())
}

/// Character offsets, always progressing even when the input has no spaces.
fn windows(text: &str, size: usize, overlap: usize) -> (Vec<(usize, String)>, usize) {
    let chars: Vec<char> = text.chars().collect();
    let mut result = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let hard_end = (start + size).min(chars.len());
        let mut end = hard_end;
        if hard_end < chars.len() {
            let lower = (start + size.saturating_sub(100)).min(hard_end);
            if let Some(position) = (lower..hard_end)
                .rev()
                .find(|index| chars[*index].is_whitespace())
            {
                end = position + 1;
            }
        }
        let value: String = chars[start..end].iter().collect();
        if !value.trim().is_empty() {
            result.push((start, value.trim().to_owned()));
        }
        if end == chars.len() {
            break;
        }
        start = end.saturating_sub(overlap).max(start + 1);
    }
    (result, chars.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_preserves_long_unicode_text() {
        let text = "Привет ".repeat(500);
        let chunks = split(&text, "Документ", Strategy::Fixed);
        assert!(chunks.len() > 1);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.text.chars().count() <= CHUNK_SIZE)
        );
        assert!(chunks.iter().any(|chunk| chunk.text.contains("Привет")));
    }

    #[test]
    fn structure_ignores_heading_inside_code_fence() {
        let text = "# Первый\nТекст\n```\n# Не заголовок\n```\n## Второй\nОтвет";
        let chunks = split(text, "Документ", Strategy::Structure);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].section, "Первый");
        assert_eq!(chunks[1].section, "Второй");
    }
}
