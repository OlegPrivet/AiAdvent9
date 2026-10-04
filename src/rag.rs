use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::StatusCode;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

use crate::rag_chunk::{self, Strategy};
use crate::rag_extract;

const DEFAULT_MODEL: &str = "bge-m3";
const DEFAULT_DIMENSION: usize = 1024;
const DEFAULT_PROFILE_ID: &str = "neuraldeep-bge-m3-v1";

#[derive(Debug, Clone)]
pub(crate) struct EmbeddingConfig {
    pub(crate) url: String,
    pub(crate) model: String,
    pub(crate) dimensions: usize,
    pub(crate) api_key_env: Option<String>,
    profile_id: String,
}

impl EmbeddingConfig {
    fn default_for(base_url: &str) -> Self {
        Self {
            url: format!("{}/embeddings", base_url.trim_end_matches('/')),
            model: DEFAULT_MODEL.into(),
            dimensions: DEFAULT_DIMENSION,
            api_key_env: Some("NEURALDEEP_API_KEY".into()),
            profile_id: DEFAULT_PROFILE_ID.into(),
        }
    }

    fn custom(url: String, model: String, dimensions: usize, api_key_env: Option<String>) -> Self {
        let fingerprint = Sha256::digest(format!("{url}\n{model}\n{dimensions}"));
        Self {
            url,
            model,
            dimensions,
            api_key_env,
            profile_id: format!("{fingerprint:x}"),
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum RagError {
    #[error("ошибка файла: {0}")]
    Io(#[from] std::io::Error),
    #[error("некорректный JSON RAG: {0}")]
    Json(#[from] serde_json::Error),
    #[error("ошибка индекса: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("ошибка сети: {0}")]
    Network(#[from] reqwest::Error),
    #[error("{0}")]
    Document(String),
    #[error("{0}")]
    Api(String),
}

#[derive(Debug, Clone)]
pub(crate) struct RagService {
    path: PathBuf,
    pub(crate) api_key: Option<String>,
    pub(crate) base_url: String,
    embedding: EmbeddingConfig,
}

#[derive(Debug, Clone)]
pub(crate) struct Source {
    pub(crate) id: String,
    pub(crate) path: String,
    pub(crate) title: String,
    pub(crate) words: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct Hit {
    pub(crate) source: String,
    pub(crate) title: String,
    pub(crate) section: String,
    pub(crate) chunk_id: String,
    pub(crate) text: String,
    pub(crate) score: f32,
    pub(crate) rerank_score: Option<f32>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Stats {
    pub(crate) sources: usize,
    pub(crate) words: usize,
    pub(crate) chars: usize,
    pub(crate) fixed: usize,
    pub(crate) structure: usize,
}

impl RagService {
    pub(crate) fn open(api_key: Option<String>, base_url: String) -> Result<Self, RagError> {
        let directory = crate::chat::state_directory().ok_or_else(|| {
            RagError::Document("Не удалось определить каталог состояния agi".into())
        })?;
        fs::create_dir_all(&directory)?;
        private_permissions(&directory, true)?;
        let service = Self {
            path: directory.join("rag.sqlite3"),
            api_key,
            base_url: base_url.trim_end_matches('/').to_owned(),
            embedding: EmbeddingConfig::default_for(&base_url),
        };
        let mut service = service;
        let connection = service.connection()?;
        service.embedding = service.load_embedding(&connection)?;
        private_permissions(&service.path, false)?;
        Ok(service)
    }

    #[cfg(test)]
    pub(crate) fn for_test(path: PathBuf, api_key: Option<String>, base_url: String) -> Self {
        let mut service = Self {
            path,
            api_key,
            embedding: EmbeddingConfig::default_for(&base_url),
            base_url,
        };
        let connection = service.connection().expect("test database should open");
        service.embedding = service
            .load_embedding(&connection)
            .expect("test embedding config should load");
        service
    }

    fn connection(&self) -> Result<Connection, RagError> {
        let connection = Connection::open(&self.path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS sources (
                id TEXT PRIMARY KEY,
                path TEXT NOT NULL UNIQUE,
                title TEXT NOT NULL,
                format TEXT NOT NULL,
                checksum TEXT NOT NULL,
                words INTEGER NOT NULL,
                chars INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS chunks (
                chunk_id TEXT PRIMARY KEY,
                source_id TEXT NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
                strategy TEXT NOT NULL,
                ordinal INTEGER NOT NULL,
                section TEXT NOT NULL,
                text TEXT NOT NULL,
                model TEXT NOT NULL,
                vector BLOB NOT NULL,
                profile_id TEXT NOT NULL DEFAULT 'neuraldeep-bge-m3-v1'
             );
             CREATE TABLE IF NOT EXISTS embedding_config (
                id INTEGER PRIMARY KEY CHECK(id=1),
                url TEXT NOT NULL,
                model TEXT NOT NULL,
                dimensions INTEGER NOT NULL,
                api_key_env TEXT,
                profile_id TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_rag_chunks_strategy ON chunks(strategy, source_id);",
        )?;
        let has_chars: i64 = connection.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('sources') WHERE name='chars'",
            [],
            |row| row.get(0),
        )?;
        if has_chars == 0 {
            connection.execute(
                "ALTER TABLE sources ADD COLUMN chars INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        let has_profile: i64 = connection.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('chunks') WHERE name='profile_id'",
            [],
            |row| row.get(0),
        )?;
        if has_profile == 0 {
            connection.execute(
                "ALTER TABLE chunks ADD COLUMN profile_id TEXT NOT NULL DEFAULT 'neuraldeep-bge-m3-v1'",
                [],
            )?;
        }
        Ok(connection)
    }

    fn load_embedding(&self, connection: &Connection) -> Result<EmbeddingConfig, RagError> {
        let stored = connection
            .query_row(
                "SELECT url,model,dimensions,api_key_env,profile_id FROM embedding_config WHERE id=1",
                [],
                |row| {
                    Ok(EmbeddingConfig {
                        url: row.get(0)?,
                        model: row.get(1)?,
                        dimensions: row.get::<_, i64>(2)? as usize,
                        api_key_env: row.get(3)?,
                        profile_id: row.get(4)?,
                    })
                },
            )
            .optional()?;
        Ok(stored.unwrap_or_else(|| EmbeddingConfig::default_for(&self.base_url)))
    }

    pub(crate) fn embedding_config(&self) -> &EmbeddingConfig {
        &self.embedding
    }

    pub(crate) fn stale_chunks(&self) -> Result<usize, RagError> {
        let connection = self.connection()?;
        self.ensure_active(&connection)?;
        Ok(connection.query_row(
            "SELECT COUNT(*) FROM chunks WHERE profile_id != ?1",
            [&self.embedding.profile_id],
            |row| row.get::<_, i64>(0),
        )? as usize)
    }

    fn ensure_active(&self, connection: &Connection) -> Result<(), RagError> {
        if self.load_embedding(connection)?.profile_id != self.embedding.profile_id {
            return Err(RagError::Document(
                "Настройка эмбеддингов изменилась; повторите команду".into(),
            ));
        }
        Ok(())
    }

    pub(crate) async fn configure_embeddings(
        &mut self,
        url: String,
        model: String,
        api_key_env: Option<String>,
    ) -> Result<usize, RagError> {
        validate_embedding_options(&url, &model, api_key_env.as_deref())?;
        let url = url.trim_end_matches('/').to_owned();
        let model = model.trim().to_owned();
        let mut candidate = EmbeddingConfig::custom(url, model, 1, api_key_env);
        let vectors = self
            .embed_with(&candidate, &["проверка эмбеддингов"], None)
            .await?;
        candidate.dimensions = vectors[0].len();
        candidate.profile_id = EmbeddingConfig::custom(
            candidate.url.clone(),
            candidate.model.clone(),
            candidate.dimensions,
            candidate.api_key_env.clone(),
        )
        .profile_id;
        let default = EmbeddingConfig::default_for(&self.base_url);
        if candidate.url == default.url
            && candidate.model == default.model
            && candidate.dimensions == default.dimensions
        {
            candidate.profile_id = default.profile_id;
        }
        self.save_embedding(candidate)
    }

    pub(crate) fn reset_embeddings(&mut self) -> Result<usize, RagError> {
        self.save_embedding(EmbeddingConfig::default_for(&self.base_url))
    }

    fn save_embedding(&mut self, embedding: EmbeddingConfig) -> Result<usize, RagError> {
        let connection = self.connection()?;
        self.ensure_active(&connection)?;
        connection.execute(
            "INSERT INTO embedding_config(id,url,model,dimensions,api_key_env,profile_id)
             VALUES(1,?1,?2,?3,?4,?5)
             ON CONFLICT(id) DO UPDATE SET url=excluded.url,model=excluded.model,
             dimensions=excluded.dimensions,api_key_env=excluded.api_key_env,profile_id=excluded.profile_id",
            params![embedding.url, embedding.model, embedding.dimensions as i64,
                    embedding.api_key_env, embedding.profile_id],
        )?;
        self.embedding = embedding;
        self.stale_chunks()
    }

    pub(crate) fn list(&self) -> Result<Vec<Source>, RagError> {
        let connection = self.connection()?;
        let mut statement =
            connection.prepare("SELECT id, path, title, words FROM sources ORDER BY path")?;
        let rows = statement.query_map([], |row| {
            Ok(Source {
                id: row.get(0)?,
                path: row.get(1)?,
                title: row.get(2)?,
                words: row.get::<_, i64>(3)?.max(0) as usize,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub(crate) fn stats(&self) -> Result<Stats, RagError> {
        let connection = self.connection()?;
        let (sources, words, chars): (i64, i64, i64) = connection.query_row(
            "SELECT COUNT(*), COALESCE(SUM(words), 0), COALESCE(SUM(chars), 0) FROM sources",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let count = |strategy: &str| -> Result<usize, RagError> {
            Ok(connection.query_row(
                "SELECT COUNT(*) FROM chunks WHERE strategy=?1",
                [strategy],
                |row| row.get::<_, i64>(0),
            )? as usize)
        };
        Ok(Stats {
            sources: sources as usize,
            words: words as usize,
            chars: chars as usize,
            fixed: count("fixed")?,
            structure: count("structure")?,
        })
    }

    pub(crate) fn corpus_fingerprint(&self) -> Result<String, RagError> {
        let connection = self.connection()?;
        let mut statement =
            connection.prepare("SELECT path, checksum FROM sources ORDER BY path")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut fingerprint = Sha256::new();
        fingerprint.update(self.embedding.profile_id.as_bytes());
        for row in rows {
            let (path, checksum) = row?;
            let actual = format!("{:x}", Sha256::digest(fs::read(&path)?));
            if checksum != actual {
                return Err(RagError::Document(format!(
                    "Источник изменился после индексирования: {path}. Выполните /rag refresh"
                )));
            }
            fingerprint.update(path.as_bytes());
            fingerprint.update([0]);
            fingerprint.update(checksum.as_bytes());
            fingerprint.update([0]);
        }
        Ok(format!("{:x}", fingerprint.finalize()))
    }

    pub(crate) fn lengths(&self, strategy: Strategy) -> Result<Vec<usize>, RagError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare("SELECT text FROM chunks WHERE strategy=?1")?;
        let texts = statement.query_map([strategy.as_str()], |row| row.get::<_, String>(0))?;
        texts
            .map(|text| text.map(|text| text.chars().count()).map_err(Into::into))
            .collect()
    }

    pub(crate) fn remove(&self, id: &str) -> Result<bool, RagError> {
        let connection = self.connection()?;
        Ok(connection.execute("DELETE FROM sources WHERE id=?1", [id])? > 0)
    }

    pub(crate) fn collect_paths(paths: &[PathBuf]) -> Result<Vec<PathBuf>, RagError> {
        let mut result = Vec::new();
        for path in paths {
            collect(path, &mut result, true)?;
        }
        result.sort();
        result.dedup();
        Ok(result)
    }

    pub(crate) async fn add(&self, path: &Path) -> Result<(usize, usize), RagError> {
        let path = fs::canonicalize(path)?;
        let checksum = format!("{:x}", Sha256::digest(fs::read(&path)?));
        let document = rag_extract::extract(
            &path,
            self.api_key.as_deref().unwrap_or_default(),
            &self.base_url,
        )
        .await?;
        let words = document.text.split_whitespace().count();
        let chars = document.text.chars().count();
        let fixed = rag_chunk::split(&document.text, &document.title, Strategy::Fixed);
        let structure = rag_chunk::split(&document.text, &document.title, Strategy::Structure);
        let all = fixed.into_iter().chain(structure).collect::<Vec<_>>();
        if all.is_empty() {
            return Err(RagError::Document(format!(
                "{}: нет чанков",
                path.display()
            )));
        }
        let vectors = self
            .embed_many(
                &all.iter()
                    .map(|chunk| chunk.text.as_str())
                    .collect::<Vec<_>>(),
            )
            .await?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        self.ensure_active(&transaction)?;
        let path_text = path.to_string_lossy().into_owned();
        let existing: Option<String> = transaction
            .query_row(
                "SELECT id FROM sources WHERE path=?1",
                [&path_text],
                |row| row.get(0),
            )
            .optional()?;
        let id = existing.unwrap_or_else(|| Uuid::new_v4().to_string());
        transaction.execute(
            "INSERT INTO sources(id,path,title,format,checksum,words,chars) VALUES(?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(path) DO UPDATE SET title=excluded.title,format=excluded.format,
             checksum=excluded.checksum,words=excluded.words,chars=excluded.chars",
            params![
                id,
                path_text,
                document.title,
                document.format,
                checksum,
                words as i64,
                chars as i64
            ],
        )?;
        transaction.execute("DELETE FROM chunks WHERE source_id=?1", [&id])?;
        for (chunk, vector) in all.iter().zip(vectors) {
            let chunk_id = format!("{}:{}:{}", id, chunk.strategy.as_str(), chunk.ordinal);
            transaction.execute(
                "INSERT INTO chunks(chunk_id,source_id,strategy,ordinal,section,text,model,vector,profile_id)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    chunk_id,
                    id,
                    chunk.strategy.as_str(),
                    chunk.ordinal as i64,
                    chunk.section,
                    chunk.text,
                    self.embedding.model,
                    encode(&vector),
                    self.embedding.profile_id
                ],
            )?;
        }
        transaction.commit()?;
        Ok((words, all.len()))
    }

    pub(crate) async fn refresh(&self) -> Result<Vec<String>, RagError> {
        let mut messages = Vec::new();
        let sources: Vec<(String, String, bool)> = {
            let connection = self.connection()?;
            let mut statement = connection.prepare(
                "SELECT s.path,s.checksum, EXISTS(SELECT 1 FROM chunks c WHERE c.source_id=s.id AND c.profile_id!=?1)
                 FROM sources s ORDER BY s.path",
            )?;
            statement
                .query_map([&self.embedding.profile_id], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get::<_, bool>(2)?))
                })?
                .collect::<Result<_, _>>()?
        };
        for (path, checksum, stale) in sources {
            let path = PathBuf::from(path);
            let current = match fs::read(&path) {
                Ok(bytes) => format!("{:x}", Sha256::digest(bytes)),
                Err(error) => {
                    messages.push(format!("{}: {error}", path.display()));
                    continue;
                }
            };
            if current == checksum && !stale {
                continue;
            }
            match self.add(&path).await {
                Ok((words, chunks)) => messages.push(format!(
                    "Обновлено {}: {words} слов, {chunks} чанков",
                    path.display()
                )),
                Err(error) => messages.push(format!("{}: {error}", path.display())),
            }
        }
        if messages.is_empty() {
            messages.push("Изменённых документов нет".into());
        }
        Ok(messages)
    }

    pub(crate) async fn reindex(&self) -> Result<Vec<String>, RagError> {
        let sources = self.list()?;
        if sources.is_empty() {
            return Ok(vec!["Документов для переиндексации нет".into()]);
        }
        let mut messages = Vec::new();
        for source in sources {
            match self.add(Path::new(&source.path)).await {
                Ok((words, chunks)) => messages.push(format!(
                    "Переиндексировано {}: {words} слов, {chunks} чанков",
                    source.path
                )),
                Err(error) => messages.push(format!("{}: {error}", source.path)),
            }
        }
        let stale = self.stale_chunks()?;
        if stale > 0 {
            messages.push(format!("Осталось чанков прежней модели: {stale}"));
        }
        Ok(messages)
    }

    pub(crate) async fn search(
        &self,
        query: &str,
        strategy: Strategy,
        limit: usize,
    ) -> Result<Vec<Hit>, RagError> {
        let rows = {
            let connection = self.connection()?;
            self.ensure_active(&connection)?;
            let stale: i64 = connection.query_row(
                "SELECT COUNT(*) FROM chunks WHERE profile_id != ?1",
                [&self.embedding.profile_id],
                |row| row.get(0),
            )?;
            if stale > 0 {
                return Err(RagError::Document(format!(
                    "Чанков прежней модели: {stale}. Выполните: agi rag reindex"
                )));
            }
            let mut statement = connection.prepare(
                "SELECT s.path,s.title,c.section,c.chunk_id,c.text,c.vector FROM chunks c
                 JOIN sources s ON s.id=c.source_id WHERE c.strategy=?1 AND c.profile_id=?2",
            )?;
            let rows = statement.query_map(
                params![strategy.as_str(), self.embedding.profile_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Vec<u8>>(5)?,
                    ))
                },
            )?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        if rows.is_empty() {
            return Err(RagError::Document(
                "Индекс пуст. Добавьте документы: agi rag add PATH".into(),
            ));
        }
        let query_vector = self.embed_many(&[query]).await?.remove(0);
        let mut hits = rows
            .into_iter()
            .map(|(source, title, section, chunk_id, text, bytes)| {
                let vector = decode(&bytes, self.embedding.dimensions).ok_or_else(|| {
                    RagError::Document(format!("Повреждён вектор чанка {chunk_id}"))
                })?;
                let score = vector
                    .iter()
                    .zip(&query_vector)
                    .map(|(left, right)| left * right)
                    .sum();
                Ok(Hit {
                    rerank_score: None,
                    source,
                    title,
                    section,
                    chunk_id,
                    text,
                    score,
                })
            })
            .collect::<Result<Vec<_>, RagError>>()?;
        hits.sort_by(|left, right| right.score.total_cmp(&left.score));
        hits.truncate(limit);
        Ok(hits)
    }

    async fn embed_many(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, RagError> {
        self.embed_with(&self.embedding, texts, Some(self.embedding.dimensions))
            .await
    }

    async fn embed_with(
        &self,
        embedding: &EmbeddingConfig,
        texts: &[&str],
        dimensions: Option<usize>,
    ) -> Result<Vec<Vec<f32>>, RagError> {
        let key = embedding
            .api_key_env
            .as_deref()
            .map(|name| {
                if name == "NEURALDEEP_API_KEY" {
                    self.api_key.clone().or_else(|| std::env::var(name).ok())
                } else {
                    std::env::var(name).ok()
                }
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| RagError::Api(format!("Для эмбеддингов задайте {name}")))
            })
            .transpose()?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()?;
        let mut result: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
        for batch in texts.chunks(16) {
            for attempt in 0..3 {
                let mut request = client
                    .post(&embedding.url)
                    .json(&serde_json::json!({"model":embedding.model,"input":batch}));
                if let Some(key) = &key {
                    request = request.bearer_auth(key);
                }
                let response = request.send().await?;
                let status = response.status();
                if status.is_success() {
                    let payload: EmbeddingResponse = response.json().await?;
                    let vectors = validate_embeddings(payload, batch.len(), dimensions)?;
                    if let Some(first) = result.first()
                        && vectors
                            .first()
                            .is_some_and(|vector| vector.len() != first.len())
                    {
                        return Err(RagError::Api(
                            "Сервис вернул эмбеддинги разной размерности".into(),
                        ));
                    }
                    result.extend(vectors);
                    break;
                }
                if attempt == 2
                    || !(status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
                {
                    return Err(RagError::Api(format!(
                        "сервис эмбеддингов вернул HTTP {status}"
                    )));
                }
                let delay = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(1)
                    .min(30);
                tokio::time::sleep(Duration::from_secs(delay)).await;
            }
        }
        Ok(result)
    }
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingItem>,
}

#[derive(Deserialize)]
struct EmbeddingItem {
    index: usize,
    embedding: Vec<f32>,
}

fn validate_embeddings(
    response: EmbeddingResponse,
    count: usize,
    dimension: Option<usize>,
) -> Result<Vec<Vec<f32>>, RagError> {
    let mut vectors = vec![None; count];
    let inferred = response.data.first().map(|item| item.embedding.len());
    let dimension = dimension.or(inferred).unwrap_or(0);
    if dimension == 0 || dimension > 16_384 {
        return Err(RagError::Api("Некорректная размерность эмбеддинга".into()));
    }
    for item in response.data {
        if item.index >= count || vectors[item.index].is_some() || item.embedding.len() != dimension
        {
            return Err(RagError::Api(
                "Некорректный индекс или размерность эмбеддинга".into(),
            ));
        }
        let norm = item
            .embedding
            .iter()
            .map(|value| value * value)
            .sum::<f32>()
            .sqrt();
        if !norm.is_finite() || norm <= 0.0 || item.embedding.iter().any(|value| !value.is_finite())
        {
            return Err(RagError::Api("Некорректные значения эмбеддинга".into()));
        }
        vectors[item.index] = Some(
            item.embedding
                .into_iter()
                .map(|value| value / norm)
                .collect(),
        );
    }
    vectors
        .into_iter()
        .map(|vector| vector.ok_or_else(|| RagError::Api("Неполный ответ эмбеддингов".into())))
        .collect()
}

fn encode(vector: &[f32]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn decode(bytes: &[u8], dimension: usize) -> Option<Vec<f32>> {
    if bytes.len() != dimension * 4 {
        return None;
    }
    Some(
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|part| f32::from_le_bytes(*part))
            .collect(),
    )
}

fn validate_embedding_options(
    url: &str,
    model: &str,
    api_key_env: Option<&str>,
) -> Result<(), RagError> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|error| RagError::Document(format!("Некорректный URL эмбеддингов: {error}")))?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(RagError::Document(
            "Укажите полный HTTP(S) URL без логина, пароля, query и fragment".into(),
        ));
    }
    if model.trim().is_empty() || model.contains(['\n', '\r']) {
        return Err(RagError::Document("Укажите имя модели эмбеддингов".into()));
    }
    if let Some(name) = api_key_env
        && (name.is_empty()
            || !name.starts_with(|value: char| value == '_' || value.is_ascii_alphabetic())
            || !name
                .chars()
                .all(|value| value == '_' || value.is_ascii_alphanumeric()))
    {
        return Err(RagError::Document(
            "Некорректное имя переменной окружения для API-ключа".into(),
        ));
    }
    Ok(())
}

fn collect(path: &Path, result: &mut Vec<PathBuf>, explicit: bool) -> Result<(), RagError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_file() {
        result.push(path.to_owned());
        return Ok(());
    }
    if !metadata.is_dir() {
        return Ok(());
    }
    if !explicit
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| matches!(name, ".git" | "target" | "node_modules" | ".venv"))
    {
        return Ok(());
    }
    for entry in fs::read_dir(path)? {
        collect(&entry?.path(), result, false)?;
    }
    Ok(())
}

#[cfg(unix)]
fn private_permissions(path: &Path, directory: bool) -> Result<(), RagError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(
        path,
        fs::Permissions::from_mode(if directory { 0o700 } else { 0o600 }),
    )?;
    Ok(())
}

#[cfg(not(unix))]
fn private_permissions(_path: &Path, _directory: bool) -> Result<(), RagError> {
    Ok(())
}

impl RagService {
    pub(crate) async fn context(
        &self,
        question: &str,
        strategy: Strategy,
    ) -> Result<Vec<Hit>, RagError> {
        let hits = self.search(question, strategy, 12).await?;
        Ok(select_context(hits))
    }
}

fn select_context(hits: Vec<Hit>) -> Vec<Hit> {
    select_context_with_limit(hits, crate::config::RAG_CONTEXT_K)
}

pub(crate) fn select_context_with_limit(hits: Vec<Hit>, limit: usize) -> Vec<Hit> {
    let mut chars = 0;
    let mut sections = std::collections::HashSet::new();
    let mut selected = Vec::new();
    for hit in &hits {
        let length = hit.text.chars().count();
        if chars + length > crate::config::RAG_CONTEXT_CHARS
            || !sections.insert((hit.source.clone(), hit.section.clone()))
        {
            continue;
        }
        chars += length;
        selected.push(hit.clone());
        if selected.len() == limit {
            return selected;
        }
    }
    let mut chunk_ids = selected
        .iter()
        .map(|hit| hit.chunk_id.clone())
        .collect::<std::collections::HashSet<_>>();
    for hit in hits {
        let length = hit.text.chars().count();
        if chars + length > crate::config::RAG_CONTEXT_CHARS
            || !chunk_ids.insert(hit.chunk_id.clone())
        {
            continue;
        }
        chars += length;
        selected.push(hit);
        if selected.len() == limit {
            break;
        }
    }
    selected
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, routing::post};
    use serde_json::{Value, json};

    #[test]
    fn validates_embedding_response_indices() {
        let vector = vec![1.0; DEFAULT_DIMENSION];
        let valid = EmbeddingResponse {
            data: vec![
                EmbeddingItem {
                    index: 1,
                    embedding: vector.clone(),
                },
                EmbeddingItem {
                    index: 0,
                    embedding: vector,
                },
            ],
        };
        assert_eq!(
            validate_embeddings(valid, 2, Some(DEFAULT_DIMENSION))
                .expect("vectors")
                .len(),
            2
        );
    }

    #[test]
    fn rejects_incomplete_duplicate_and_invalid_embeddings() {
        let item = |index, embedding| EmbeddingItem { index, embedding };
        let ones = || vec![1.0; DEFAULT_DIMENSION];
        for response in [
            EmbeddingResponse {
                data: vec![item(0, ones())],
            },
            EmbeddingResponse {
                data: vec![item(0, ones()), item(0, ones())],
            },
            EmbeddingResponse {
                data: vec![item(0, vec![1.0])],
            },
            EmbeddingResponse {
                data: vec![item(0, vec![0.0; DEFAULT_DIMENSION])],
            },
            EmbeddingResponse {
                data: vec![item(0, vec![f32::NAN; DEFAULT_DIMENSION])],
            },
        ] {
            assert!(validate_embeddings(response, 2, Some(DEFAULT_DIMENSION)).is_err());
        }
    }

    #[test]
    fn rejects_credentials_in_embedding_url_and_invalid_key_variable() {
        assert!(
            validate_embedding_options("https://user:secret@example.com/v1/embeddings", "m", None)
                .is_err()
        );
        assert!(
            validate_embedding_options(
                "http://localhost:8000/v1/embeddings",
                "m",
                Some("BAD-NAME")
            )
            .is_err()
        );
        assert!(
            validate_embedding_options("http://localhost:8000/v1/embeddings", "m", None).is_ok()
        );
    }

    #[tokio::test]
    async fn arbitrary_document_is_indexed_searched_and_kept_after_failed_update() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock");
        let url = format!("http://{}", listener.local_addr().expect("address"));
        let server = tokio::spawn(async move {
            let router = Router::new().route(
                "/embeddings",
                post(|Json(body): Json<Value>| async move {
                    let input = body["input"].as_array().expect("input array");
                    Json(json!({"data": input.iter().enumerate().map(|(index, _)| {
                        json!({"index":index,"embedding":vec![1.0f32; DEFAULT_DIMENSION]})
                    }).collect::<Vec<_>>() }))
                }),
            );
            axum::serve(listener, router).await.expect("serve mock");
        });
        let directory = std::env::temp_dir().join(format!("agi-rag-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).expect("test directory");
        let document = directory.join("памятка.md");
        fs::write(
            &document,
            "# Заголовок\nХранение чатов в SQLite.\n## Детали\nИндекс RAG локальный.",
        )
        .expect("document");
        let mut service =
            RagService::for_test(directory.join("rag.sqlite3"), Some("test-key".into()), url);
        let (words, chunks) = service.add(&document).await.expect("index file");
        assert!(words > 5);
        assert!(chunks >= 3);
        let stats = service.stats().expect("stats");
        assert_eq!(stats.sources, 1);
        assert!(stats.fixed > 0 && stats.structure > 0);
        let hits = service
            .search("Где хранятся чаты?", Strategy::Structure, 3)
            .await
            .expect("search");
        assert!(!hits.is_empty());
        assert!(hits[0].source.ends_with("памятка.md"));
        service.embedding.url = "http://127.0.0.1:1/embeddings".into();
        fs::write(&document, "Изменённый текст").expect("update document");
        assert!(service.add(&document).await.is_err());
        assert_eq!(service.stats().expect("old stats").words, words);
        server.abort();
        fs::remove_dir_all(directory).expect("cleanup");
    }

    #[tokio::test]
    async fn custom_embedding_model_survives_restart_and_requires_reindex_after_change() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock");
        let url = format!("http://{}", listener.local_addr().expect("address"));
        let server = tokio::spawn(async move {
            let router = Router::new().route(
                "/v1/embeddings",
                post(|Json(body): Json<Value>| async move {
                    let input = body["input"].as_array().expect("input array");
                    Json(json!({"data": input.iter().enumerate().map(|(index, _)| {
                        json!({"index":index,"embedding":[1.0, 2.0, 3.0]})
                    }).collect::<Vec<_>>() }))
                }),
            );
            axum::serve(listener, router).await.expect("serve mock");
        });
        let directory = std::env::temp_dir().join(format!("agi-rag-custom-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).expect("test directory");
        let path = directory.join("notes.md");
        fs::write(
            &path,
            "# Заметки\nСобственная локальная модель эмбеддингов.",
        )
        .expect("write document");
        let database = directory.join("rag.sqlite3");
        let mut service = RagService::for_test(database.clone(), None, url.clone());
        let endpoint = format!("{url}/v1/embeddings");
        assert_eq!(
            service
                .configure_embeddings(endpoint.clone(), "model-one".into(), None)
                .await
                .expect("configure model"),
            0
        );
        assert_eq!(service.embedding_config().dimensions, 3);
        service.add(&path).await.expect("index document");
        assert!(
            !service
                .search("модель", Strategy::Structure, 3)
                .await
                .expect("search with custom model")
                .is_empty()
        );
        drop(service);

        let mut service = RagService::for_test(database, None, url);
        assert_eq!(service.embedding_config().model, "model-one");
        assert_eq!(service.embedding_config().dimensions, 3);
        assert!(
            service
                .configure_embeddings(endpoint, "model-two".into(), None)
                .await
                .expect("change model")
                > 0
        );
        assert!(
            service
                .search("модель", Strategy::Structure, 3)
                .await
                .is_err()
        );
        service.reindex().await.expect("reindex documents");
        assert_eq!(service.stale_chunks().expect("stale chunks"), 0);
        assert!(
            !service
                .search("модель", Strategy::Structure, 3)
                .await
                .expect("search after reindex")
                .is_empty()
        );
        server.abort();
        fs::remove_dir_all(directory).expect("cleanup");
    }

    #[test]
    fn retrieved_chunks_are_separate_from_the_user_question() {
        let chat = crate::chat::Chat::new();
        let request =
            crate::agent::AgentRequest::new(&chat, "Вопрос".into(), Vec::new()).with_rag(vec![
                Hit {
                    source: "notes.md".into(),
                    title: "Заметки".into(),
                    section: "Раздел".into(),
                    chunk_id: "chunk-1".into(),
                    text: "Проверенный факт".into(),
                    score: 0.9,
                    rerank_score: None,
                },
            ]);
        let messages = crate::agent::main_messages(&request);
        assert_eq!(
            messages
                .last()
                .and_then(|message| message.content.as_deref()),
            Some("Вопрос")
        );
        assert!(
            messages
                .iter()
                .any(|message| message.content.as_deref().is_some_and(|text| {
                    text.contains("notes.md") && text.contains("Проверенный факт")
                }))
        );
    }

    #[test]
    fn context_prefers_distinct_sections_and_fills_remaining_slots() {
        let hit = |chunk_id: &str, section: &str| Hit {
            source: "notes.md".into(),
            title: "Заметки".into(),
            section: section.into(),
            chunk_id: chunk_id.into(),
            text: "Текст".into(),
            score: 0.9,
            rerank_score: None,
        };
        let selected = select_context(vec![
            hit("1", "Первый"),
            hit("2", "Первый"),
            hit("3", "Второй"),
            hit("4", "Второй"),
            hit("5", "Третий"),
        ]);
        assert_eq!(
            selected
                .iter()
                .map(|hit| hit.chunk_id.as_str())
                .collect::<Vec<_>>(),
            vec!["1", "3", "5", "2"]
        );
    }
}
