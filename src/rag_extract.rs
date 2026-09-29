use std::fs;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

use quick_xml::Reader;
use quick_xml::events::Event;
use reqwest::multipart;
use serde::Deserialize;

use crate::rag::RagError;

pub(crate) struct DocumentText {
    pub(crate) text: String,
    pub(crate) title: String,
    pub(crate) format: String,
}

pub(crate) async fn extract(
    path: &Path,
    key: &str,
    base_url: &str,
) -> Result<DocumentText, RagError> {
    let title = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Документ")
        .to_owned();
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let text = match extension.as_str() {
        "pdf" => {
            let pages = pdf_extract::extract_text_by_pages(path)
                .map_err(|error| RagError::Document(format!("{}: PDF: {error}", path.display())))?;
            if pages.is_empty() || pages.iter().any(|page| page.trim().is_empty()) {
                ocr(path, key, base_url).await?
            } else {
                pages
                    .into_iter()
                    .enumerate()
                    .map(|(index, page)| format!("# Страница {}\n{}", index + 1, page.trim()))
                    .collect::<Vec<_>>()
                    .join("\n\n")
            }
        }
        "png" | "jpg" | "jpeg" | "webp" | "bmp" | "tiff" | "tif" => {
            ocr(path, key, base_url).await?
        }
        "docx" => docx(path)?,
        "html" | "htm" => {
            let data = fs::read(path)?;
            html2text::from_read(data.as_slice(), 120)
                .map_err(|error| RagError::Document(format!("{}: HTML: {error}", path.display())))?
        }
        _ => {
            let data = fs::read(path)?;
            if data.contains(&0) {
                return Err(RagError::Document(format!(
                    "{}: бинарный файл не поддерживается",
                    path.display()
                )));
            }
            String::from_utf8(data)
                .map_err(|_| RagError::Document(format!("{}: текст не в UTF-8", path.display())))?
        }
    };
    let text = text.replace("\r\n", "\n").trim().to_owned();
    if text.is_empty() {
        return Err(RagError::Document(format!(
            "{}: не удалось извлечь текст",
            path.display()
        )));
    }
    let title = text
        .lines()
        .find_map(|line| line.strip_prefix("# ").map(str::trim))
        .filter(|value| !value.is_empty())
        .unwrap_or(&title)
        .to_owned();
    Ok(DocumentText {
        text,
        title,
        format: extension,
    })
}

fn docx(path: &Path) -> Result<String, RagError> {
    let file = fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| RagError::Document(format!("{}: DOCX: {error}", path.display())))?;
    let mut document = archive
        .by_name("word/document.xml")
        .map_err(|error| RagError::Document(format!("{}: DOCX: {error}", path.display())))?;
    let mut xml = String::new();
    document.read_to_string(&mut xml)?;
    let mut reader = Reader::from_str(&xml);
    let mut text = String::new();
    let mut paragraph = String::new();
    let mut heading_level = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) if event.name().as_ref() == b"w:p" => {
                paragraph.clear();
                heading_level = None;
            }
            Ok(Event::Empty(event)) if event.name().as_ref() == b"w:pStyle" => {
                for attribute in event.attributes().flatten() {
                    if attribute.key.as_ref() == b"w:val" {
                        let value = String::from_utf8_lossy(attribute.value.as_ref());
                        heading_level = value
                            .strip_prefix("Heading")
                            .or_else(|| value.strip_prefix("heading"))
                            .and_then(|value| value.parse::<usize>().ok())
                            .filter(|level| (1..=6).contains(level));
                    }
                }
            }
            Ok(Event::Text(event)) => {
                let decoded = event.decode().map_err(|error| {
                    RagError::Document(format!("{}: DOCX: {error}", path.display()))
                })?;
                let value = quick_xml::escape::unescape(&decoded).map_err(|error| {
                    RagError::Document(format!("{}: DOCX: {error}", path.display()))
                })?;
                paragraph.push_str(&value);
            }
            Ok(Event::End(event)) if event.name().as_ref() == b"w:p" => {
                if !paragraph.trim().is_empty() {
                    if let Some(level) = heading_level {
                        text.push_str(&"#".repeat(level));
                        text.push(' ');
                    }
                    text.push_str(paragraph.trim());
                    text.push_str("\n\n");
                }
            }
            Ok(Event::Empty(event)) if event.name().as_ref() == b"w:tab" => paragraph.push('\t'),
            Ok(Event::Eof) => break,
            Err(error) => {
                return Err(RagError::Document(format!(
                    "{}: DOCX: {error}",
                    path.display()
                )));
            }
            _ => {}
        }
    }
    Ok(text)
}

#[derive(Deserialize)]
struct OcrJob {
    id: String,
}

#[derive(Deserialize)]
struct OcrStatus {
    status: String,
    error: Option<String>,
}

#[derive(Deserialize)]
struct OcrResult {
    content: String,
}

async fn ocr(path: &Path, key: &str, base_url: &str) -> Result<String, RagError> {
    if key.is_empty() {
        return Err(RagError::Document(format!(
            "{}: для OCR нужен NEURALDEEP_API_KEY",
            path.display()
        )));
    }
    let client = reqwest::Client::new();
    let bytes = fs::read(path)?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("document")
        .to_owned();
    let part = multipart::Part::bytes(bytes).file_name(filename);
    let response = client
        .post(format!("{base_url}/ocr/extract"))
        .bearer_auth(key)
        .multipart(
            multipart::Form::new()
                .part("file", part)
                .text("model_profile", "fast"),
        )
        .send()
        .await?;
    let response = checked(response, "загрузка OCR").await?;
    let job: OcrJob = response.json().await?;
    for _ in 0..300 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let response = client
            .get(format!("{base_url}/ocr/jobs/{}", job.id))
            .bearer_auth(key)
            .send()
            .await?;
        let status: OcrStatus = checked(response, "статус OCR").await?.json().await?;
        match status.status.as_str() {
            "completed" => {
                let response = client
                    .get(format!(
                        "{base_url}/ocr/jobs/{}/result?format=markdown",
                        job.id
                    ))
                    .bearer_auth(key)
                    .send()
                    .await?;
                let result: OcrResult = checked(response, "результат OCR").await?.json().await?;
                return Ok(result.content);
            }
            "failed" | "error" => {
                return Err(RagError::Document(format!(
                    "{}: OCR: {}",
                    path.display(),
                    status.error.unwrap_or_else(|| "неизвестная ошибка".into())
                )));
            }
            _ => {}
        }
    }
    Err(RagError::Document(format!(
        "{}: OCR не завершился за 5 минут",
        path.display()
    )))
}

async fn checked(response: reqwest::Response, action: &str) -> Result<reqwest::Response, RagError> {
    if response.status().is_success() {
        Ok(response)
    } else {
        Err(RagError::Document(format!(
            "{action}: HTTP {}",
            response.status()
        )))
    }
}
