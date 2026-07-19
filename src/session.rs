//! Хранилище сессий с деревом resume (fork-tree).
//!
//! Уроки обзора `codex-rs/thread-store` (родительские ссылки `forked_from_id`,
//! дерево веток, path-адресация rollout-файлов) и `codex-rs/message-history`
//! (толерантное перечисление истории: битая запись не должна ронять список).
//!
//! ## Формат файла
//!
//! Одна сессия — один JSON-файл `session-<created>-<id>.json` в каталоге
//! хранилища, где `created` — UNIX-секунды создания сессии:
//!
//! ```text
//! {
//!   "id": "…", "parent": null | "…", "created": 1784366471,
//!   "workspace": "/abs/path", "model": "deepseek-v4-pro",
//!   "messages": [{"role": "user", "content": "…", "tool_calls": [ … ]?}],
//!   "meta": {"turns": 3, "api_calls": 5, "tokens": 12345}
//! }
//! ```
//!
//! ## Семантика
//!
//! - [`SessionStore::save`] — атомарная запись (tmp-файл + `rename(2)`), имя
//!   файла детерминировано парой `(created, id)`: повторный `save` той же
//!   сессии перезаписывает тот же файл. Гонка двух процессов по одному id —
//!   last-writer-wins, но без повреждения файла (rename атомарен);
//! - [`SessionStore::load`] — строгий полный парс: битый файл = ошибка;
//! - [`SessionStore::list`] — толерантное перечисление: парсится только шапка
//!   файла без материализации тел сообщений ([`SessionHeader`]), битые файлы
//!   пропускаются с предупреждением в stderr, недостающие `id`/`created`
//!   добираются из имени файла (совместимость со старыми `session-<ts>.json`);
//! - [`SessionStore::fork`] — ветка дерева resume: новая сессия с
//!   `parent = id` исходной и историей, обрезанной до точки форка. Файл на
//!   диске не создаётся — когда сохранять ветку, решает вызывающая сторона.

#![forbid(unsafe_code)]

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Deserializer, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Префикс имени файла сессии.
const FILE_PREFIX: &str = "session-";
/// Суффикс (расширение) имени файла сессии.
const FILE_SUFFIX: &str = ".json";

/// Роль автора сообщения сессии.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// системный промпт
    System,
    /// пользователь
    User,
    /// ассистент (LLM)
    Assistant,
    /// результат инструмента
    Tool,
}

impl Role {
    /// Строковое имя роли (как в JSON-файле).
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Один вызов инструмента, записанный ассистентом в сообщение.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// id вызова (для связи с результатом инструмента)
    pub id: String,
    /// имя инструмента
    pub name: String,
    /// аргументы вызова (JSON-строкой, как в chat/completions)
    pub arguments: String,
}

/// Сообщение сессии: роль, текст и (опционально) вызовы инструментов.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// автор сообщения
    pub role: Role,
    /// текст (пустой у чистых tool-вызовов ассистента)
    pub content: String,
    /// вызовы инструментов; ключ в JSON опускается при `None`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

impl Message {
    /// Произвольное сообщение без вызовов инструментов.
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Message { role, content: content.into(), tool_calls: None }
    }

    /// Системное сообщение.
    pub fn system(content: impl Into<String>) -> Self {
        Self::new(Role::System, content)
    }

    /// Пользовательское сообщение.
    pub fn user(content: impl Into<String>) -> Self {
        Self::new(Role::User, content)
    }

    /// Текстовое сообщение ассистента.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new(Role::Assistant, content)
    }

    /// Сообщение-результат инструмента.
    pub fn tool(content: impl Into<String>) -> Self {
        Self::new(Role::Tool, content)
    }

    /// Сообщение ассистента с вызовами инструментов.
    pub fn assistant_with_tools(content: impl Into<String>, calls: Vec<ToolCall>) -> Self {
        Message { role: Role::Assistant, content: content.into(), tool_calls: Some(calls) }
    }

    /// `true`, если сообщение несёт вызовы инструментов.
    pub fn has_tool_calls(&self) -> bool {
        self.tool_calls.as_ref().is_some_and(|calls| !calls.is_empty())
    }
}

/// Счётчики сессии (поле `meta` файла): ходы, вызовы API, суммарные токены.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    /// завершённые ходы агента
    pub turns: u64,
    /// вызовы LLM API
    pub api_calls: u64,
    /// суммарные токены (prompt + completion)
    pub tokens: u64,
}

/// Сессия целиком — содержимое файла `session-<created>-<id>.json`.
///
/// Поля `id` и `created` обязательны при загрузке (строгий [`SessionStore::load`]);
/// остальные имеют serde-умолчания, чтобы не ломаться на урезанных файлах.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// уникальный id сессии (см. [`new_id`])
    pub id: String,
    /// id родительской сессии; `None` — корень дерева resume
    #[serde(default)]
    pub parent: Option<String>,
    /// создана, UNIX-секунды
    pub created: u64,
    /// рабочий каталог сессии
    #[serde(default)]
    pub workspace: PathBuf,
    /// модель LLM
    #[serde(default)]
    pub model: String,
    /// история переписки
    #[serde(default)]
    pub messages: Vec<Message>,
    /// счётчики использования
    #[serde(default)]
    pub meta: Meta,
}

impl Session {
    /// Новая корневая сессия (без родителя) со свежим id и текущим `created`.
    pub fn new(workspace: impl Into<PathBuf>, model: impl Into<String>) -> Self {
        Session {
            id: new_id(),
            parent: None,
            created: now_secs(),
            workspace: workspace.into(),
            model: model.into(),
            messages: Vec::new(),
            meta: Meta::default(),
        }
    }

    /// Имя файла сессии: `session-<created>-<id>.json`.
    pub fn file_name(&self) -> String {
        format!("{FILE_PREFIX}{}-{}{FILE_SUFFIX}", self.created, self.id)
    }

    /// `true`, если сессия — ветка (форк, есть родитель).
    pub fn is_fork(&self) -> bool {
        self.parent.is_some()
    }

    /// Число сообщений в истории.
    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    /// Добавить сообщение в конец истории.
    pub fn push_message(&mut self, message: Message) {
        self.messages.push(message);
    }

    /// Учесть один завершённый ход агента.
    pub fn record_turn(&mut self) {
        self.meta.turns += 1;
    }

    /// Учесть вызов LLM API: +1 к числу вызовов, токены prompt+completion —
    /// в общий счётчик токенов.
    pub fn record_api_call(&mut self, prompt_tokens: u64, completion_tokens: u64) {
        self.meta.api_calls += 1;
        self.meta.tokens += prompt_tokens + completion_tokens;
    }
}

/// Лёгкое описание сессии без тел сообщений — элемент [`SessionStore::list`]
/// и [`SessionStore::children_of`].
#[derive(Debug, Clone, PartialEq)]
pub struct SessionMeta {
    /// id сессии (из шапки файла; для старых файлов — из имени файла)
    pub id: String,
    /// id родителя; `None` — корень дерева
    pub parent: Option<String>,
    /// создана, UNIX-секунды
    pub created: u64,
    /// рабочий каталог сессии
    pub workspace: PathBuf,
    /// модель LLM
    pub model: String,
    /// путь к файлу на диске
    pub path: PathBuf,
    /// число сообщений (посчитано без материализации их тел)
    pub message_count: usize,
    /// счётчики (поле `meta` файла)
    pub meta: Meta,
}

impl SessionMeta {
    /// `true`, если сессия — корень дерева (не форк).
    pub fn is_root(&self) -> bool {
        self.parent.is_none()
    }
}

/// Каталог-ориентированное хранилище сессий с деревом resume.
///
/// Все операции чтения толерантны к мусору в каталоге: посторонние файлы и
/// недописанные tmp-файлы игнорируются, битые JSON пропускаются с warning.
pub struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    /// Открыть хранилище в каталоге `dir`; каталог создаётся при отсутствии.
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)
            .with_context(|| format!("не удалось создать каталог сессий {}", dir.display()))?;
        Ok(SessionStore { dir })
    }

    /// Каталог хранилища.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Сохранить сессию атомарно (tmp-файл + rename). Возвращает путь файла.
    ///
    /// Путь детерминирован парой `(created, id)` — повторное сохранение той же
    /// сессии перезаписывает тот же файл. `id` проверяется на
    /// файлобезопасность (защита от path traversal через чужой/битый id).
    pub fn save(&self, session: &Session) -> Result<PathBuf> {
        validate_id(&session.id)?;
        let name = session.file_name();
        let path = self.dir.join(&name);
        let pid = std::process::id();
        let tmp = self.dir.join(format!("{name}.tmp-{pid}"));
        let json =
            serde_json::to_string_pretty(session).context("не удалось сериализовать сессию")?;
        fs::write(&tmp, json).with_context(|| format!("не удалось записать {}", tmp.display()))?;
        if let Err(e) = fs::rename(&tmp, &path) {
            let _ = fs::remove_file(&tmp); // не оставляем сироту
            return Err(e).with_context(|| format!("не удалось переименовать в {}", path.display()));
        }
        Ok(path)
    }

    /// Загрузить сессию из файла (строгий полный парс: битый JSON — ошибка).
    pub fn load(&self, path: impl AsRef<Path>) -> Result<Session> {
        let path = path.as_ref();
        let text = fs::read_to_string(path)
            .with_context(|| format!("не удалось прочитать {}", path.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("битый файл сессии {}", path.display()))
    }

    /// Перечислить сессии: новые первыми (`created` по убыванию, при равенстве —
    /// `id` по убыванию). Парсится только шапка каждого файла; битые файлы
    /// пропускаются с предупреждением в stderr. Отсутствующий каталог —
    /// пустой список.
    pub fn list(&self) -> Result<Vec<SessionMeta>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("не удалось перечислить {}", self.dir.display()));
            }
        };
        let mut out = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(e) => {
                    eprintln!("session: ошибка чтения элемента каталога: {e}");
                    continue;
                }
            };
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some((created_in_name, id_in_name)) = parse_file_name(name) else { continue };
            let path = entry.path();
            if !path.is_file() {
                continue; // подкаталог с «нашим» именем — не сессия
            }
            match Self::read_header(&path) {
                Ok(mut meta) => {
                    // Добираем пробелы шапки из имени файла (старые session-<ts>.json).
                    if meta.created == 0 {
                        meta.created = created_in_name;
                    }
                    if meta.id.is_empty() {
                        meta.id =
                            id_in_name.map_or_else(|| format!("legacy-{created_in_name}"), String::from);
                    }
                    out.push(meta);
                }
                Err(e) => eprintln!("session: пропускаю битый файл {}: {e:#}", path.display()),
            }
        }
        out.sort_by(|a, b| b.created.cmp(&a.created).then(b.id.cmp(&a.id)));
        Ok(out)
    }

    /// Форк сессии: новая ветка с `parent = id` исходной и историей,
    /// обрезанной до первых `at_msg_idx` сообщений (`at_msg_idx == len` —
    /// ветка от кончика истории). Счётчики ветки обнуляются, `id`/`created` —
    /// новые. Файл на диске не создаётся — сохранение ветки на вызывающем.
    pub fn fork(&self, path: impl AsRef<Path>, at_msg_idx: usize) -> Result<Session> {
        let base = self.load(path)?;
        if at_msg_idx > base.messages.len() {
            return Err(anyhow!(
                "точка форка {at_msg_idx} за пределами истории ({} сообщений)",
                base.messages.len()
            ));
        }
        let mut messages = base.messages;
        messages.truncate(at_msg_idx);
        let mut branch = Session::new(base.workspace, base.model);
        branch.parent = Some(base.id);
        branch.messages = messages;
        Ok(branch)
    }

    /// Дочерние ветки сессии `id` (у которых `parent == id`), от старых к
    /// новым. Несуществующий `id` — пустой список.
    pub fn children_of(&self, id: &str) -> Result<Vec<SessionMeta>> {
        let mut all = self.list()?;
        all.retain(|m| m.parent.as_deref() == Some(id));
        all.reverse(); // list() отдаёт новые первыми, дети нужны по возрастанию
        Ok(all)
    }

    /// Самая свежая сессия хранилища; `None`, если сессий нет. Ошибка чтения
    /// каталога выводится в stderr и тоже даёт `None`.
    pub fn find_latest(&self) -> Option<SessionMeta> {
        match self.list() {
            Ok(list) => list.into_iter().next(),
            Err(e) => {
                eprintln!("session: не удалось найти последнюю сессию: {e:#}");
                None
            }
        }
    }

    /// Прочитать только шапку файла сессии (без материализации сообщений).
    fn read_header(path: &Path) -> Result<SessionMeta> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("не удалось прочитать {}", path.display()))?;
        let head: SessionHeader = serde_json::from_str(&text)
            .with_context(|| format!("битый JSON в {}", path.display()))?;
        Ok(SessionMeta {
            id: head.id,
            parent: head.parent,
            created: head.created,
            workspace: head.workspace,
            model: head.model,
            path: path.to_path_buf(),
            message_count: head.messages,
            meta: head.meta,
        })
    }
}

/// Шапка файла сессии: все поля, кроме тел сообщений. Все поля с умолчаниями —
/// `list()` толерантен к старым/урезанным файлам, пробелы добираются из имени.
#[derive(Deserialize)]
struct SessionHeader {
    #[serde(default)]
    id: String,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    created: u64,
    #[serde(default)]
    workspace: PathBuf,
    #[serde(default)]
    model: String,
    #[serde(default, deserialize_with = "count_items")]
    messages: usize,
    #[serde(default)]
    meta: Meta,
}

/// Десериализатор-счётчик: обходит элементы массива без их материализации —
/// тело каждого сообщения читается как [`serde::de::IgnoredAny`], поэтому
/// перечисление сессий не выделяет память под содержимое переписки.
fn count_items<'de, D>(deserializer: D) -> std::result::Result<usize, D::Error>
where
    D: Deserializer<'de>,
{
    let items = Vec::<serde::de::IgnoredAny>::deserialize(deserializer)?;
    Ok(items.len())
}

/// Разобрать имя файла сессии: `session-<created>-<id>.json` или старый
/// `session-<created>.json`. Возвращает `(created, id-из-имени)`.
fn parse_file_name(name: &str) -> Option<(u64, Option<&str>)> {
    let stem = name.strip_prefix(FILE_PREFIX)?.strip_suffix(FILE_SUFFIX)?;
    match stem.split_once('-') {
        Some((ts, id)) if !id.is_empty() => Some((ts.parse::<u64>().ok()?, Some(id))),
        Some(_) => None, // «session-123-.json» — мусорное имя
        None => Some((stem.parse::<u64>().ok()?, None)),
    }
}

/// Проверить id на файлобезопасность: непустой, только `[A-Za-z0-9_-]`.
/// Защита от path traversal при сохранении сессии с чужим или битым id.
fn validate_id(id: &str) -> Result<()> {
    let ok = !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(anyhow!("небезопасный id сессии: {id:?}"))
    }
}

/// Сгенерировать уникальный id сессии: наносекунды + pid + счётчик (hex).
/// Файлобезопасно; внутри процесса коллизии исключены счётчиком, между
/// процессами — наносекундами и pid.
fn new_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let pid = std::process::id();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}-{pid:x}-{seq:x}")
}

/// Текущее UNIX-время в секундах (0, если системные часы переведены назад).
fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Свежий уникальный каталог под тест (параллельные тесты не пересекаются).
    fn fresh_dir(tag: &str) -> PathBuf {
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("theseus_session_test_{pid}_{tag}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Сессия с явным id/created и `n_msgs` пользовательскими сообщениями.
    fn make_session(id: &str, created: u64, n_msgs: usize) -> Session {
        let mut s = Session {
            id: id.to_string(),
            parent: None,
            created,
            workspace: PathBuf::from("/tmp/ws"),
            model: "test-model".to_string(),
            messages: Vec::new(),
            meta: Meta::default(),
        };
        for i in 0..n_msgs {
            s.push_message(Message::user(format!("сообщение-{i}")));
        }
        s
    }

    fn cleanup(dir: &Path) {
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn roundtrip_save_load() {
        let dir = fresh_dir("roundtrip");
        let store = SessionStore::new(&dir).unwrap();
        assert_eq!(store.dir(), dir.as_path());
        let mut s = make_session("abc-123", 1_784_366_471, 2);
        s.parent = Some("parent-9".to_string());
        s.messages.push(Message::assistant_with_tools(
            "считаю",
            vec![ToolCall {
                id: "call-1".to_string(),
                name: "bash".to_string(),
                arguments: "{\"cmd\":\"ls\"}".to_string(),
            }],
        ));
        s.record_turn();
        s.record_api_call(120, 30);
        let path = store.save(&s).unwrap();
        assert_eq!(path.file_name().unwrap().to_str().unwrap(), "session-1784366471-abc-123.json");
        let loaded = store.load(&path).unwrap();
        assert_eq!(s, loaded);
        assert!(loaded.is_fork());
        assert!(loaded.messages[2].has_tool_calls());
        assert!(!loaded.messages[0].has_tool_calls());
        cleanup(&dir);
    }

    #[test]
    fn file_name_format_roundtrip() {
        let s = make_session("x1-y2", 42, 0);
        assert_eq!(s.file_name(), "session-42-x1-y2.json");
        let name = s.file_name();
        let (created, id) = parse_file_name(&name).unwrap();
        assert_eq!(created, 42);
        assert_eq!(id, Some("x1-y2"));
        // старый формат без id в имени
        assert_eq!(parse_file_name("session-100.json"), Some((100, None)));
        // чужие и мусорные имена
        assert!(parse_file_name("events-1.json").is_none());
        assert!(parse_file_name("session-abc.json").is_none());
        assert!(parse_file_name("session-1-.json").is_none());
        assert!(parse_file_name("session-1-a.tmp").is_none());
        assert!(parse_file_name("session--a.json").is_none());
    }

    #[test]
    fn save_twice_keeps_single_file() {
        let dir = fresh_dir("overwrite");
        let store = SessionStore::new(&dir).unwrap();
        let mut s = make_session("solo", 7, 1);
        store.save(&s).unwrap();
        s.push_message(Message::assistant("ответ"));
        let path = store.save(&s).unwrap();
        let files: Vec<_> = fs::read_dir(&dir).unwrap().collect();
        assert_eq!(files.len(), 1, "повторный save не плодит файлы, tmp не остаётся");
        assert_eq!(store.load(&path).unwrap().message_count(), 2);
        cleanup(&dir);
    }

    #[test]
    fn list_sorted_newest_first() {
        let dir = fresh_dir("list_sort");
        let store = SessionStore::new(&dir).unwrap();
        for (id, created) in [("a", 100), ("b", 300), ("c", 200)] {
            store.save(&make_session(id, created, 1)).unwrap();
        }
        let list = store.list().unwrap();
        let ids: Vec<&str> = list.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["b", "c", "a"]);
        assert!(list.iter().all(SessionMeta::is_root));
        assert!(list.iter().all(|m| m.path.starts_with(&dir)));
        cleanup(&dir);
    }

    #[test]
    fn list_skips_broken_and_foreign_files() {
        let dir = fresh_dir("list_broken");
        let store = SessionStore::new(&dir).unwrap();
        store.save(&make_session("good", 10, 2)).unwrap();
        // битый JSON под нашим именем — пропуск с warning
        fs::write(dir.join("session-20-bad.json"), "{ not json").unwrap();
        // валидный JSON не нашей схемы
        fs::write(dir.join("session-30-weird.json"), "[1,2,3]").unwrap();
        // посторонние имена — тихий пропуск
        fs::write(dir.join("events-40.json"), "{}").unwrap();
        fs::write(dir.join("session-abc.json"), "{}").unwrap();
        // подкаталог с «нашим» именем — тоже пропуск
        fs::create_dir(dir.join("session-50-dir.json")).unwrap();
        let list = store.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "good");
        assert_eq!(list[0].message_count, 2);
        cleanup(&dir);
    }

    #[test]
    fn load_rejects_broken_json() {
        let dir = fresh_dir("load_broken");
        let store = SessionStore::new(&dir).unwrap();
        let path = dir.join("session-1-x.json");
        fs::write(&path, "{ broken").unwrap();
        let err = store.load(&path).unwrap_err();
        assert!(format!("{err:#}").contains("битый файл сессии"));
        assert!(store.load(dir.join("session-2-y.json")).is_err(), "нет файла — ошибка");
        cleanup(&dir);
    }

    #[test]
    fn list_tolerates_legacy_files() {
        let dir = fresh_dir("legacy");
        let store = SessionStore::new(&dir).unwrap();
        // старый формат: только messages, имя без id
        fs::write(
            dir.join("session-555.json"),
            r#"{"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":"yo"}]}"#,
        )
        .unwrap();
        // новое имя, но шапка без id — id добирается из имени файла
        fs::write(dir.join("session-777-named.json"), r#"{"created":777,"messages":[]}"#).unwrap();
        let list = store.list().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "named");
        assert_eq!(list[0].created, 777);
        assert_eq!(list[1].id, "legacy-555");
        assert_eq!(list[1].created, 555);
        assert_eq!(list[1].message_count, 2);
        cleanup(&dir);
    }

    #[test]
    fn fork_truncates_and_links_parent() {
        let dir = fresh_dir("fork_basic");
        let store = SessionStore::new(&dir).unwrap();
        let base = make_session("base-1", 100, 5);
        let path = store.save(&base).unwrap();
        let branch = store.fork(&path, 3).unwrap();
        assert_eq!(branch.parent.as_deref(), Some("base-1"));
        assert_eq!(branch.message_count(), 3);
        assert_eq!(branch.messages[2].content, "сообщение-2");
        assert_ne!(branch.id, "base-1", "у ветки должен быть новый id");
        assert!(branch.created >= base.created);
        assert_eq!(branch.workspace, base.workspace);
        assert_eq!(branch.model, base.model);
        assert_eq!(branch.meta, Meta::default(), "счётчики ветки обнуляются");
        assert!(branch.is_fork());
        // исходник не тронут, ветка на диске не появилась
        assert_eq!(store.load(&path).unwrap().message_count(), 5);
        assert_eq!(store.list().unwrap().len(), 1);
        cleanup(&dir);
    }

    #[test]
    fn fork_edge_points() {
        let dir = fresh_dir("fork_edge");
        let store = SessionStore::new(&dir).unwrap();
        let path = store.save(&make_session("e", 1, 3)).unwrap();
        assert_eq!(store.fork(&path, 0).unwrap().message_count(), 0, "форк с нуля — пусто");
        assert_eq!(store.fork(&path, 3).unwrap().message_count(), 3, "форк с кончика — всё");
        assert!(store.fork(&path, 4).is_err(), "за пределами истории — ошибка");
        assert!(store.fork(dir.join("session-9-ghost.json"), 0).is_err(), "нет файла — ошибка");
        cleanup(&dir);
    }

    #[test]
    fn fork_chain_and_children_of() {
        let dir = fresh_dir("fork_tree");
        let store = SessionStore::new(&dir).unwrap();
        let root = make_session("root", 100, 4);
        let root_path = store.save(&root).unwrap();
        // root -> b (200) и root -> d (300); b -> c (400)
        let mut b = store.fork(&root_path, 2).unwrap();
        b.created = 200;
        let b_path = store.save(&b).unwrap();
        let mut d = store.fork(&root_path, 1).unwrap();
        d.created = 300;
        store.save(&d).unwrap();
        let mut c = store.fork(&b_path, 1).unwrap();
        c.created = 400;
        store.save(&c).unwrap();

        let kids = store.children_of("root").unwrap();
        let kid_ids: Vec<&str> = kids.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(kid_ids, [b.id.as_str(), d.id.as_str()], "дети — по возрастанию created");
        assert!(kids.iter().all(|m| m.parent.as_deref() == Some("root")));
        assert!(!kids[0].is_root());

        let grand = store.children_of(&b.id).unwrap();
        assert_eq!(grand.len(), 1);
        assert_eq!(grand[0].id, c.id);
        assert!(store.children_of(&c.id).unwrap().is_empty());
        assert!(store.children_of("нет-такого").unwrap().is_empty());
        // самая свежая сессия — лист дерева c
        assert_eq!(store.find_latest().unwrap().id, c.id);
        cleanup(&dir);
    }

    #[test]
    fn find_latest_on_empty_store() {
        let dir = fresh_dir("latest_empty");
        let store = SessionStore::new(&dir).unwrap();
        assert!(store.find_latest().is_none());
        assert!(store.list().unwrap().is_empty());
        // удалённый после открытия каталог — пустой список, а не ошибка/паника
        fs::remove_dir_all(&dir).unwrap();
        assert!(store.list().unwrap().is_empty());
        assert!(store.find_latest().is_none());
    }

    #[test]
    fn meta_counters_accumulate_and_persist() {
        let dir = fresh_dir("meta");
        let store = SessionStore::new(&dir).unwrap();
        let mut s = make_session("m", 1, 0);
        s.record_turn();
        s.record_turn();
        s.record_api_call(100, 50);
        s.record_api_call(200, 70);
        assert_eq!(s.meta, Meta { turns: 2, api_calls: 2, tokens: 420 });
        let path = store.save(&s).unwrap();
        assert_eq!(store.load(&path).unwrap().meta, s.meta, "полный парс");
        assert_eq!(store.list().unwrap()[0].meta, s.meta, "лёгкий list видит те же счётчики");
        cleanup(&dir);
    }

    #[test]
    fn save_rejects_unsafe_id() {
        let dir = fresh_dir("unsafe_id");
        let store = SessionStore::new(&dir).unwrap();
        for bad in ["../evil", "a/b", "", "a b", "a.b"] {
            assert!(store.save(&make_session(bad, 1, 0)).is_err(), "id {bad:?} должен быть отклонён");
        }
        assert!(store.save(&make_session("ok_ID-9", 1, 0)).is_ok());
        cleanup(&dir);
    }

    #[test]
    fn message_serde_shape() {
        // у сообщения без вызовов ключ tool_calls в JSON отсутствует
        let v = serde_json::to_value(Message::user("привет")).unwrap();
        assert_eq!(v, serde_json::json!({"role": "user", "content": "привет"}));
        // с вызовами — присутствует; roundtrip через Value
        let m = Message::assistant_with_tools(
            "",
            vec![ToolCall { id: "c1".to_string(), name: "read_file".to_string(), arguments: "{}".to_string() }],
        );
        let v = serde_json::to_value(&m).unwrap();
        assert!(v.get("tool_calls").is_some());
        let back: Message = serde_json::from_value(v).unwrap();
        assert_eq!(back, m);
        // parent у корня сериализуется явным null (по формату файла)
        let v = serde_json::to_value(make_session("n", 1, 0)).unwrap();
        assert!(v.get("parent").is_some_and(serde_json::Value::is_null));
        // роль сериализуется строчными буквами
        assert_eq!(serde_json::to_value(Role::Tool).unwrap(), "tool");
        assert_eq!(Role::Assistant.as_str(), "assistant");
        assert_eq!(Role::System.to_string(), "system");
    }

    #[test]
    fn list_counts_messages_without_touching_bodies() {
        let dir = fresh_dir("big_msgs");
        let store = SessionStore::new(&dir).unwrap();
        let mut s = make_session("big", 1, 0);
        let fat = "юникод-💾-".repeat(20_000); // ~200 КБ на сообщение
        for _ in 0..5 {
            s.push_message(Message::assistant(&fat));
        }
        // тело, похожее на поля шапки, не должно сбивать парсер
        s.push_message(Message::user(r#"{"id":"fake","created":999999999}"#));
        store.save(&s).unwrap();
        let list = store.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].message_count, 6);
        assert_eq!(list[0].id, "big", "id — из шапки, а не из тела сообщения");
        assert_eq!(list[0].created, 1);
        cleanup(&dir);
    }
}
