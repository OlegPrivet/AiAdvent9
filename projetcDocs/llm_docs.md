# neuraldeep.ru — LLM API reference (для coding-агентов)

> Полная машиночитаемая справка. Curl-friendly: `curl https://neuraldeep.ru/llms-full.txt`.
> Индекс с указателями: `https://neuraldeep.ru/llms.txt`. Человеческая версия: `https://neuraldeep.ru/docs`.
> Документ один (~умещается в контекст). Секции разделены заголовками `## <name>` — грепай по ним.

Base URL: `https://api.neuraldeep.ru/v1` (OpenAI-совместимый)
Auth: `Authorization: Bearer $YOUR_KEY`

Available models:
- `gpt-oss-120b` — chat · tools · reasoning · 131k ctx · внешний вендор, обработка вне РФ
- `qwen3.8-27b` — chat · qwen3 tools · reasoning · 256k ctx · dense 27B · на RTX PRO 6000 96GB в РФ · vision (4 image/prompt)
- `qwen3.6-35b-a3b` — chat · qwen3 tools · reasoning · 256k ctx · MoE 35B/3B-active · BF16 на 2× RTX 4090 48GB · vision (1 image/prompt)
- `gemma-4-31b` — chat · tools · multimodal (image/video) · 262k ctx · Google Gemma 4 · внешний вендор, обработка вне РФ
- `e5-large` — multilingual embedding · 1024-dim · 3 replicas
- `bge-m3` — multilingual embedding · 1024-dim · 8k ctx
- `bge-reranker` — cross-encoder rerank
- `whisper-1` — WhisperX large-v3-turbo · multilingual · word timestamps · RTF ~16×
- `qwen3-tts` / `espeech` — TTS синтез речи · `/v1/audio/speech` · 8 голосов · 10 языков · RU с ударениями

## Разделы (deep-links на якоря человеческой доки)
Hub API:
- Текст · чат — https://neuraldeep.ru/docs#chat
- Векторизация — https://neuraldeep.ru/docs#vector
- Реранжирование — https://neuraldeep.ru/docs#rerank
- Транскрибация — https://neuraldeep.ru/docs#transcribe
- Распознавание картинок — https://neuraldeep.ru/docs#vision
- Structured output — https://neuraldeep.ru/docs#structured
- Агенты · tools — https://neuraldeep.ru/docs#agents
- Остатки лимитов · Limits API — https://neuraldeep.ru/docs#limits
- Поиск · Search API — https://neuraldeep.ru/docs#search
- OCR · документы — https://neuraldeep.ru/docs#ocr
- Картинки · Image API — https://neuraldeep.ru/docs#images
- Анонимизация ПДн · PII Guard — https://neuraldeep.ru/docs#guardrails
- SpeechCore · STT — https://neuraldeep.ru/docs#speechcore
- Озвучка · TTS API — https://neuraldeep.ru/docs#tts
- Книга · поиск и MCP — https://neuraldeep.ru/docs#book-api

Drift API:
- Drift · обзор — https://neuraldeep.ru/docs#drift-api
- Файлы + sandbox preview — https://neuraldeep.ru/docs#drift-files
- Свои tools, skills, MCP — https://neuraldeep.ru/docs#drift-caller-tools
- Задачи · proactive — https://neuraldeep.ru/docs#drift-tasks
- Конструктор агентов — https://neuraldeep.ru/docs#agent-hosting
- Память · memory — https://neuraldeep.ru/docs#drift-memory
- Изоляция и безопасность — https://neuraldeep.ru/docs#drift-security

Лимиты:
- Настройки агента — https://neuraldeep.ru/docs#agent-config
- Лимиты · ошибки — https://neuraldeep.ru/docs#limits

Справка:
- Стриминг — https://neuraldeep.ru/docs#streaming
- SDK · Python / JS — https://neuraldeep.ru/docs#sdk
- Приватность — https://neuraldeep.ru/docs#privacy

## Текст · Chat completion

OpenAI-совместимый `/v1/chat/completions`. Поддерживает streaming, tools, reasoning (gpt-oss внутри reasoning-теги прокидывает), `user` для session-sticky routing.

```bash
curl https://api.neuraldeep.ru/v1/chat/completions \
  -H "Authorization: Bearer $YOUR_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-oss-120b",
    "messages": [
      {"role":"system","content":"You are helpful."},
      {"role":"user","content":"Привет"}
    ],
    "max_tokens": 500,
    "temperature": 0.2
  }'
```

```python
from openai import OpenAI
client = OpenAI(api_key="$YOUR_KEY", base_url="https://api.neuraldeep.ru/v1")
r = client.chat.completions.create(
    model="gpt-oss-120b",
    messages=[{"role":"user","content":"Привет"}],
    max_tokens=500,
)
print(r.choices[0].message.content)
```

> Шли `user: <session_id>` чтобы держать сессию на одном upstream — KV-cache не сбросится, multi-turn быстрее.

**Reasoning по моделям.** `reasoning_effort` (low/med/high) работает только на `gpt-oss-120b`. На `qwen3.8-27b`/`qwen3.6-35b-a3b` он принимается, но НЕ влияет — у арх Qwen нет маппинга «уровень усилия → бюджет». Управляй так: `chat_template_kwargs: { "thinking_token_budget": N }` — потолок размышления; `{ "enable_thinking": false }` (или алиас `-noreason`) — выключить размышление для быстрых ответов. По умолчанию reasoning ВКЛ, `reasoning_content` приходит отдельным полем.

## Векторизация · Embeddings

OpenAI-совместимый `/v1/embeddings`. Принимает строку или массив. Возвращает 1024-мерные вектора. Deterministic → LiteLLM кеширует автоматом.

```bash
curl https://api.neuraldeep.ru/v1/embeddings \
  -H "Authorization: Bearer $YOUR_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"e5-large","input":"привет мир"}'
```

```bash
curl https://api.neuraldeep.ru/v1/embeddings \
  -H "Authorization: Bearer $YOUR_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"bge-m3","input":["текст 1","текст 2","text 3"]}'
```

```python
from openai import OpenAI
client = OpenAI(api_key="$YOUR_KEY", base_url="https://api.neuraldeep.ru/v1")
texts = ["первый документ", "второй", "third"]
r = client.embeddings.create(model="bge-m3", input=texts)
vectors = [e.embedding for e in r.data]  # list[list[float]], dim=1024
```

> Для RAG — `e5-large` на запросы/доки с префиксом "query: " / "passage: ". Для точности — `bge-m3` (длиннее контекст).

## Реранжирование · Rerank

Пересортировка кандидатов по релевантности к query. Отдаёт `relevance_score` для каждого документа. Используется после векторного поиска для fine-grained ранжирования top-k.

```bash
curl https://api.neuraldeep.ru/v1/rerank \
  -H "Authorization: Bearer $YOUR_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "bge-reranker",
    "query": "Что такое LLM?",
    "documents": [
      "Large language models обучены на больших корпусах.",
      "Сегодня в Москве солнечно.",
      "GPT-4 — это transformer-архитектура OpenAI."
    ]
  }'
```

```python
import httpx
# topk_docs: list[str] полученные векторным поиском (top 50)
r = httpx.post(
    "https://api.neuraldeep.ru/v1/rerank",
    headers={"Authorization": f"Bearer $YOUR_KEY"},
    json={"model": "bge-reranker", "query": query, "documents": topk_docs},
    timeout=15.0,
).json()
# результаты отсортированы по relevance desc
top3 = [topk_docs[x["index"]] for x in r["results"][:3]]
```

> Pipeline: сначала embeddings → ANN (Qdrant/FAISS) → top 50 → rerank → top 3-5 для LLM.

## Транскрибация · Speech-to-Text

OpenAI Whisper API формат. Принимает multipart/form-data с файлом аудио. Возвращает текст + таймкоды + сегменты.

```bash
curl https://api.neuraldeep.ru/v1/audio/transcriptions \
  -H "Authorization: Bearer $YOUR_KEY" \
  -F file=@meeting.wav \
  -F model=whisper-1
```

```python
from openai import OpenAI
client = OpenAI(api_key="$YOUR_KEY", base_url="https://api.neuraldeep.ru/v1")
with open("meeting.wav","rb") as f:
    r = client.audio.transcriptions.create(model="whisper-1", file=f)
print(r.text)
```

> Поддерживаются WAV/MP3/M4A/OGG. Чем короче файл — тем меньше латенси (последовательная обработка).

## Распознавание картинок · Vision

Multimodal через стандартный `/v1/chat/completions`. Передавай `image_url` в content-array (URL или base64). Модель описывает картинку, извлекает текст, отвечает на вопросы по изображению.

```bash
curl https://api.neuraldeep.ru/v1/chat/completions \
  -H "Authorization: Bearer $YOUR_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "qwen3.6-35b-a3b",
    "messages": [{
      "role": "user",
      "content": [
        {"type":"text","text":"Что на картинке? Извлеки текст если есть."},
        {"type":"image_url","image_url":{"url":"data:image/jpeg;base64,/9j/..."}}
      ]
    }],
    "max_tokens": 500
  }'
```

```python
import base64
from openai import OpenAI

with open("photo.jpg","rb") as f:
    b64 = base64.b64encode(f.read()).decode()

client = OpenAI(api_key="$YOUR_KEY", base_url="https://api.neuraldeep.ru/v1")
r = client.chat.completions.create(
    model="qwen3.6-35b-a3b",
    messages=[{
        "role":"user",
        "content":[
            {"type":"text","text":"Опиши кратко что на фото"},
            {"type":"image_url","image_url":{"url":f"data:image/jpeg;base64,{b64}"}},
        ],
    }],
    max_tokens=400,
)
print(r.choices[0].message.content)
```

> Лимит — 1 картинка на запрос. Если нужно несколько — делай N последовательных запросов и склеивай ответы у себя в коде.

## Structured output · JSON / guided grammar

vLLM поддерживает guided JSON, regex и grammar (llguidance). Через OpenAI-совместимый `response_format`.

```bash
curl https://api.neuraldeep.ru/v1/chat/completions \
  -H "Authorization: Bearer $YOUR_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-oss-120b",
    "messages": [{"role":"user","content":"Extract name and age from: Иван, 30 лет"}],
    "response_format": {"type":"json_object"}
  }'
```

```python
from pydantic import BaseModel
from openai import OpenAI

class Person(BaseModel):
    name: str
    age: int

client = OpenAI(api_key="$YOUR_KEY", base_url="https://api.neuraldeep.ru/v1")
r = client.chat.completions.create(
    model="gpt-oss-120b",
    messages=[{"role":"user","content":"Иван, 30 лет"}],
    response_format={"type":"json_schema","json_schema":{
        "name":"Person","schema":Person.model_json_schema(),"strict":True
    }},
)
person = Person.model_validate_json(r.choices[0].message.content)
```

> Для strict JSON — обязательно `strict: true` в schema. vLLM гарантирует соответствие грамматике.

## Агенты · Tool calling

Стандартный OpenAI tool-calling через `tools` + `tool_choice`. Модель решает когда звать инструмент, возвращает JSON с аргументами.

```python
from openai import OpenAI
client = OpenAI(api_key="$YOUR_KEY", base_url="https://api.neuraldeep.ru/v1")

tools = [{
  "type": "function",
  "function": {
    "name": "get_weather",
    "description": "Получает погоду в городе",
    "parameters": {
      "type": "object",
      "properties": {"city": {"type": "string"}},
      "required": ["city"],
    }
  }
}]

messages = [{"role":"user","content":"Какая погода в Москве?"}]
r = client.chat.completions.create(
    model="gpt-oss-120b",
    messages=messages,
    tools=tools,
    tool_choice="auto",
)
# r.choices[0].message.tool_calls → список вызовов
```

```bash
curl https://api.neuraldeep.ru/v1/chat/completions \
  -H "Authorization: Bearer $YOUR_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model":"gpt-oss-120b",
    "messages":[{"role":"user","content":"tell a short story"}],
    "stream": true,
    "max_tokens": 800
  }'
```

> Для многократных агентных итераций используй `user: session_id` чтобы сессия жила на одном upstream — prefix caching экономит токены до 10×.

#### Coding-агенты · opencode

[opencode](https://opencode.ai) — open-source coding-агент в терминале. Работает через стандартный `/v1/chat/completions`, никаких патчей не нужно. Smoke-tested на обеих моделях + tool-calling.

```json
{
  "$schema": "https://opencode.ai/config.json",
  "provider": {
    "neuraldeep": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "NeuralDeep Hub",
      "options": {
        "baseURL": "https://api.neuraldeep.ru/v1",
        "apiKey": "$YOUR_KEY"
      },
      "models": {
        "qwen3.6-35b-a3b": {
          "name": "qwen3.6-35b-a3b · 256k ctx · tools",
          "limit": { "context": 262144, "output": 16384 }
        },
        "gpt-oss-120b": {
          "name": "gpt-oss-120b · 131k ctx · reasoning",
          "limit": { "context": 131072, "output": 8192 }
        }
      }
    }
  }
}
```

```bash
# TUI-режим
opencode

# в TUI смени модель: /model → neuraldeep/qwen3.6-35b-a3b

# non-interactive (one-shot)
opencode run --model neuraldeep/qwen3.6-35b-a3b "find all env vars in src/"
```

#### Coding-агенты · OpenAI Codex CLI

[codex CLI](https://www.npmjs.com/package/@openai/codex) — терминальный coding-агент от OpenAI. Работает через `/v1/responses` (Responses API). На нашей стороне применены 3 файловых патча vLLM для поддержки `custom` tool types и multi-turn валидации (по issue [#33089](https://github.com/vllm-project/vllm/issues/33089)).

```toml
# ----------------- провайдер -----------------
[model_providers.neuraldeep]
name = "NeuralDeep Hub"
base_url = "https://api.neuraldeep.ru/v1"
wire_api = "responses"
experimental_bearer_token = "$YOUR_KEY"
# Альтернатива: env_key = "NEURALDEEP_API_KEY" + export в shell

# ----------------- профили -----------------
[profiles.neuraldeep]
model_provider = "neuraldeep"
model = "qwen3.6-35b-a3b"
# qwen3.6 → 256k ctx, нативные tool-calls, лучше для большой codebase

[profiles.neuraldeep-oss]
model_provider = "neuraldeep"
model = "gpt-oss-120b"
# gpt-oss-120b → 131k ctx, длинный reasoning
```

```bash
# TUI-режим с qwen3.6
codex --profile neuraldeep

# или с gpt-oss-120b
codex --profile neuraldeep-oss

# regулировка reasoning effort
codex --profile neuraldeep -c model_reasoning_effort=low
```

> **wire_api = "chat" больше не поддерживается** в новых codex (с конца 2025). Только `"responses"`. Если видишь ошибку `Error loading config.toml: wire_api = "chat" is no longer supported` — поменяй на responses.

## Остатки лимитов · GET /v1/limits

Сколько осталось от квоты именно у твоего ключа — session/week (чат и векторный класс отдельно), живое окно чат-RPM текущей минуты, cooldown'ы, параллельность, ночной множитель, кошелёк в рублях и процент kimi-бюджета. Тем же `Bearer sk-`, что и инференс. Сделано для агентов и кронов: планировать расход, а не узнавать о лимите из `429`.

```bash
curl -s https://api.neuraldeep.ru/v1/limits \
  -H "Authorization: Bearer $YOUR_KEY" | jq
```

```bash
# сервер уже свёл все блокеры (cooldown'ы, окна, RPM, статус ключа) в decision
curl -s https://api.neuraldeep.ru/v1/limits -H "Authorization: Bearer $YOUR_KEY" \
  | jq -e '.decision.can_request'

# тонкая настройка: тратить только если осталось >20% сессии
curl -s https://api.neuraldeep.ru/v1/limits -H "Authorization: Bearer $YOUR_KEY" \
  | jq -e '.chat.cooldown_sec == 0 and .chat.session.remaining > (.chat.session.limit / 5)'
```

Семантика: `decision` — сводка про chat-инференс (векторный лейн смотри в блоке `vector`); все числа уже эффективные (ночной ×2 применён — ничего не умножай); `remaining` посчитан на сервере и не бывает отрицательным; `key.status` ≠ `ok` означает 401 на инференсе независимо от остатков; при недоступных счётчиках — честный `503` c `Retry-After`. Ручка только читает — опрос квоту не тратит; чаще раза в 15-30 секунд поллить нет смысла.

> Потолки ТАРИФОВ (не персональные остатки) — `GET /api/public/tier-limits` без авторизации.

## Поиск · Search API

Тем же ключом — поиск по Telegram и нашей базе ТГ-каналов, поиск по интернету и краулинг сайтов. Эндпоинты под `/v1/search/*`. Доступно на **всех тарифах, включая free**, с отдельным от чата лимитом.

Две корзины лимитов: **поиск** (ТГ + база каналов + интернет) и **краулинг** (отдельно — он тяжелее). Текущий остаток — `GET /v1/search/quota` или карточка «Search API» в дашборде. При исчерпании — `429` с заголовком `Retry-After`.

```bash
# наша база Telegram-каналов (vector + full-text)
curl "https://api.neuraldeep.ru/v1/search/tg?q=AI-агенты&limit=5" \
  -H "Authorization: Bearer $YOUR_KEY"

# поиск по интернету
curl https://api.neuraldeep.ru/v1/search/web \
  -H "Authorization: Bearer $YOUR_KEY" -H "Content-Type: application/json" \
  -d '{"query":"свежие новости про LLM","limit":3}'
```

```bash
# обойти сайт (несколько страниц)
curl https://api.neuraldeep.ru/v1/search/crawl \
  -H "Authorization: Bearer $YOUR_KEY" -H "Content-Type: application/json" \
  -d '{"url":"https://example.com","limit":3}'

# сколько осталось по обеим корзинам
curl https://api.neuraldeep.ru/v1/search/quota \
  -H "Authorization: Bearer $YOUR_KEY"
```

```python
import httpx

r = httpx.get(
    "https://api.neuraldeep.ru/v1/search/tg",
    params={"q": "RAG в проде", "limit": 5},
    headers={"Authorization": "Bearer $YOUR_KEY"},
)
data = r.json()
for item in data.get("results", []):
    print(item.get("channel"), "—", item.get("text", "")[:120])
print("остаток:", data.get("quota", {}).get("day"))
```

> web-поиск и краулинг тратят реальные деньги провайдеров, поэтому у них отдельная (более скромная) корзина. ТГ-поиск по нашей базе — дешёвый, лимит щедрее.

## OCR · распознавание документов

Тем же ключом — OCR для PDF и картинок (PNG/JPG/WEBP/BMP/TIFF). Эндпоинты под `/v1/ocr/*`. Доступно на **всех тарифах**, с отдельной от чата корзиной — лимит считается в **страницах**.

Работает **асинхронно**: загружаешь документ → получаешь `job` → опрашиваешь статус (не чаще раза в секунду) → забираешь результат в `markdown`/`json`/`text`. Для разметки по координатам есть PNG-preview страницы (та же система координат, что и bbox). Профили: `fast` (по умолчанию) и `pro` (выше качество, считается за 2 страницы). Остаток — `GET /v1/ocr/balance`. При исчерпании — `429` с `Retry-After`.

```bash
# 1. загрузить (вернёт {"id":"job_...","page_count":N})
curl https://api.neuraldeep.ru/v1/ocr/extract \
  -H "Authorization: Bearer $YOUR_KEY" \
  -F "file=@invoice.pdf"
# (повыше качество: добавь -F "model_profile=pro";
#  диапазон страниц: -F 'page_ranges=[{"start":1,"end":5}]')

# 2. статус задачи
curl https://api.neuraldeep.ru/v1/ocr/jobs/job_123 \
  -H "Authorization: Bearer $YOUR_KEY"

# 3. результат в markdown
curl "https://api.neuraldeep.ru/v1/ocr/jobs/job_123/result?format=markdown" \
  -H "Authorization: Bearer $YOUR_KEY"
```

```python
import time, httpx

H = {"Authorization": "Bearer $YOUR_KEY"}
# 1. загрузка
with open("invoice.pdf", "rb") as f:
    job = httpx.post(
        "https://api.neuraldeep.ru/v1/ocr/extract",
        headers=H, files={"file": f},
    ).json()
jid = job["id"]
print("страниц:", job["page_count"], "списано:", job["scan_pages_charged"])

# 2. поллинг (раз в секунду)
while True:
    st = httpx.get(f"https://api.neuraldeep.ru/v1/ocr/jobs/{jid}", headers=H).json()
    if st["status"] == "completed":
        break
    time.sleep(1)

# 3. результат
res = httpx.get(
    f"https://api.neuraldeep.ru/v1/ocr/jobs/{jid}/result",
    params={"format": "markdown"}, headers=H,
).json()
print(res["content"])
```

> Статус, результат и preview не списывают лимит — платишь только за страницы при загрузке. `pro` = 2 страницы за лист (выбирай для сложной вёрстки/таблиц).

## Картинки · Image API

Тем же ключом — генерация и обработка картинок под `/v1/images/*`: генерация (FLUX), апскейл ×4, удаление фона, улучшение, аватар. Доступно на **всех тарифах**, отдельная корзина — лимит считается в **операциях** (1 операция = 1 вызов).

Работает **асинхронно**: `POST` создаёт задачу → `{task_uid}` → опрашиваешь `GET /v1/images/tasks/{uid}` пока `status=finished` → забираешь картинку бинарём через `GET /v1/images/tasks/{uid}/result` (сырой S3-url наружу не отдаём). FLUX понимает **только английский** — кириллический промпт авто-переводится (отключить: `translate=false`). Остаток — `GET /v1/images/quota`. При исчерпании — `429` с `Retry-After`.

**Размер** задаётся только через `options.aspect_ratio`: `1:1` · `9:16` · `16:9` · `4:5` · `3:2` · `5:3` · `3:5`. Параметры `width`/`height`/`size` не поддерживаются. Если не указать или передать что-то иное — по умолчанию `1:1` (запрос не падает).

```bash
# 1. генерация (промпт можно по-русски — авто-перевод RU→EN)
uid=$(curl -s https://api.neuraldeep.ru/v1/images/generate \
  -H "Authorization: Bearer $YOUR_KEY" -H "Content-Type: application/json" \
  -d '{"prompt":"кот-космонавт, неон","options":{"aspect_ratio":"1:1"}}' | jq -r .task_uid)

# 2. статус задачи
curl https://api.neuraldeep.ru/v1/images/tasks/$uid \
  -H "Authorization: Bearer $YOUR_KEY"

# 3. результат файлом
curl https://api.neuraldeep.ru/v1/images/tasks/$uid/result \
  -H "Authorization: Bearer $YOUR_KEY" -o out.png

# обработка готовой картинки (multipart): upscale / background/remove / enhance
curl https://api.neuraldeep.ru/v1/images/upscale \
  -H "Authorization: Bearer $YOUR_KEY" -F "image=@photo.jpg"
```

```python
import time, httpx

H = {"Authorization": "Bearer $YOUR_KEY"}
# 1. генерация
job = httpx.post(
    "https://api.neuraldeep.ru/v1/images/generate",
    headers=H, json={"prompt": "космический кот в неоне", "options": {"aspect_ratio": "1:1"}},
).json()
uid = job["task_uid"]

# 2. поллинг (раз в секунду, держи таймаут — очередь генерации иногда стопорится)
for _ in range(120):
    st = httpx.get(f"https://api.neuraldeep.ru/v1/images/tasks/{uid}", headers=H).json()
    if st["status"] == "finished":
        break
    time.sleep(1)

# 3. результат бинарём
img = httpx.get(f"https://api.neuraldeep.ru/v1/images/tasks/{uid}/result", headers=H).content
open("out.png", "wb").write(img)
```

> Статус и результат не списывают лимит — платишь только за создание задачи. Генерация — GPU-дорогая, держи таймаут на поллинге (очередь иногда стопорится независимо от апскейла/улучшения).

## Анонимизация ПДн · PII Guard API

Тем же ключом — обезличивание персональных данных (ПДн) под `/v1/pii/*`, чтобы встроить его в **свою** логику (не только через LLM). Находит ФИО, телефоны, e-mail, локации и т.п., заменяет на теги вида `<PII type="PERSON" id="1" />`, а на обратном пути восстанавливает — со **склонением по-русски**. Доступно на **всех тарифах**, отдельная от чата корзина — лимит считается в **операциях**.

**Приватность (важно):** сервис **stateless по маппингу** — соответствие тег→оригинал возвращается **тебе** в ответе `/anonymize` и **сразу удаляется** на нашей стороне. Исходные ПДн у нас не хранятся. Маппинг держишь ты и присылаешь его обратно в `/deanonymize`.

Два эндпоинта:
- `POST /v1/pii/anonymize` — `{text}` или `{texts:[…]}` → `{anonymized, mapping, system_prompt}`. `system_prompt` можно подложить модели, чтобы она аккуратно работала с тегами. Тариф: **1 ед. за блок ≤2000 симв** каждого текста.
- `POST /v1/pii/deanonymize` — `{text|texts, mapping}` → `{deanonymized}`. Тариф: **1 ед. за вызов** (обратная подстановка дешёвая).

Опционально `entities` в `/anonymize` — whitelist типов (`["PERSON","PHONE_NUMBER"]`), если нужно ловить только часть. Остаток — `GET /v1/pii/quota`. При исчерпании — `429` с `Retry-After`.

```bash
# 1. обезличить — сохрани mapping из ответа
curl https://api.neuraldeep.ru/v1/pii/anonymize \
  -H "Authorization: Bearer $YOUR_KEY" -H "Content-Type: application/json" \
  -d '{"text":"Иван Петров, +79161234567, живёт в Москве"}'
# → {"anonymized":"<PII type=\"PERSON\" ... /> , <PII type=\"PHONE_NUMBER\" ... />, ...",
#    "mapping":{"<PII type=\"PERSON\" id=\"1\" />":"Иван Петров", ...}, "system_prompt":"…"}

# 2. восстановить ответ модели (теги → оригиналы, со склонением)
curl https://api.neuraldeep.ru/v1/pii/deanonymize \
  -H "Authorization: Bearer $YOUR_KEY" -H "Content-Type: application/json" \
  -d '{"text":"Письмо для <PII type=\"PERSON\" id=\"1\" />","mapping":{...}}'
```

```python
import httpx
H = {"Authorization": "Bearer $YOUR_KEY"}

# 1. обезличиваем пользовательский текст
a = httpx.post("https://api.neuraldeep.ru/v1/pii/anonymize",
               headers=H, json={"text": "Иван Петров, тел +79161234567, Москва"}).json()
safe, mapping = a["anonymized"], a["mapping"]

# 2. отправляем В ЛЮБУЮ модель БЕЗ ПДн (system_prompt объясняет теги)
resp = httpx.post("https://api.neuraldeep.ru/v1/chat/completions", headers=H, json={
    "model": "qwen3.6-35b-a3b",
    "messages": [{"role": "system", "content": a["system_prompt"]},
                 {"role": "user", "content": safe}],
}).json()["choices"][0]["message"]["content"]

# 3. восстанавливаем ПДн в ответе модели (тем же mapping)
final = httpx.post("https://api.neuraldeep.ru/v1/pii/deanonymize",
                   headers=H, json={"text": resp, "mapping": mapping}).json()["deanonymized"]
print(final)
```

> Латентность `anonymize` — ~100–150 мс на короткий текст. Восстановление почти бесплатно по CPU (обратный словарь + морфология). Хочешь автоматическую анонимизацию **прямо в LLM-запросе** (без ручного пайпа) — это делает встроенный guardrail на ключе; напиши в поддержку, включим точечно на твой токен.

## SpeechCore · транскрибация аудио/видео

Отдельный сервис [speechcore.neuraldeep.ru](https://speechcore.neuraldeep.ru) — транскрибация аудио/видео (записи до ~6 часов): диаризация спикеров, тайм-коды по словам, экспорт SRT/VTT/TSV/DOCX/PDF. В вебе — вход через тот же Hub-аккаунт; **по API — тем же `sk-*` ключом**, что и для `/v1/*`. Для коротких аудио и OpenAI-совместимости есть также синхронный `whisper-1` (раздел «Транскрибация» выше) — SpeechCore же заточен под длинные записи и диаризацию.

База API — `https://speechcore.neuraldeep.ru/api` (НЕ `api.neuraldeep.ru`). Лимит считается в **транскрипциях в день**: free **1** · starter **50** · pro **200**. Работает **асинхронно**: `POST /upload` → `{task_id}` → опрашиваешь `GET /transcriptions/{id}/status` пока `status=completed` → забираешь результат: `GET /transcriptions/{id}` (JSON с сегментами, спикерами, `detected_language`, `duration`) или `GET /transcriptions/{id}/markdown` (текст с тайм-кодами).

**Опции транскрибации** — query-параметры к `POST /upload` (файл — в multipart-теле, опции — в URL):

- `diarize=true` — **разделение по спикерам**: в сегментах появляется поле `speaker` (`SPEAKER_00`, `SPEAKER_01`…), а в саммари — % времени говорения каждого. `diarize_speakers_num=N` (1–20) — подсказать точное число голосов, если оно известно (точнее, чем автоопределение).
- `language=ru` — зафиксировать язык (по умолчанию — автоопределение).
- **Спец-слова / терминология:** `hotwords` — ключевые слова через пробел или запятую (≤500 симв.), которые модель будет распознавать точнее: имена, бренды, аббревиатуры, продукты (`NeuralDeep, Kimi, RAG, ФЗ-152`).
- `initial_prompt` — контекст-промпт (≤2000 симв.): задаёт тематику/стиль и повышает точность на специфической лексике (`«Технический созвон про LLM-инфраструктуру»`).
- `model` — модель Whisper (по умолчанию `large-v3`).

```bash
# опции — в query-строке URL, файл — в multipart-теле (-F)
curl -s "https://speechcore.neuraldeep.ru/api/upload?diarize=true&diarize_speakers_num=3&language=ru&hotwords=NeuralDeep,Kimi,RAG&initial_prompt=Технический%20созвон%20про%20LLM" \
  -H "Authorization: Bearer $YOUR_KEY" \
  -F "file=@meeting.mp3"
```

```bash
# 1. загрузить (тот же sk-ключ, что и для /v1/*)
tid=$(curl -s https://speechcore.neuraldeep.ru/api/upload \
  -H "Authorization: Bearer $YOUR_KEY" \
  -F "file=@meeting.mp3" | jq -r .task_id)

# 2. статус
curl "https://speechcore.neuraldeep.ru/api/transcriptions/$tid/status" \
  -H "Authorization: Bearer $YOUR_KEY"

# 3. результат текстом (markdown с тайм-кодами)
curl "https://speechcore.neuraldeep.ru/api/transcriptions/$tid/markdown" \
  -H "Authorization: Bearer $YOUR_KEY"

# свои транскрипции списком
curl "https://speechcore.neuraldeep.ru/api/transcriptions?limit=20" \
  -H "Authorization: Bearer $YOUR_KEY"
```

```python
import time, httpx

BASE = "https://speechcore.neuraldeep.ru/api"
H = {"Authorization": "Bearer $YOUR_KEY"}

# 1. загрузка файла (аудио/видео, до ~6 ч) + опции в params
params = {
    "diarize": "true",            # разделение по спикерам
    "diarize_speakers_num": 3,    # если число голосов известно
    "language": "ru",
    "hotwords": "NeuralDeep, Kimi, RAG",          # спец-слова/термины
    "initial_prompt": "Технический созвон про LLM",
}
with open("meeting.mp3", "rb") as f:
    tid = httpx.post(f"{BASE}/upload", headers=H, params=params, files={"file": f}).json()["task_id"]

# 2. поллинг (раз в пару секунд)
while True:
    st = httpx.get(f"{BASE}/transcriptions/{tid}/status", headers=H).json()
    if st["status"] in ("completed", "failed"):
        break
    time.sleep(2)

# 3. результат: сегменты + тайм-коды, или готовый markdown
data = httpx.get(f"{BASE}/transcriptions/{tid}", headers=H).json()
print("язык:", data["detected_language"], "длительность:", data["duration"])
print(httpx.get(f"{BASE}/transcriptions/{tid}/markdown", headers=H).text)
```

**Возможности в веб-интерфейсе** ([speechcore.neuraldeep.ru](https://speechcore.neuraldeep.ru)), помимо API:

- **Диаризация спикеров** — реплики размечены по говорящим (SPEAKER_00…), видно кто сколько говорил (% времени).
- **Саммари встречи** — структурированное LLM-резюме (темы, решения, задачи) с учётом спикеров. Тем же sk-ключом.
- **Чат по транскрипту** — вопросы к расшифровке («что решили по X?», «какие дедлайны?») с ответами по тайм-кодам.
- **Экспорт** — SRT · VTT · TSV · DOCX · PDF · TXT, плюс markdown с тайм-кодами.
- **Спец-слова и контекст** — поля hotwords / контекст-промпт прямо в форме загрузки (то же, что `hotwords`/`initial_prompt` в API).
- **Публичная ссылка** — поделиться транскриптом на чтение без входа.
- **Realtime** — потоковая транскрибация с микрофона/вкладки (через расширение «Meeting Recorder»).

> Поллинг и выдача результата лимит не списывают — он тратится только при загрузке (1 файл = 1 транскрипция). Записи до ~6 ч; крупные файлы грузи целиком — обработка идёт в фоне на GPU, поэтому держи разумный таймаут на поллинге. Диаризация и спец-слова работают и в вебе, и по API одинаково.

## Озвучка · TTS API

Тем же ключом — синтез речи под `/v1/audio/speech`: текст → аудио (WAV 24 kHz). Модель **Qwen3-TTS** на наших GPU в РФ. Доступно на **всех тарифах**, отдельная корзина — лимит считается в **символах** входного текста. Формат запроса **OpenAI-совместимый**, работает **синхронно**: отправил текст → сразу получил аудио-файл.

**8 голосов:** `vivian`, `serena`, `ono_anna`, `sohee` (жен) · `dylan`, `ryan`, `aiden`, `uncle_fu` (муж) — мультиязычные. **10 языков:** `Russian`, `English`, `German`, `French`, `Spanish`, `Italian`, `Chinese`, `Japanese`, `Korean`, `Portuguese` (+`Auto`). **Эмоцию и стиль** задаёшь свободным текстом в `instructions` («радостно», «спокойный диктор»). Список голосов — `GET /v1/audio/voices`, остаток квоты — `GET /v1/audio/quota`. Исчерпание — `429` с `Retry-After`.

**🇷🇺 Русский — с ударениями.** При `language=Russian` озвучку берёт отдельная модель **ESpeech** (F5 + RUAccent) — она **корректно ставит ударения и резолвит омографы** по контексту (открыл замо́к → вошёл в за́мок), там где обычные TTS мажут. Прочие языки — на Qwen3-TTS. Какой движок озвучил, видно в ответном заголовке `X-TTS-Engine` (`espeech`/`qwen`). Хочешь задать ударение вручную — поставь `+` перед ударным гласным (`«зам+ок»`); если `+` уже есть в тексте, авто-акцентизатор не вмешивается.

```bash
# синтез (голос serena, русский) → файл
curl -s https://api.neuraldeep.ru/v1/audio/speech \
  -H "Authorization: Bearer $YOUR_KEY" -H "Content-Type: application/json" \
  -d '{"input":"Привет! Это синтез речи на наших серверах.","voice":"serena","language":"Russian"}' \
  -o speech.wav

# с эмоцией (через instructions)
curl -s https://api.neuraldeep.ru/v1/audio/speech \
  -H "Authorization: Bearer $YOUR_KEY" -H "Content-Type: application/json" \
  -d '{"input":"Поздравляю с релизом!","voice":"vivian","language":"Russian","instructions":"радостно, с энтузиазмом"}' \
  -o happy.wav

# ударение вручную через «+» (зам+ок = запор) — RUAccent не вмешивается, если «+» уже есть
curl -s https://api.neuraldeep.ru/v1/audio/speech \
  -H "Authorization: Bearer $YOUR_KEY" -H "Content-Type: application/json" \
  -d '{"input":"Он повесил амбарный зам+ок.","voice":"serena","language":"Russian"}' \
  -o stress.wav

# список голосов
curl https://api.neuraldeep.ru/v1/audio/voices -H "Authorization: Bearer $YOUR_KEY"
```

```python
import httpx

r = httpx.post(
    "https://api.neuraldeep.ru/v1/audio/speech",
    headers={"Authorization": "Bearer $YOUR_KEY"},
    json={"input": "Текст для озвучки.", "voice": "dylan", "language": "Russian"},
)
open("speech.wav", "wb").write(r.content)
```

> Лимит — в символах текста (`len(input)`), не в запросах. Кап — 5 000 символов на один вызов. Клонирование голоса по образцу пока недоступно (вводим за согласием/верификацией).

## Drift · персональный агент через API

**Drift** — наш self-hosted AI-агент с памятью, sandbox'ом, скиллами и Google/Telegram-интеграциями. Обычно ты с ним общаешься на [drift.neuraldeep.ru](https://drift.neuraldeep.ru) или через Telegram-бота. Но можно ходить и через публичный API — из curl, Python-SDK, Cursor/Cline/Continue, своих ботов или CI.

**Доступ:** только при оплаченном Drift (Drift Pass / Starter / Pro). Trial и admin-grant — нет.
**Лимиты:** наследуются от тарифа Hub-аккаунта (RPM / parallel / session / week — см. `limits` ниже).
**Source-метка:** каждый запрос через API стампится как `source=api` — видно в логах кабинета и админки (web / api / telegram / scheduler).

#### 1 · Выпустить токен

Открой [Dashboard](/app) → секция «🤖 Drift API tokens» → «+ создать токен». Имя — для своих заметок («cursor», «my-bot», etc.). Plaintext значение (`dft_*`) показывается **один раз** при создании — скопируй сразу. Лимит 5 активных токенов на юзера. Отозвать можно в любой момент кнопкой «отозвать».

#### 2 · Отправить сообщение

OpenAI-совместимый `/v1/chat/completions`. Внутри запускается ReAct-агент с тулзами (sandbox shell, web-search, Google, etc.) — ответ может прилететь не сразу, агент может крутить несколько итераций. Stream через `stream: true` рекомендуется для длинных задач.

```bash
curl https://drift.neuraldeep.ru/v1/chat/completions \
  -H "Authorization: Bearer dft_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-oss-120b",
    "messages": [
      {"role": "user", "content": "Что у меня запланировано на завтра?"}
    ]
  }'
```

```bash
curl -N https://drift.neuraldeep.ru/v1/chat/completions \
  -H "Authorization: Bearer dft_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-oss-120b",
    "messages": [{"role":"user","content":"Прочитай мой MEMORY.md и пересскажи кратко"}],
    "stream": true
  }'

# event-stream возвращает:
#   data: {"choices":[{"delta":{"content":"..."}}],...}
#   data: [DONE]
```

```python
from openai import OpenAI

# base_url указывает на Drift, токен — личный dft_*
client = OpenAI(
    api_key="dft_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
    base_url="https://drift.neuraldeep.ru/v1",
)

# Non-stream
r = client.chat.completions.create(
    model="gpt-oss-120b",  # или qwen3.6-35b-a3b — две основные LLM Hub'а
    messages=[{"role": "user", "content": "Запиши в память: проект Y стартует 1 июня"}],
)
print(r.choices[0].message.content)

# Stream
stream = client.chat.completions.create(
    model="gpt-oss-120b",
    messages=[{"role": "user", "content": "Покажи список моих файлов и пересскажи MEMORY.md"}],
    stream=True,
)
for chunk in stream:
    if chunk.choices[0].delta.content:
        print(chunk.choices[0].delta.content, end="", flush=True)
```

> Поле `model` — это какой upstream LLM использовать внутри агента: `gpt-oss-120b` (131k ctx, длинный reasoning, рекомендуется по умолчанию) или `qwen3.6-35b-a3b` (256k ctx, нативные tool-calls). Если не указать — fallback на gpt-oss-120b.
> **Не передавай весь history** в `messages` — Drift сам поднимает память из своей БД, иначе будет дубль. Шли только последний user-промпт.

#### 3 · Управлять чатами (conversations)

По умолчанию все сообщения идут в один дефолтный чат юзера. Если хочешь разделять контексты (например, рабочий и личный) — создавай chat'ы явно и передавай `conversation_id`.

```bash
# список своих чатов
curl https://drift.neuraldeep.ru/v1/conversations \
  -H "Authorization: Bearer dft_..."

# создать новый
curl -X POST https://drift.neuraldeep.ru/v1/conversations \
  -H "Authorization: Bearer dft_..." \
  -H "Content-Type: application/json" \
  -d '{"title": "Project X — research"}'
# → {"id": 1234, "title": "Project X — research", ...}

# отправить сообщение в конкретный чат
curl -X POST https://drift.neuraldeep.ru/v1/chat/completions \
  -H "Authorization: Bearer dft_..." \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-oss-120b",
    "messages": [{"role":"user","content":"Какой статус по project X?"}],
    "conversation_id": 1234
  }'

# удалить чат
curl -X DELETE https://drift.neuraldeep.ru/v1/conversations/1234 \
  -H "Authorization: Bearer dft_..."
```

#### 4 · Прочитать историю

```bash
curl "https://drift.neuraldeep.ru/v1/history?conversation_id=1234&limit=50" \
  -H "Authorization: Bearer dft_..."
# → {"messages":[{"id":..., "role":"user|assistant", "content":"...", "created_at":..., "source":"api"}, ...]}
```

> Поле `source` в каждом сообщении показывает откуда оно пришло: `web` (drift.neuraldeep.ru), `api` (этот endpoint), `telegram` (бот), `scheduler` (фоновая задача по расписанию). Используй его если у тебя смешанные источники и нужно фильтровать.

#### Что НЕ поддерживается

· редактирование/regenerate сообщений (только append) · · branching (forks) · · webhook'и события · · multi-image в одном сообщении (один — да, через `/v1/files/upload`)

> **Свои tools / skills / MCP-серверы** — поддерживается с 28.05.26. См. раздел «🔌 Свои tools, skills, MCP» ниже: можно передавать caller-tools (return-to-caller pattern), inline SKILL.md и подключать remote MCP-серверы прямо в запросе.

## Drift · файлы + sandbox preview

У Drift'а свой workspace в MinIO (per-user, изолированный) и под капотом — Docker-sandbox с предустановленными `openpyxl / pandas / pdfplumber / pypdf / python-docx / Pillow / chardet / bs4 / lxml`. Аплоадишь файл — он попадает и в MinIO, и сразу в sandbox; модель видит **preview первых страниц/строк** в init-промпте без лишних tool-calls.

```bash
curl -X POST https://drift.neuraldeep.ru/v1/files/upload \
  -H "Authorization: Bearer dft_xxxxxxxx" \
  -F file=@./report.pdf
# → {"uploaded":[{"name":"report.pdf","path":"report.pdf","size":48863,
#                 "preview":"...первые 2 страницы PDF..."}]}
```

```python
import httpx
TOKEN = "dft_xxxxxxxx"
BASE  = "https://drift.neuraldeep.ru/v1"

# 1) upload
with open("report.pdf","rb") as f:
    up = httpx.post(f"{BASE}/files/upload",
                    headers={"Authorization": f"Bearer {TOKEN}"},
                    files={"file": ("report.pdf", f, "application/pdf")}).json()
print("preview:", up["uploaded"][0]["preview"][:200])

# 2) ask agent — он уже видит preview в init prompt
r = httpx.post(f"{BASE}/chat/completions",
    headers={"Authorization": f"Bearer {TOKEN}"},
    json={"model":"qwen3.6-35b-a3b","messages":[
        {"role":"user","content":"Резюмируй report.pdf"}
    ]}).json()
print(r["choices"][0]["message"]["content"])
```

> Preview-форматы: PDF → 2 страницы (pdfplumber), XLSX → первый лист 20 строк (openpyxl), CSV/TXT/JSON/MD → 6000 символов, DOCX → 6000 символов. Для остального — preview `null`, модель сама позовёт `read_file` или python в sandbox.

> **Лимиты:** 20 MB на файл, 8 файлов на запрос. Sandbox UTF-8, кириллица в именах работает.
> **analyze_image** работает *только* на растровых картинках (.png/.jpg/.webp/...). PDF читай через preview или `pdfplumber`.

## Drift · свои tools, skills, MCP

С 28.05.26 в `/v1/chat/completions` можно передавать **свои** инструменты, скиллы и remote MCP-серверы. Полная схема — в примерах ниже.

#### tools — return-to-caller (как OpenAI)

Передаёшь массив `tools` в формате OpenAI. Если модель решит позвать инструмент — она вернёт `finish_reason: "tool_calls"` с JSON-аргументами. **Исполняешь ты сам** в своей среде (Drift не запускает твой код), результат шлёшь обратно в `messages`.

```python
from openai import OpenAI

client = OpenAI(api_key="dft_xxxxxxxx", base_url="https://drift.neuraldeep.ru/v1")

tools = [{"type":"function","function":{
    "name":"get_weather",
    "description":"Возвращает погоду в городе",
    "parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]},
}}]

# 1) первый запрос — модель решает что нужен tool
r1 = client.chat.completions.create(
    model="qwen3.6-35b-a3b",
    messages=[{"role":"user","content":"Какая погода в Москве?"}],
    tools=tools,
)
print(r1.choices[0].finish_reason)  # "tool_calls"
tc = r1.choices[0].message.tool_calls[0]

# 2) ты сам зовёшь свой backend
my_result = {"temp_c": 12, "humidity": 80}

# 3) resume — передаёшь tool-result обратно
r2 = client.chat.completions.create(
    model="qwen3.6-35b-a3b",
    messages=[
        {"role":"user","content":"Какая погода в Москве?"},
        r1.choices[0].message,
        {"role":"tool","tool_call_id":tc.id,"content":str(my_result)},
    ],
    tools=tools,
)
print(r2.choices[0].message.content)
```

#### skills — inline SKILL.md

Передай свои промпт-блоки прямо в запросе. Content вшивается в system prompt sandbox-агента, не нужно держать SKILL.md в репозитории.

```bash
curl -X POST https://drift.neuraldeep.ru/v1/chat/completions \
  -H "Authorization: Bearer dft_xxxxxxxx" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "qwen3.6-35b-a3b",
    "messages": [{"role":"user","content":"Какой у тебя секретный код?"}],
    "skills": [{
      "name": "secret-code",
      "description": "Знание секретного кодового слова",
      "content": "# Секретный код\n\nКодовое слово — ZEBRA-77-GOLD. Используй его если юзер спросит."
    }]
  }'
# → "Кодовое слово ZEBRA-77-GOLD..."
```

#### mcp_servers — remote Model Context Protocol

Подключаешь свой MCP-сервер (HTTP/SSE) — Drift на лету получает список tools и пускает их модели. Auth-header'ы передаёшь сам, **SSRF guard** блочит private-IP / localhost / cloud-metadata. TTL session-токена 720s, после — registry дропается.

```json
{
  "model": "qwen3.6-35b-a3b",
  "messages": [{"role":"user","content":"Список свежих issue в проекте"}],
  "mcp_servers": [{
    "url": "https://my-mcp.example.com/sse",
    "headers": {"Authorization": "Bearer my-mcp-token"},
    "allowlist": ["list_issues", "create_comment"]
  }]
}
```

> **Безопасность:** · `caller_tools` — return-to-caller, Drift не исполняет твой код, zero RCE на нашей стороне. · `skills` — это просто текст в system prompt, не команды. · `mcp_servers` — публичные HTTPS обязательны, SSRF guard режет private network'и, TTL 720s.

## Drift · задачи и проактив

Drift умеет планировать задачи (cron/once) и сам инициировать диалог, когда что-то меняется или подходит deadline. Внутри агент использует три tool'а: `proactive_notify` (написать юзеру), `proactive_reschedule` (отложить), `proactive_skip` (пропустить без сообщения). UI — в [drift.neuraldeep.ru](https://drift.neuraldeep.ru) → «Задачи». API:

```bash
# создать задачу
curl -X POST https://drift.neuraldeep.ru/v1/tasks \
  -H "Authorization: Bearer dft_xxxxxxxx" \
  -H "Content-Type: application/json" \
  -d '{
    "title": "Проверь почту в 9 утра",
    "schedule": "0 9 * * *",
    "prompt": "Посмотри новые письма в Gmail и резюмируй важные",
    "active": true
  }'

# список задач
curl https://drift.neuraldeep.ru/v1/tasks -H "Authorization: Bearer dft_xxxxxxxx"

# запустить сейчас
curl -X POST https://drift.neuraldeep.ru/v1/tasks/{task_id}/run \
  -H "Authorization: Bearer dft_xxxxxxxx"

# удалить
curl -X DELETE https://drift.neuraldeep.ru/v1/tasks/{task_id} \
  -H "Authorization: Bearer dft_xxxxxxxx"
```

> Schedule — стандартный cron-syntax (5-полевой). Drift проверяет задачи раз в минуту. Если ты в proactive-выводе зовёшь `proactive_skip` — юзеру push не уходит, задача отмечена «skipped». `proactive_reschedule(seconds=N)` — сдвигает следующий запуск, не меняя cron.

## Конструктор агентов · Agent Hosting

No-code конструктор автоматизаций в кабинете: [hub.neuraldeep.ru/app](https://hub.neuraldeep.ru/app/agents) → «Агенты». Описываешь агента словами и задаёшь триггер — он работает сам на движке Drift (веб-поиск, код в песочнице, файлы, память) и присылает результат в Telegram. Отдельного доступа к GPU у него нет — всё через тот же API.

Что настраивается:

- · **Инструкция** — что агент делает каждый запуск (промпт задачи).
- · **Модель** — Qwen 3.6 / GPT-OSS 120B / Kimi K2.6 (Pro). Для ресёрча с инструментами лучше reasoning-модель.
- · **Триггер:** `вручную` (кнопка), `таймер` (интервал — напр. утренний дайджест), `вебхук` (персональный URL).
- · **Доставка** — итог приходит в Telegram: текст, а сгенерированные файлы (PDF, таблицы) — документом.
- · **Запуски** — каждый прогон логируется: шаги, вызванные инструменты, трейс и итог.

Вебхук-триггер — запуск агента из внешней системы (тело запроса = вход агента):

```bash
curl -X POST https://drift.neuraldeep.ru/v1/agents/hook/<токен-вебхука> \
  -H "Content-Type: application/json" \
  -d '{"topic": "Сделай отчёт по рынку ИИ в РФ"}'
# токен вебхука = аутентификация, берётся из карточки агента в кабинете
# ответ: {"ok": true, "accepted": true} — агент отрабатывает в фоне, результат в Telegram
```

> Расписанием управляет платформа — пиши, ЧТО делать каждый запуск, а не «каждые N минут» (интервал задаётся триггером). Если нужен файл — явно проси в инструкции *«сделай PDF и доставь через deliver_file»*, иначе агент может ограничиться текстом.

## Drift · память

У каждого юзера в Drift есть постоянная память — `MEMORY.md` в его workspace + per-conversation сжатая история. Агент сам решает что записать ([[remember-the-X]] паттерн в reasoning), но через API можно и снаружи дёргать.

```bash
# прочитать
curl https://drift.neuraldeep.ru/v1/memory \
  -H "Authorization: Bearer dft_xxxxxxxx"
# → {"content":"# Memory\n\n- Имя: Иван\n- Тариф: starter\n..."}

# перезаписать (осторожно — переписывает всё)
curl -X PUT https://drift.neuraldeep.ru/v1/memory \
  -H "Authorization: Bearer dft_xxxxxxxx" \
  -H "Content-Type: application/json" \
  -d '{"content":"# Memory\n\n- Проект Y стартует 1 июня"}'
```

> Обычно проще не дёргать API напрямую — попроси агента *«Запомни Х»* в диалоге, он сам решит формат и не сломает существующие заметки.

## Drift · изоляция и безопасность

Каждый юзер Drift живёт в **per-user Docker-контейнере** с эфемерным volume `daisy-ws-{uid}`. Кросс-юзерного доступа нет.

Что обеспечивается:

- · **Sandbox boundary** — внутри контейнера юзер видит ровно `/workspace` (без uid в пути), нет mount'а соседей.
- · **SSRF guard** на MCP-URL и tool `fetch_url` — блок на private/loopback/link-local/cloud-metadata.
- · **Auth** — каждый запрос верифицируется по `dft_*` токену; 5 активных токенов на юзера, мгновенный revoke.
- · **Tier-gating** — Drift доступен только при оплате (Drift Pass / Starter / Pro), trial 14 дней для новых юзеров.
- · **Session TTL** — MCP registry в памяти core, drop через 720s или при finally в dispatch.
- · **Privacy** — prompt/response в БД для админ-модерации, но retention-окно admin / classify-then-delete для opt-out юзеров. Не обучаемся на твоих данных.

## Идеальные настройки кодового агента

Кодовый агент упирается в лимит не объёмом, а одновременностью: на один ход он делает пачку tool-call'ов и выбивает `parallel` раньше, чем подходит к потолку запросов. Конструктор выше собирает конфиг под конкретного агента, где число потоков, пауза и таймауты уже посчитаны от лимитов выбранного тарифа.

**Числа живые.** Конструктор читает [/api/public/tier-limits](https://hub.neuraldeep.ru/api/public/tier-limits) — тот же словарь, по которому гейт отказывает, вместе с админ-оверрайдами. Мы подняли тебе rate limit — выданный конфиг меняется сам, без правки доки. Поэтому таблицы лимитов тут нет: второй экземпляр чисел рано или поздно разъедется с первым.

```bash
curl -s https://hub.neuraldeep.ru/api/public/tier-limits | jq
```

По каким правилам считается конфиг:

- · **потоки** — не весь `parallel` тарифа, а примерно три четверти. Ключ у тебя один на всё: тот же `sk-` уходит в веб-чат, а он на каждое сообщение шлёт параллельно ответ и фоновые запросы. Отдашь агенту все слоты — поймаешь 429 ровно в тот момент, когда откроешь чат в браузере;
- · **пауза между запросами** — 60 секунд, делённые на чат-RPM тарифа. Нужна клиентам, у которых есть такой регулятор (Cline, Roo Code, Kilo Code); у остальных её роль играет ограничение потоков;
- · **таймаут запроса** — две трети от потолка гейта. Клиент обязан сдаваться раньше нас: иначе вместо своей внятной ошибки он получит наш 408, потратив на мёртвый запрос всё время целиком;
- · **тишина в стриме** — 60 секунд без единого чанка. Меньше — ловишь ложные обрывы, когда лейн просто стоит в очереди; больше — повторяешь историю, когда апстрим отдал 200 и замолчал, а человек четверть часа смотрел в пустой экран;
- · **ретраи** — три, это меньше, чем по умолчанию у самих клиентов. Все они игнорируют `Retry-After` от 60 секунд и выше (защита от «сервер усыпил меня на два часа»), поэтому длинные окна — трёхчасовой cooldown и недельный кап — повторами не переживаются, попытки просто сгорают внутри того же окна.

Таймауты одинаковы на всех тарифах, и это не упрощение: они описывают поведение гейта и апстрима, а тариф задаёт одновременность и объём. Разводить таймауты по тарифам значило бы выдумывать точность, которой нет.

Чем реально ходят на хаб (снимок `request_log.harness` за 30 дней, 20.08.26): opencode — 60 аккаунтов, Cline — 12, Kilo Code — 11, Qwen Code — 9, Zed — 5, Roo Code, Codex, aider — единицы. Заметная доля трафика идёт самописными агентами, поэтому в конструкторе есть отдельный профиль под OpenAI SDK.

Агента нет в списке? Транспортные настройки переносятся один в один: столько-то потоков, такая-то пауза, такие-то таймауты — названия ключей смотри в его доке.

## Лимиты и коды ошибок

Лимиты действуют **четыре одновременно**: `session` (3h-окно по UTC), `week` (ISO-неделя), `parallel` (одновременные in-flight запросы) и `rpm` (запросов в минуту). Любой из них даёт 429 — ретраить нужно по конкретной причине. Чем выше тариф (free → starter → pro) — тем шире окна по всем четырём измерениям.

> Если работаешь через агент (codex / opencode / cline / continue) — самая частая причина 429 это `parallel`, агент стреляет 3+ tool-call'а одновременно. На free поставь `max_concurrency=2`, на платных тарифах можно агрессивнее — точное число подбери эмпирически.

Headers на ответе:

- `X-Tier` — free | starter | pro
- `X-Window` — session | week (на наших 429; LiteLLM-нативные parallel/rpm пока без header'а — парси body)
- `Retry-After` — секунды до можно-снова

Примеры 429 body:

```json
// session исчерпан
{"detail": "session limit reached — cooldown N min"}

// week исчерпан
{"detail": "week limit reached — reset in Nh"}

// parallel — клиент бьёт >N concurrent
{"detail": "Rate limit exceeded ... Limit type: max_parallel_requests"}

// rpm — burst >N req/min
{"detail": "Rate limit exceeded ... Limit type: rpm"}
```

Полный список кодов:

- 200 — ok
- 400 — context_window_exceeded или невалидный body
- 401 — bad/revoked key, или key не подписан на модель
- 408 — upstream таймаут (router retry × 2 не помог)
- 429 — rate limit, читай `X-Window` + `Retry-After` или `detail.Limit type`
- 500 — internal error, повторить
- 502 — upstream недоступен; обычно проходит автоматически (router retry × 2)

## Книга «Агенты и вайб-кодинг» · поиск и MCP

Практический курс о работе с ИИ-агентами — 42 главы — доступен как поиск для агентов. **Нужен твой ключ Hub** — тот же `sk-`, что и для чата. Читалка на сайте открыта всем без ключа — [neuraldeep.ru/learn/books/agenty-vajbkoding](https://neuraldeep.ru/learn/books/agenty-vajbkoding).

Поиск гибридный: полнотекстовый BM25 плюс векторный по эмбеддингам `giga-embeddings` (370 чанков, нарезка по абзацам). Они ошибаются по-разному — полнотекстовый находит точное слово, но бессилен, когда спрашивают другими словами; векторный ловит смысл, но промахивается мимо термина. Слияние закрывает обе дыры, поэтому `mode=hybrid` стоит по умолчанию.

```bash
curl -G "https://neuraldeep.ru/api/v1/books/agenty-vajbkoding/search" \
  -H "Authorization: Bearer $YOUR_KEY" \
  --data-urlencode "q=сколько стоит держать много скиллов" \
  --data-urlencode "mode=hybrid" --data-urlencode "limit=5"
```

```json
{
  "query": "…", "mode": "hybrid", "used_vector": true,
  "hits": [{
    "chapter": "ch12",
    "chapter_title": "Скиллы: механика и жизненный цикл",
    "label": "Глава 12",
    "score": 0.032,
    "text": "Скиллы платные, даже когда не используются…",
    "url": "https://neuraldeep.ru/learn/books/agenty-vajbkoding/ch12"
  }]
}
```

**Эндпоинты**

| метод | что делает |
|---|---|
| `GET /api/v1/books` | список книг |
| `GET /api/v1/books/{slug}/toc` | оглавление: главы, приложения, модули |
| `GET /api/v1/books/{slug}/chapter/{id}` | полный текст главы (`ch12`, `appА`) |
| `GET /api/v1/books/{slug}/search` | поиск: `q`, `mode` = `hybrid`\|`fts`\|`vector`, `limit` 1–20 |

**MCP-сервер** — тот же поиск инструментами для твоего агента:

```json
{
  "mcpServers": {
    "neuraldeep-book": {
      "type": "http",
      "url": "https://neuraldeep.ru/api/mcp/book",
      "headers": { "Authorization": "Bearer $YOUR_KEY" }
    }
  }
}
```

Инструменты: `book_search` (гибридный поиск), `book_chapter` (глава целиком), `book_toc` (оглавление). Транспорт — JSON-RPC поверх HTTP, состояния сервер не держит. Без ключа — 401.

**Ограничения.** 60 запросов в минуту на ключ. Счёт идёт по владельцу ключа, а не по адресу, — доп-ключи считаются вместе с основным. Ответы модели по книге остаются в читалке: за ними GPU, и там действует лимит тарифа (free 10 вопросов в сутки).

Книга собрана из полугода переписки сообщества [t.me/aostrikov_agents_chat](https://t.me/aostrikov_agents_chat).

## Стриминг · SSE

Добавь `"stream": true` → ответ приходит SSE-потоком: чанки `data: {...}`, последний — `data: [DONE]`. Работает на всех чат-моделях и при tool-calling.

## SDK · Python / JS

OpenAI-совместимо — подойдут официальные SDK, только поменяй `base_url`.

```python
from openai import OpenAI
client = OpenAI(api_key="$YOUR_KEY", base_url="https://api.neuraldeep.ru/v1")
r = client.chat.completions.create(model="gpt-oss-120b", messages=[...], max_tokens=500)
```

```typescript
import OpenAI from "openai";
const client = new OpenAI({ apiKey: "$YOUR_KEY", baseURL: "https://api.neuraldeep.ru/v1" });
```

## Privacy · хранение данных

По умолчанию prompts/responses **не хранятся**. В Dashboard два независимых тогла:

- **Хранение истории запросов** (default OFF) — если включить, тело запроса/ответа остаётся в БД и видно тебе в логах. Если выключено — тело удаляется сразу после обработки, в БД только метрики (модель, токены, статус, latency).
- **Разрешить аналитику запросов** — если включено, мы LLM-классифицируем тип запроса (код / ресёрч / агент) для развития сервиса; тело используется только для классификации и сразу удаляется. Если выключено — тело не читается, категоризация только по типу endpoint'а.

На моделях не обучаемся, данные не продаём. Метрики (latency, токены, статус) собираются всегда обезличенно через Prometheus.
