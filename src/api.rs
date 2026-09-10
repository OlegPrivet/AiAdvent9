use std::collections::BTreeMap;
use std::io;
use std::time::{Duration, Instant};

use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

#[cfg(test)]
use crate::chat::ChatMessage;
use crate::metrics::TokenUsage;
use crate::settings::Settings;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const STREAM_RETRY_DELAY: Duration = Duration::from_millis(250);

pub(crate) fn structured_response_format() -> Value {
    json!({
        "type": "json_schema",
        "json_schema": {
            "name": "agi_response",
            "strict": true,
            "schema": {
                "type": "object",
                "properties": {
                    "short_answer": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Основной ответ в 1–3 предложениях"
                    },
                    "details": {
                        "type": "array",
                        "description": "Только надёжные и относящиеся к вопросу пояснения и факты",
                        "items": { "type": "string", "minLength": 1 },
                        "minItems": 1,
                        "maxItems": 8
                    },
                    "conclusion": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Краткий вывод или следующий шаг"
                    }
                },
                "required": ["short_answer", "details", "conclusion"],
                "additionalProperties": false
            }
        }
    })
}

#[derive(Debug, Clone)]
pub(crate) struct NeuralDeepClient {
    http: Client,
    api_key: String,
    base_url: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ApiAnswer {
    pub(crate) content: String,
    pub(crate) truncated: bool,
    pub(crate) elapsed_ms: u64,
    pub(crate) usage: Option<TokenUsage>,
}

#[derive(Debug, Error)]
pub(crate) enum ApiError {
    #[error("неизвестно окно контекста модели {0}; обновите справочник моделей")]
    UnknownContext(String),
    #[error("некорректный tool-call AI-сервиса: {0}")]
    InvalidToolCall(String),
    #[error("не удалось настроить HTTP-клиент: {0}")]
    BuildClient(reqwest::Error),
    #[error("не удалось выполнить запрос к AI-сервису: {0}")]
    Request(String),
    #[error("AI-сервис вернул HTTP {status}: {message}")]
    Http { status: StatusCode, message: String },
    #[error("не удалось разобрать ответ AI-сервиса: {0}")]
    InvalidJson(String),
    #[error("не удалось разобрать фрагмент потокового ответа: {0}")]
    InvalidStream(serde_json::Error),
    #[error(
        "потоковый ответ оборвался без маркера [DONE] (получено {received_bytes} байт, текста: {received_chars} символов, finish_reason: {finish_reason})"
    )]
    IncompleteStream {
        received_bytes: usize,
        received_chars: usize,
        finish_reason: String,
    },
    #[error("не удалось вывести потоковый ответ: {0}")]
    Output(io::Error),
    #[error("Structured Output не соответствует ожидаемой JSON Schema: {0}")]
    InvalidStructuredOutput(serde_json::Error),
    #[error(
        "AI-сервис вернул ответ без текста (finish_reason: {finish_reason}); попробуйте увеличить max_tokens в /settings"
    )]
    MissingContent { finish_reason: String },
}

impl NeuralDeepClient {
    pub(crate) fn new(api_key: String, base_url: String) -> Result<Self, ApiError> {
        let http = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(ApiError::BuildClient)?;

        Ok(Self {
            http,
            api_key,
            base_url: base_url.trim_end_matches('/').to_owned(),
        })
    }

    /// A single ordinary completion for auxiliary UI requests, without tools or chat state.
    pub(crate) async fn complete_text(
        &self,
        messages: &[ApiMessage],
        model: &str,
    ) -> Result<ApiAnswer, ApiError> {
        let started_at = Instant::now();
        let request = json!({
            "model": model, "messages": messages, "max_tokens": 4096,
            "temperature": 0.1, "stream": false,
            "user": Uuid::new_v4().to_string(),
            "chat_template_kwargs": {"enable_thinking": false}
        });
        let response = self
            .post(&request)
            .await?
            .json::<ChatResponse>()
            .await
            .map_err(|error| ApiError::InvalidJson(describe_reqwest_error(&error)))?;
        let choice = response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| missing_content(None))?;
        finish_answer(
            choice.message.content.unwrap_or_default(),
            choice.finish_reason,
            &Settings::default(),
            response.usage.map(Into::into),
            elapsed_millis(started_at.elapsed()),
        )
    }

    #[cfg(test)]
    pub(crate) async fn ask(
        &self,
        history: &[ChatMessage],
        chat_id: Uuid,
        question: &str,
        settings: &Settings,
    ) -> Result<ApiAnswer, ApiError> {
        let started_at = Instant::now();
        let response = self
            .send_request(history, chat_id, question, settings, false)
            .await?;
        let response = response
            .json::<ChatResponse>()
            .await
            .map_err(|error| ApiError::InvalidJson(describe_reqwest_error(&error)))?;
        let usage = response.usage.map(Into::into);
        let choice = response
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| missing_content(None))?;

        finish_answer(
            choice.message.content.unwrap_or_default(),
            choice.finish_reason,
            settings,
            usage,
            elapsed_millis(started_at.elapsed()),
        )
    }

    #[cfg(test)]
    pub(crate) async fn ask_streaming<F>(
        &self,
        history: &[ChatMessage],
        chat_id: Uuid,
        question: &str,
        settings: &Settings,
        on_delta: F,
    ) -> Result<ApiAnswer, ApiError>
    where
        F: FnMut(&str) -> io::Result<()>,
    {
        let mut messages = Vec::new();
        if let Some(prompt) = settings.effective_system_prompt() {
            messages.push(ApiMessage::text("system", prompt));
        }
        messages.extend(
            history
                .iter()
                .map(|message| ApiMessage::text(message.role.as_api_str(), &message.content)),
        );
        messages.push(ApiMessage::text("user", question));
        let turn = self
            .complete_streaming(&messages, chat_id, settings, None, on_delta)
            .await?;
        finish_answer(
            turn.content,
            turn.finish_reason,
            settings,
            turn.usage,
            turn.elapsed_ms,
        )
    }

    pub(crate) async fn complete_streaming<F>(
        &self,
        messages: &[ApiMessage],
        session_id: Uuid,
        settings: &Settings,
        tools: Option<Value>,
        mut on_delta: F,
    ) -> Result<ApiTurn, ApiError>
    where
        F: FnMut(&str) -> io::Result<()>,
    {
        let started_at = Instant::now();
        if settings.context_tokens() == 0 {
            return Err(ApiError::UnknownContext(settings.model().into()));
        }
        let messages = messages.to_vec();
        let mut request = json!({
            "model": settings.model(), "messages": messages,
            "max_tokens": settings.max_tokens(), "temperature": settings.temperature(),
            "user": session_id.to_string(), "chat_template_kwargs": {"enable_thinking": false},
            "stream": true, "stream_options": {"include_usage": true}
        });
        if let Some(tools) = tools {
            request["tools"] = tools;
            request["tool_choice"] = json!("auto");
        } else if settings.response_format_enabled() {
            request["response_format"] = structured_response_format();
        }
        if let Some(stop) = settings.stop_sequence() {
            request["stop"] = json!(stop);
        }
        let mut response = self.post(&request).await?;
        let mut decoder = SseDecoder::default();
        let mut content = String::new();
        let mut finish_reason = None;
        let mut usage = None;
        let mut done = false;
        let mut tool_calls = BTreeMap::new();
        let mut received_bytes = 0_usize;
        let mut received_events = 0_usize;
        let mut retried = false;

        'response: loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    received_bytes = received_bytes.saturating_add(chunk.len());
                    for data in decoder.push(&chunk) {
                        if data == b"[DONE]" {
                            done = true;
                            break 'response;
                        }
                        received_events = received_events.saturating_add(1);
                        consume_stream_chunk(
                            &data,
                            &mut content,
                            &mut finish_reason,
                            &mut usage,
                            &mut tool_calls,
                            &mut on_delta,
                        )?;
                    }
                }
                Ok(None) if received_events == 0 && !retried => {
                    retried = true;
                    tokio::time::sleep(STREAM_RETRY_DELAY).await;
                    response = self.post(&request).await?;
                    decoder = SseDecoder::default();
                    received_bytes = 0;
                }
                Ok(None) => break,
                Err(_) if received_events == 0 && !retried => {
                    retried = true;
                    tokio::time::sleep(STREAM_RETRY_DELAY).await;
                    response = self.post(&request).await?;
                    decoder = SseDecoder::default();
                    received_bytes = 0;
                }
                Err(error) => {
                    let retry = if retried {
                        "; повтор до начала потока уже выполнен"
                    } else {
                        ""
                    };
                    return Err(ApiError::Request(format!(
                        "чтение потокового ответа после {received_bytes} байт{retry}: {}",
                        describe_reqwest_error(&error)
                    )));
                }
            }
        }

        if !done {
            for data in decoder.finish() {
                if data == b"[DONE]" {
                    done = true;
                    break;
                }
                consume_stream_chunk(
                    &data,
                    &mut content,
                    &mut finish_reason,
                    &mut usage,
                    &mut tool_calls,
                    &mut on_delta,
                )?;
            }
        }

        if !done {
            return Err(ApiError::IncompleteStream {
                received_bytes,
                received_chars: content.chars().count(),
                finish_reason: finish_reason.unwrap_or_else(|| "не указан".into()),
            });
        }

        let tool_calls = tool_calls.into_values().collect::<Vec<_>>();
        let mut ids = std::collections::HashSet::new();
        for call in &tool_calls {
            if call.id.is_empty()
                || call.function.name.is_empty()
                || call.kind != "function"
                || !ids.insert(&call.id)
            {
                return Err(ApiError::InvalidToolCall(
                    "отсутствует ID/имя, повторяется ID или неизвестен тип".into(),
                ));
            }
        }
        if finish_reason.as_deref() == Some("tool_calls") && tool_calls.is_empty() {
            return Err(ApiError::InvalidToolCall("пустой список вызовов".into()));
        }
        if !tool_calls.is_empty() && finish_reason.as_deref() != Some("tool_calls") {
            return Err(ApiError::InvalidToolCall(
                "вызов не завершён (возможно, исчерпан max_tokens)".into(),
            ));
        }
        let actual_context = usage
            .and_then(|usage| usize::try_from(usage.total_tokens).ok())
            .unwrap_or_default();
        Ok(ApiTurn {
            context: crate::context::ContextReport {
                limit: settings.context_tokens(),
                before: actual_context,
                after: actual_context,
                output_reserve: 0,
                removed_messages: 0,
            },
            content,
            finish_reason,
            usage,
            tool_calls,
            elapsed_ms: elapsed_millis(started_at.elapsed()),
        })
    }

    #[cfg(test)]
    async fn send_request(
        &self,
        history: &[ChatMessage],
        chat_id: Uuid,
        question: &str,
        settings: &Settings,
        stream: bool,
    ) -> Result<Response, ApiError> {
        let system_instruction = settings.effective_system_prompt();
        let mut messages = Vec::with_capacity(history.len() + 2);
        if let Some(system_instruction) = system_instruction.as_deref() {
            messages.push(RequestMessage {
                role: "system",
                content: system_instruction,
            });
        }
        messages.extend(history.iter().map(|message| RequestMessage {
            role: message.role.as_api_str(),
            content: &message.content,
        }));
        messages.push(RequestMessage {
            role: "user",
            content: question,
        });
        let user = chat_id.to_string();

        let request = ChatRequest {
            model: settings.model(),
            messages,
            max_tokens: settings.max_tokens(),
            response_format: settings
                .response_format_enabled()
                .then(structured_response_format),
            stop: settings.stop_sequence(),
            temperature: settings.temperature(),
            user: &user,
            chat_template_kwargs: ChatTemplateKwargs {
                enable_thinking: false,
            },
            stream,
            stream_options: stream.then_some(StreamOptions {
                include_usage: true,
            }),
        };

        self.post(&request).await
    }

    async fn post(&self, request: &impl Serialize) -> Result<Response, ApiError> {
        let response = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&request)
            .send()
            .await
            .map_err(|error| {
                ApiError::Request(format!(
                    "соединение или отправка запроса: {}",
                    describe_reqwest_error(&error)
                ))
            })?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.map_err(|error| {
                ApiError::Request(format!(
                    "чтение тела HTTP-ошибки: {}",
                    describe_reqwest_error(&error)
                ))
            })?;
            return Err(ApiError::Http {
                status,
                message: extract_error_message(&body),
            });
        }

        Ok(response)
    }
}

fn describe_reqwest_error(error: &reqwest::Error) -> String {
    use std::error::Error as _;

    let kind = if error.is_timeout() {
        "таймаут"
    } else if error.is_connect() {
        "ошибка соединения"
    } else if error.is_body() {
        "обрыв тела ответа"
    } else if error.is_decode() {
        "ошибка декодирования"
    } else {
        "транспортная ошибка"
    };
    let mut details = vec![format!("{kind}: {error}")];
    let mut source = error.source();
    while let Some(cause) = source {
        let message = cause.to_string();
        if !details.iter().any(|detail| detail == &message) {
            details.push(message);
        }
        source = cause.source();
    }
    details.join(": ")
}

fn consume_stream_chunk<F>(
    data: &[u8],
    content: &mut String,
    finish_reason: &mut Option<String>,
    usage: &mut Option<TokenUsage>,
    tool_calls: &mut BTreeMap<usize, ToolCall>,
    on_delta: &mut F,
) -> Result<(), ApiError>
where
    F: FnMut(&str) -> io::Result<()>,
{
    let chunk = serde_json::from_slice::<StreamChunk>(data).map_err(ApiError::InvalidStream)?;
    if let Some(chunk_usage) = chunk.usage {
        *usage = Some(chunk_usage.into());
    }

    for choice in chunk.choices {
        if choice.index != 0 {
            continue;
        }
        for delta in choice.delta.tool_calls {
            if delta.index > 255 {
                return Err(ApiError::InvalidToolCall("слишком много вызовов".into()));
            }
            let call = tool_calls.entry(delta.index).or_default();
            if let Some(id) = delta.id {
                call.id.push_str(&id);
            }
            if let Some(kind) = delta.kind {
                call.kind = kind;
            }
            if let Some(function) = delta.function {
                if let Some(name) = function.name {
                    call.function.name.push_str(&name);
                }
                if let Some(arguments) = function.arguments {
                    call.function.arguments.push_str(&arguments);
                }
            }
            if call.function.arguments.len() > 128_000 {
                return Err(ApiError::InvalidToolCall(
                    "аргументы превышают 128 КБ".into(),
                ));
            }
        }
        if let Some(reason) = choice.finish_reason {
            *finish_reason = Some(reason);
        }
        if let Some(delta) = choice.delta.content.filter(|value| !value.is_empty()) {
            on_delta(&delta).map_err(ApiError::Output)?;
            content.push_str(&delta);
        }
    }

    Ok(())
}

pub(crate) fn finish_answer(
    content: String,
    finish_reason: Option<String>,
    settings: &Settings,
    usage: Option<TokenUsage>,
    elapsed_ms: u64,
) -> Result<ApiAnswer, ApiError> {
    let content = content.trim().to_owned();
    if content.is_empty() {
        return Err(missing_content(finish_reason));
    }

    let content = if settings.response_format_enabled() {
        let structured = serde_json::from_str::<StructuredAnswer>(&content)
            .map_err(ApiError::InvalidStructuredOutput)?;
        serde_json::to_string_pretty(&structured).map_err(ApiError::InvalidStructuredOutput)?
    } else {
        content
    };
    let truncated = matches!(finish_reason.as_deref(), Some("length" | "max_tokens"));

    Ok(ApiAnswer {
        content,
        truncated,
        elapsed_ms,
        usage,
    })
}

fn elapsed_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn missing_content(finish_reason: Option<String>) -> ApiError {
    ApiError::MissingContent {
        finish_reason: finish_reason.unwrap_or_else(|| "не указан".to_owned()),
    }
}

#[cfg(test)]
#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<RequestMessage<'a>>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<&'a str>,
    temperature: f32,
    user: &'a str,
    chat_template_kwargs: ChatTemplateKwargs,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
}

#[cfg(test)]
#[derive(Debug, Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[cfg(test)]
#[derive(Debug, Serialize)]
struct ChatTemplateKwargs {
    enable_thinking: bool,
}

#[cfg(test)]
#[derive(Debug, Serialize)]
struct RequestMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<UsagePayload>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ResponseMessage,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResponseMessage {
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<UsagePayload>,
}

#[derive(Debug, Deserialize)]
struct UsagePayload {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokenDetails>,
}

#[derive(Debug, Deserialize)]
struct PromptTokenDetails {
    #[serde(default)]
    cached_tokens: u64,
}

impl From<UsagePayload> for TokenUsage {
    fn from(usage: UsagePayload) -> Self {
        Self {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
            cached_prompt_tokens: usage
                .prompt_tokens_details
                .map_or(0, |details| details.cached_tokens),
        }
    }
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    #[serde(default)]
    index: usize,
    delta: StreamDelta,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StreamDelta {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCallDelta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ApiMessage {
    pub(crate) role: String,
    pub(crate) content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) tool_calls: Vec<ToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_call_id: Option<String>,
}

impl ApiMessage {
    pub(crate) fn text(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: Some(content.into()),
            tool_calls: vec![],
            tool_call_id: None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ApiTurn {
    pub(crate) context: crate::context::ContextReport,
    pub(crate) content: String,
    pub(crate) finish_reason: Option<String>,
    pub(crate) tool_calls: Vec<ToolCall>,
    pub(crate) usage: Option<TokenUsage>,
    pub(crate) elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ToolCall {
    pub(crate) id: String,
    #[serde(rename = "type")]
    pub(crate) kind: String,
    pub(crate) function: ToolFunction,
}

impl Default for ToolCall {
    fn default() -> Self {
        Self {
            id: String::new(),
            kind: "function".into(),
            function: ToolFunction::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct ToolFunction {
    pub(crate) name: String,
    pub(crate) arguments: String,
}

#[derive(Debug, Deserialize)]
struct ToolCallDelta {
    index: usize,
    id: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    function: Option<ToolFunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct ToolFunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Debug, Default)]
struct SseDecoder {
    pending: Vec<u8>,
}

impl SseDecoder {
    fn push(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        self.pending.extend_from_slice(bytes);
        self.take_complete_lines(false)
    }

    fn finish(&mut self) -> Vec<Vec<u8>> {
        self.take_complete_lines(true)
    }

    fn take_complete_lines(&mut self, include_remainder: bool) -> Vec<Vec<u8>> {
        let mut data_lines = Vec::new();

        while let Some(line_end) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line = self.pending.drain(..=line_end).collect::<Vec<_>>();
            if let Some(data) = parse_sse_data_line(&line) {
                data_lines.push(data);
            }
        }

        if include_remainder && !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            if let Some(data) = parse_sse_data_line(&line) {
                data_lines.push(data);
            }
        }

        data_lines
    }
}

fn parse_sse_data_line(line: &[u8]) -> Option<Vec<u8>> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let data = line.strip_prefix(b"data:")?;
    Some(data.strip_prefix(b" ").unwrap_or(data).to_vec())
}

#[derive(Debug, Deserialize, Serialize)]
struct StructuredAnswer {
    short_answer: String,
    details: Vec<String>,
    conclusion: String,
}

#[derive(Debug, Deserialize)]
struct ErrorResponse {
    detail: Option<Value>,
    error: Option<OpenAiError>,
}

#[derive(Debug, Deserialize)]
struct OpenAiError {
    message: Option<String>,
}

fn extract_error_message(body: &str) -> String {
    if let Ok(error) = serde_json::from_str::<ErrorResponse>(body) {
        if let Some(message) = error.error.and_then(|error| error.message) {
            return message;
        }

        if let Some(detail) = error.detail {
            return match detail {
                Value::String(message) => message,
                other => other.to_string(),
            };
        }
    }

    let body = body.trim();
    if body.is_empty() {
        "ответ не содержит описания ошибки".to_owned()
    } else {
        body.chars().take(500).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc::{self, Receiver};
    use std::thread::{self, JoinHandle};

    use super::*;
    use crate::input::BufferedInput;

    #[tokio::test]
    async fn sends_large_context_to_provider_without_local_token_guessing() {
        let server =
            crate::test_http::MockServer::new(|_| crate::test_http::text_response("Ответ"));
        let client = test_client(server.url.clone());
        let messages = vec![
            ApiMessage::text("user", "x".repeat(300_000)),
            ApiMessage::text("assistant", "Ответ"),
            ApiMessage::text("user", "Новый вопрос"),
        ];
        let turn = client
            .complete_streaming(&messages, Uuid::nil(), &Settings::default(), None, |_| {
                Ok(())
            })
            .await
            .expect("provider decides whether context fits");
        assert_eq!(turn.content, "Ответ");
        assert_eq!(server.requests().len(), 1);
        assert_eq!(messages.len(), 3);
    }

    #[tokio::test]
    async fn assembles_interleaved_fragmented_tool_calls_and_preserves_usage() {
        let server = crate::test_http::MockServer::new(|_| {
            let chunks = [
                json!({"choices":[{"index":0,"delta":{"tool_calls":[
                    {"index":1,"id":"call_b","type":"function","function":{"name":"delegate_","arguments":"{\"handle\":"}},
                    {"index":0,"id":"call_a","type":"function","function":{"name":"delegate_task","arguments":"{\"handle\":\"aa\","}}
                ]}}]}),
                json!({"choices":[{"index":0,"delta":{"tool_calls":[
                    {"index":0,"function":{"arguments":"\"task\":\"Проверь\"}"}},
                    {"index":1,"function":{"name":"task","arguments":"\"bb\",\"task\":\"Сравни\"}"}}
                ]},"finish_reason":"tool_calls"}]}),
                json!({"choices":[],"usage":{"prompt_tokens":30,"completion_tokens":10,"total_tokens":40}}),
            ];
            (
                200,
                chunks
                    .iter()
                    .map(|chunk| format!("data: {chunk}\n\n"))
                    .collect::<String>()
                    + "data: [DONE]\n\n",
            )
        });
        let turn = test_client(server.url.clone())
            .complete_streaming(
                &[ApiMessage::text("user", "Вопрос")],
                Uuid::nil(),
                &Settings::default(),
                Some(json!([])),
                |_| Ok(()),
            )
            .await
            .expect("turn");
        assert_eq!(turn.tool_calls.len(), 2);
        assert_eq!(turn.tool_calls[0].id, "call_a");
        assert_eq!(turn.tool_calls[1].id, "call_b");
        for call in &turn.tool_calls {
            assert_eq!(call.function.name, "delegate_task");
            assert!(serde_json::from_str::<Value>(&call.function.arguments).is_ok());
        }
        assert_eq!(turn.usage.expect("usage").total_tokens, 40);
    }

    #[tokio::test]
    async fn rejects_tool_calls_with_missing_duplicate_ids_or_truncated_arguments() {
        for case in 0..3 {
            let server = crate::test_http::MockServer::new(move |_| {
                let mut calls = vec![crate::test_http::delegate(0, "call_a", "aa")];
                if case == 0 {
                    calls[0]["id"] = json!("");
                }
                if case == 1 {
                    calls.push(crate::test_http::delegate(1, "call_a", "aa"));
                }
                let (_, mut body) = crate::test_http::tool_response(calls);
                if case == 2 {
                    body = body.replace(
                        "\"finish_reason\":\"tool_calls\"",
                        "\"finish_reason\":\"length\"",
                    );
                }
                (200, body)
            });
            let result = test_client(server.url.clone())
                .complete_streaming(&[], Uuid::nil(), &Settings::default(), None, |_| Ok(()))
                .await;
            assert!(
                matches!(result, Err(ApiError::InvalidToolCall(_))),
                "case {case}"
            );
        }
    }

    #[tokio::test]
    async fn sends_chat_request_and_returns_content() {
        let response_body = r#"{"choices":[{"message":{"content":"{\"conclusion\":\"Лось живёт в северных лесах.\",\"details\":[\"Это крупнейший представитель семейства оленевых.\"],\"short_answer\":\"Лось — крупное млекопитающее.\"}"},"finish_reason":"stop"}],"usage":{"prompt_tokens":120,"completion_tokens":45,"total_tokens":165,"prompt_tokens_details":{"cached_tokens":20}}}"#;
        let (base_url, request_rx, server) = spawn_server(200, response_body);
        let client = test_client(base_url);
        let mut settings = Settings::default();
        let mut settings_input = BufferedInput::new(Cursor::new(
            "1\n19\n2\n1\n3\n750\n4\n0,75\n5\n2\n<END>\nesc\n",
        ));
        settings
            .configure(&mut settings_input, &mut Vec::new())
            .expect("settings should be configured");

        let answer = client
            .ask(&[], Uuid::nil(), "Что такое ownership?", &settings)
            .await
            .expect("request should succeed");
        let request = request_rx.recv().expect("request should be captured");
        server.join().expect("mock server should stop");

        assert_eq!(
            answer.content,
            r#"{
  "short_answer": "Лось — крупное млекопитающее.",
  "details": [
    "Это крупнейший представитель семейства оленевых."
  ],
  "conclusion": "Лось живёт в северных лесах."
}"#
        );
        assert!(!answer.truncated);
        assert_eq!(
            answer.usage,
            Some(TokenUsage {
                prompt_tokens: 120,
                completion_tokens: 45,
                total_tokens: 165,
                cached_prompt_tokens: 20,
            })
        );
        assert!(request.starts_with("POST /chat/completions HTTP/1.1\r\n"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer test-key")
        );

        let body = request
            .split_once("\r\n\r\n")
            .expect("request should contain a body")
            .1;
        let body: Value = serde_json::from_str(body).expect("body should be valid JSON");
        assert_eq!(body["model"], "deepseek-v4-pro");
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(1));
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "Что такое ownership?");
        assert_eq!(body["max_tokens"], 750);
        assert_eq!(body["temperature"], 0.75);
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
        assert_eq!(
            body["response_format"]["json_schema"]["schema"]["required"],
            json!(["short_answer", "details", "conclusion"])
        );
        assert_eq!(
            body["response_format"]["json_schema"]["schema"]["additionalProperties"],
            false
        );
        assert_eq!(
            body["response_format"]["json_schema"]["schema"]["properties"]["details"]["minItems"],
            1
        );
        assert_eq!(body["stop"], "<END>");
        assert_eq!(body["user"], Uuid::nil().to_string());
        assert_eq!(body["stream"], false);
    }

    #[tokio::test]
    async fn streams_sse_deltas_and_returns_complete_answer() {
        let response_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Привет, \"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"мир!\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":null},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":4,\"total_tokens\":14}}\n\n",
            "data: [DONE]\n\n",
        );
        let (base_url, request_rx, server) = spawn_server(200, response_body);
        let settings = Settings::default();
        let mut streamed = String::new();

        let answer = test_client(base_url)
            .ask_streaming(&[], Uuid::nil(), "test", &settings, |delta| {
                streamed.push_str(delta);
                Ok(())
            })
            .await
            .expect("stream should succeed");
        let request = request_rx.recv().expect("request should be captured");
        server.join().expect("mock server should stop");

        assert_eq!(streamed, "Привет, мир!");
        assert_eq!(answer.content, "Привет, мир!");
        assert!(!answer.truncated);
        assert_eq!(
            answer.usage,
            Some(TokenUsage {
                prompt_tokens: 10,
                completion_tokens: 4,
                total_tokens: 14,
                cached_prompt_tokens: 0,
            })
        );
        let body = request
            .split_once("\r\n\r\n")
            .expect("request should contain a body")
            .1;
        let body: Value = serde_json::from_str(body).expect("body should be valid JSON");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[tokio::test]
    async fn retries_stream_once_when_body_breaks_before_first_byte() {
        let complete = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Ответ после повтора\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let (base_url, request_rx, server) =
            spawn_stream_sequence(&[(1024, ""), (complete.len(), complete)]);

        let answer = test_client(base_url)
            .ask_streaming(&[], Uuid::nil(), "test", &Settings::default(), |_| Ok(()))
            .await
            .expect("second stream should succeed");
        assert_eq!(answer.content, "Ответ после повтора");
        request_rx.recv().expect("first request");
        request_rx.recv().expect("retry request");
        server.join().expect("mock server should stop");
    }

    #[tokio::test]
    async fn reports_stream_stage_progress_and_cause_after_retry_fails() {
        let (base_url, request_rx, server) = spawn_stream_sequence(&[(1024, ""), (1024, "")]);

        let error = test_client(base_url)
            .ask_streaming(&[], Uuid::nil(), "test", &Settings::default(), |_| Ok(()))
            .await
            .expect_err("both streams should fail");
        let message = error.to_string();
        assert!(message.contains("чтение потокового ответа после 0 байт"));
        assert!(message.contains("повтор до начала потока уже выполнен"));
        assert!(message.contains("error decoding response body"));
        request_rx.recv().expect("first request");
        request_rx.recv().expect("retry request");
        server.join().expect("mock server should stop");
    }

    #[tokio::test]
    async fn sends_full_chat_history_custom_prompt_and_session_id() {
        let response_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Продолжение\"},\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n",
        );
        let (base_url, request_rx, server) = spawn_server(200, response_body);
        let client = test_client(base_url);
        let chat_id = Uuid::new_v4();
        let history = vec![
            ChatMessage {
                role: crate::chat::MessageRole::User,
                content: "Первый вопрос".to_owned(),
                metrics: None,
            },
            ChatMessage {
                role: crate::chat::MessageRole::Assistant,
                content: "Первый ответ".to_owned(),
                metrics: None,
            },
        ];
        let mut settings = Settings::default();
        let mut settings_input =
            BufferedInput::new(Cursor::new("6\n1\nТолько мой системный prompt\nesc\n"));
        settings
            .configure(&mut settings_input, &mut Vec::new())
            .expect("custom prompt should be configured");

        client
            .ask_streaming(&history, chat_id, "Следующий вопрос", &settings, |_| Ok(()))
            .await
            .expect("stream should succeed");
        let request = request_rx.recv().expect("request should be captured");
        server.join().expect("mock server should stop");
        let body = request
            .split_once("\r\n\r\n")
            .expect("request should contain a body")
            .1;
        let body: Value = serde_json::from_str(body).expect("body should be valid JSON");

        assert_eq!(body["user"], chat_id.to_string());
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(4));
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(
            body["messages"][0]["content"],
            "Только мой системный prompt"
        );
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "Первый вопрос");
        assert_eq!(body["messages"][2]["role"], "assistant");
        assert_eq!(body["messages"][2]["content"], "Первый ответ");
        assert_eq!(body["messages"][3]["role"], "user");
        assert_eq!(body["messages"][3]["content"], "Следующий вопрос");
    }

    #[tokio::test]
    async fn omits_system_prompt_from_request_by_default() {
        let response_body =
            r#"{"choices":[{"message":{"content":"Ответ"},"finish_reason":"stop"}]}"#;
        let (base_url, request_rx, server) = spawn_server(200, response_body);
        let settings = Settings::default();

        test_client(base_url)
            .ask(&[], Uuid::nil(), "Вопрос без системного prompt", &settings)
            .await
            .expect("request should succeed");
        let request = request_rx.recv().expect("request should be captured");
        server.join().expect("mock server should stop");
        let body = request
            .split_once("\r\n\r\n")
            .expect("request should contain a body")
            .1;
        let body: Value = serde_json::from_str(body).expect("body should be valid JSON");

        assert_eq!(body["messages"].as_array().map(Vec::len), Some(1));
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(
            body["messages"][0]["content"],
            "Вопрос без системного prompt"
        );
        assert_eq!(body["temperature"], 0.1);
    }

    #[tokio::test]
    async fn rejects_malformed_or_incomplete_sse() {
        let cases = [
            ("data: not-json\n\ndata: [DONE]\n\n", "malformed"),
            (
                "data: {\"choices\":[{\"delta\":{\"content\":\"часть\"},\"finish_reason\":null}]}\n\n",
                "incomplete",
            ),
        ];

        for (response_body, expected) in cases {
            let (base_url, _request_rx, server) = spawn_server(200, response_body);
            let settings = Settings::default();
            let error = test_client(base_url)
                .ask_streaming(&[], Uuid::nil(), "test", &settings, |_| Ok(()))
                .await
                .expect_err("invalid SSE should fail");
            server.join().expect("mock server should stop");

            match expected {
                "malformed" => assert!(matches!(error, ApiError::InvalidStream(_))),
                "incomplete" => {
                    assert!(matches!(error, ApiError::IncompleteStream { .. }))
                }
                _ => panic!("unknown test case"),
            }
        }
    }

    #[test]
    fn decodes_sse_lines_split_across_network_chunks() {
        let mut decoder = SseDecoder::default();

        assert!(decoder.push(b"data: {\"choices\":").is_empty());
        let lines = decoder.push(b"[]}\r\ndata: [DO");
        assert_eq!(lines, [br#"{"choices":[]}"#.to_vec()]);
        assert_eq!(decoder.push(b"NE]\r\n"), [b"[DONE]".to_vec()]);
    }

    #[tokio::test]
    async fn reports_supported_http_errors() {
        let cases = [
            (401, r#"{"error":{"message":"bad key"}}"#, "bad key"),
            (
                429,
                r#"{"detail":"session limit reached"}"#,
                "session limit reached",
            ),
            (500, "internal error", "internal error"),
        ];
        let settings = Settings::default();

        for (status, response_body, expected_message) in cases {
            let (base_url, _request_rx, server) = spawn_server(status, response_body);
            let error = test_client(base_url)
                .ask(&[], Uuid::nil(), "test", &settings)
                .await
                .expect_err("request should fail");
            server.join().expect("mock server should stop");

            match error {
                ApiError::Http {
                    status: actual_status,
                    message,
                } => {
                    assert_eq!(actual_status.as_u16(), status);
                    assert_eq!(message, expected_message);
                }
                other => panic!("unexpected error: {other}"),
            }
        }
    }

    #[tokio::test]
    async fn rejects_invalid_json() {
        let (base_url, _request_rx, server) = spawn_server(200, "not-json");
        let settings = Settings::default();

        let error = test_client(base_url)
            .ask(&[], Uuid::nil(), "test", &settings)
            .await
            .expect_err("invalid JSON should fail");
        server.join().expect("mock server should stop");

        assert!(matches!(error, ApiError::InvalidJson(_)));
    }

    #[tokio::test]
    async fn rejects_response_without_content() {
        for response_body in [
            r#"{"choices":[]}"#,
            r#"{"choices":[{"message":{"content":null}}]}"#,
            r#"{"choices":[{"message":{"content":"   "}}]}"#,
        ] {
            let (base_url, _request_rx, server) = spawn_server(200, response_body);
            let settings = Settings::default();

            let error = test_client(base_url)
                .ask(&[], Uuid::nil(), "test", &settings)
                .await
                .expect_err("missing content should fail");
            server.join().expect("mock server should stop");

            assert!(matches!(error, ApiError::MissingContent { .. }));
        }
    }

    #[tokio::test]
    async fn marks_answer_truncated_by_token_limit() {
        let response_body = r#"{"choices":[{"message":{"content":"Незавершённый ответ"},"finish_reason":"length"}]}"#;
        let (base_url, _request_rx, server) = spawn_server(200, response_body);
        let settings = Settings::default();

        let answer = test_client(base_url)
            .ask(&[], Uuid::nil(), "test", &settings)
            .await
            .expect("partial content should be returned");
        server.join().expect("mock server should stop");

        assert_eq!(answer.content, "Незавершённый ответ");
        assert!(answer.truncated);
    }

    fn test_client(base_url: String) -> NeuralDeepClient {
        NeuralDeepClient::new("test-key".to_owned(), base_url).expect("client should be built")
    }

    fn spawn_server(
        status: u16,
        response_body: &str,
    ) -> (String, Receiver<String>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("mock server should bind");
        let address = listener.local_addr().expect("mock address should exist");
        let response_body = response_body.to_owned();
        let (request_tx, request_rx) = mpsc::channel();

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("mock server should accept");
            let request = read_request(&mut stream);
            request_tx.send(request).expect("request should be sent");

            let reason = match status {
                200 => "OK",
                401 => "Unauthorized",
                429 => "Too Many Requests",
                500 => "Internal Server Error",
                _ => "Test Response",
            };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("mock response should be written");
        });

        (format!("http://{address}"), request_rx, server)
    }

    fn spawn_stream_sequence(
        responses: &[(usize, &str)],
    ) -> (String, Receiver<String>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("mock server should bind");
        let address = listener.local_addr().expect("mock address should exist");
        let responses = responses
            .iter()
            .map(|(length, body)| (*length, (*body).to_owned()))
            .collect::<Vec<_>>();
        let (request_tx, request_rx) = mpsc::channel();

        let server = thread::spawn(move || {
            for (content_length, body) in responses {
                let (mut stream, _) = listener.accept().expect("mock server should accept");
                let request = read_request(&mut stream);
                request_tx.send(request).expect("request should be sent");
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n{body}"
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("mock response should be written");
            }
        });

        (format!("http://{address}"), request_rx, server)
    }

    fn read_request(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];

        loop {
            let read = stream.read(&mut buffer).expect("request should be read");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);

            if request_is_complete(&request) {
                break;
            }
        }

        String::from_utf8(request).expect("request should be UTF-8")
    }

    fn request_is_complete(request: &[u8]) -> bool {
        let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
            return false;
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);

        request.len() >= header_end + 4 + content_length
    }
}
