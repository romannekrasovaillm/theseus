//! Инструмент todo-списка с гейтом — полноценная замена примитивному `todo_gate`
//! из `agent/mod.rs` (образец — TodoWrite из Claude Code): атомарная замена всего
//! списка, валидация, markdown-рендер, гейт перед инструментами, события для TUI,
//! статистика длительностей. Метки времени — секунды UNIX-эпохи (`u64`); методы
//! `*_at` принимают `now` явно ради детерминированных тестов и replay сессий.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;

/// Инструменты, которым не нужна активная in_progress-задача (сам todo-инструмент
/// и служебные). `finish` обрабатывается отдельно — жёстким гейтом.
const GATE_EXEMPT_TOOLS: &[&str] = &["todo_write", "todo_read", "think"];

/// Текущее время в секундах UNIX-эпохи (0 при ошибке часов — не паникуем).
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

// === Статус задачи ===

/// Статус задачи в todo-списке.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    /// Ещё не начата.
    Pending,
    /// Выполняется прямо сейчас (максимум одна на список — см. [`TodoList::validate`]).
    InProgress,
    /// Завершена.
    Done,
    /// Отменена (закрыта без результата).
    Cancelled,
}

impl TodoStatus {
    /// Маркер для markdown-чеклиста: ☐ pending, ◐ in_progress, ✔ done, ✖ cancelled.
    pub fn marker(self) -> char {
        match self {
            TodoStatus::Pending => '☐',
            TodoStatus::InProgress => '◐',
            TodoStatus::Done => '✔',
            TodoStatus::Cancelled => '✖',
        }
    }

    /// Строковое имя (совпадает с serde-представлением).
    pub fn as_str(self) -> &'static str {
        match self {
            TodoStatus::Pending => "pending",
            TodoStatus::InProgress => "in_progress",
            TodoStatus::Done => "done",
            TodoStatus::Cancelled => "cancelled",
        }
    }

    /// Закрыт ли статус (done или cancelled) — для гейта `finish` и статистики.
    pub fn is_closed(self) -> bool {
        matches!(self, TodoStatus::Done | TodoStatus::Cancelled)
    }
}

impl fmt::Display for TodoStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for TodoStatus {
    type Err = TodoError;

    /// Разбор статуса из аргументов инструмента: терпим к регистру и дефису
    /// (`in-progress`), `canceled` с одной `l` тоже принимаем.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "pending" => Ok(TodoStatus::Pending),
            "in_progress" => Ok(TodoStatus::InProgress),
            "done" => Ok(TodoStatus::Done),
            "cancelled" | "canceled" => Ok(TodoStatus::Cancelled),
            other => Err(TodoError::UnknownStatus(other.to_string())),
        }
    }
}

// === Ошибки ===

/// Ошибки операций над todo-списком.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TodoError {
    /// Пустой `id` задачи.
    EmptyId,
    /// `id` встречается в списке более одного раза.
    DuplicateId(String),
    /// Задача с таким `id` не найдена.
    UnknownId(String),
    /// Нераспознанный статус (для `FromStr`).
    UnknownStatus(String),
}

impl fmt::Display for TodoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TodoError::EmptyId => write!(f, "пустой id задачи"),
            TodoError::DuplicateId(id) => write!(f, "дублирующийся id «{id}»"),
            TodoError::UnknownId(id) => write!(f, "неизвестный id задачи «{id}»"),
            TodoError::UnknownStatus(s) => write!(
                f,
                "неизвестный статус «{s}» (ожидается pending|in_progress|done|cancelled)"
            ),
        }
    }
}

impl std::error::Error for TodoError {}

// === Задача ===

/// Задача todo-списка: `id` стабилен между перезаписями списка, `content` —
/// формулировка в императиве (как у Claude), `active_form` — «что делаю сейчас»
/// для показа в TUI, `created_at`/`closed_at` — метки времени (0/None — выставит
/// список), `artifact_verified` — артефакт проверен внешним хуком.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    /// Уникальный идентификатор.
    pub id: String,
    /// Формулировка задачи.
    pub content: String,
    /// Текущий статус.
    pub status: TodoStatus,
    /// «Что делаю сейчас» — показывается, пока задача in_progress.
    #[serde(default)]
    pub active_form: Option<String>,
    /// Момент создания (сек. эпохи).
    #[serde(default)]
    pub created_at: u64,
    /// Момент закрытия (done/cancelled); `None`, пока задача открыта.
    #[serde(default)]
    pub closed_at: Option<u64>,
    /// Артефакт задачи проверен (хук сборки/тестов/ревью).
    #[serde(default)]
    pub artifact_verified: bool,
}

impl TodoItem {
    /// Новая задача; `created_at` выставит список при вставке (см. `set_full_at`).
    pub fn new(id: &str, content: &str, status: TodoStatus) -> Self {
        TodoItem {
            id: id.to_string(),
            content: content.to_string(),
            status,
            active_form: None,
            created_at: 0,
            closed_at: None,
            artifact_verified: false,
        }
    }

    /// Builder: задать `active_form`.
    pub fn with_active_form(mut self, form: &str) -> Self {
        self.active_form = Some(form.to_string());
        self
    }

    /// Закрыта ли задача (done/cancelled).
    pub fn is_closed(&self) -> bool {
        self.status.is_closed()
    }

    /// Время от создания до закрытия; `None`, если задача ещё открыта.
    pub fn close_duration(&self) -> Option<u64> {
        self.closed_at.map(|c| c.saturating_sub(self.created_at))
    }

    /// Сколько секунд задача жила: до закрытия, а для открытой — до `now`.
    /// Насыщающее вычитание: при «сдвиге часов» назад даёт 0, а не панику.
    pub fn lifetime(&self, now: u64) -> u64 {
        self.closed_at.unwrap_or(now).saturating_sub(self.created_at)
    }
}

// === Валидация ===

/// Отчёт валидации: `errors` ломают инварианты (пустой/дублирующийся id),
/// `warnings` — мягкие нарушения (больше одного in_progress, пустой content).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ValidationReport {
    /// Жёсткие нарушения.
    pub errors: Vec<String>,
    /// Мягкие нарушения.
    pub warnings: Vec<String>,
}

impl ValidationReport {
    /// Нет ли жёстких ошибок.
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }

    /// Есть ли предупреждения.
    pub fn has_warnings(&self) -> bool {
        !self.warnings.is_empty()
    }
}

// === События, гейт, статистика ===

/// Событие изменения списка — забирается через [`TodoList::drain_events`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TodoEvent {
    /// Список полностью заменён (`set_full`): всего задач и сколько из них done.
    ListReplaced { total: usize, done: usize },
    /// У задачи `id` сменился статус `from` → `to`.
    StatusChanged { id: String, from: TodoStatus, to: TodoStatus },
    /// Хук наружу: задача помечена done, но её артефакт не проверен.
    ArtifactUnverified { id: String, content: String },
}

/// Вердикт гейта перед вызовом инструмента.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateVerdict {
    /// Выполняй инструмент, замечаний нет.
    Allow,
    /// Мягкое напоминание (не блокирует) — показать агенту строкой.
    Remind(String),
}

/// Жёсткий отказ гейта: `finish` при незакрытых задачах (урок Grok, как
/// примитивный `todo_gate` в `agent/mod.rs`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateReject {
    /// Формулировки незакрытых (pending/in_progress) задач.
    pub pending: Vec<String>,
}

impl fmt::Display for GateReject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "TodoGate: finish отклонён — незакрытые задачи: {}. \
             Закройте их или обновите todo_write.",
            self.pending.join("; ")
        )
    }
}

impl std::error::Error for GateReject {}

/// Статистика списка: счётчики по статусам (`total/pending/in_progress/done/
/// cancelled`), суммарное «открытое» время `open_secs` на момент `now`,
/// среднее/максимальное время закрытия `avg_close_secs`/`max_close_secs`
/// (None — закрытых нет).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TodoStats {
    pub total: usize,
    pub pending: usize,
    pub in_progress: usize,
    pub done: usize,
    pub cancelled: usize,
    pub open_secs: u64,
    pub avg_close_secs: Option<u64>,
    pub max_close_secs: Option<u64>,
}

// === Список ===

/// Todo-список с атомарной заменой, гейтом, событиями и статистикой.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TodoList {
    items: Vec<TodoItem>,
    /// Очередь событий для TUI; не сериализуется.
    #[serde(skip)]
    events: Vec<TodoEvent>,
}

impl TodoList {
    /// Пустой список.
    pub fn new() -> Self {
        Self::default()
    }

    /// Текущие задачи (в порядке списка).
    pub fn items(&self) -> &[TodoItem] {
        &self.items
    }

    /// Число задач.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Пуст ли список.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Найти задачу по id.
    pub fn get(&self, id: &str) -> Option<&TodoItem> {
        self.items.iter().find(|t| t.id == id)
    }

    /// Атомарная замена всего списка (как TodoWrite у Claude), время — системное.
    /// При ошибке валидации id старый список остаётся нетронутым.
    pub fn set_full(&mut self, items: Vec<TodoItem>) -> Result<ValidationReport, TodoError> {
        self.set_full_at(items, now_secs())
    }

    /// То же с явным `now`. Инварианты замены: `id` непусты и уникальны, иначе
    /// `Err` и список не меняется; по совпадению `id` наследуются `created_at` и
    /// (при неизменном статусе) `closed_at`/`artifact_verified` — LLM перезаписывает
    /// список целиком, а история не теряется; свежее закрытие ставит
    /// `closed_at = now` и сбрасывает проверку артефакта; переоткрытие
    /// (closed → pending/in_progress) очищает `closed_at`.
    pub fn set_full_at(&mut self, items: Vec<TodoItem>, now: u64) -> Result<ValidationReport, TodoError> {
        Self::check_ids(&items)?;
        let prev: HashMap<String, TodoItem> = std::mem::take(&mut self.items)
            .into_iter()
            .map(|it| (it.id.clone(), it))
            .collect();
        let mut changes: Vec<TodoEvent> = Vec::new();
        let mut next: Vec<TodoItem> = Vec::with_capacity(items.len());
        for mut it in items {
            let old = prev.get(&it.id);
            Self::normalize(&mut it, old, now);
            if let Some(p) = old {
                if p.status != it.status {
                    changes.push(TodoEvent::StatusChanged { id: it.id.clone(), from: p.status, to: it.status });
                }
            }
            next.push(it);
        }
        let done = next.iter().filter(|t| t.status == TodoStatus::Done).count();
        let total = next.len();
        self.items = next;
        self.events.push(TodoEvent::ListReplaced { total, done });
        self.events.append(&mut changes);
        Ok(self.validate())
    }

    /// Сменить статус одной задачи (время — системное). Возвращает прежний статус.
    pub fn set_status(&mut self, id: &str, status: TodoStatus) -> Result<TodoStatus, TodoError> {
        self.set_status_at(id, status, now_secs())
    }

    /// То же с явным `now`.
    pub fn set_status_at(&mut self, id: &str, status: TodoStatus, now: u64) -> Result<TodoStatus, TodoError> {
        let idx = self.items.iter().position(|t| t.id == id)
            .ok_or_else(|| TodoError::UnknownId(id.to_string()))?;
        let prev = self.items[idx].clone();
        let from = prev.status;
        let mut it = prev.clone();
        it.status = status;
        Self::normalize(&mut it, Some(&prev), now);
        self.items[idx] = it;
        if from != status {
            self.events.push(TodoEvent::StatusChanged { id: id.to_string(), from, to: status });
        }
        Ok(from)
    }

    /// Отметить, что артефакт done-задачи проверен (хук сборки/тестов/ревью).
    pub fn mark_artifact_verified(&mut self, id: &str) -> Result<(), TodoError> {
        let it = self.items.iter_mut().find(|t| t.id == id)
            .ok_or_else(|| TodoError::UnknownId(id.to_string()))?;
        it.artifact_verified = true;
        Ok(())
    }

    /// Валидация текущего списка: id уникальны и непусты (ошибки), максимум один
    /// in_progress и у него есть active_form (предупреждения, не ошибки).
    pub fn validate(&self) -> ValidationReport {
        let mut report = ValidationReport::default();
        let mut seen: HashSet<&str> = HashSet::new();
        for it in &self.items {
            if it.id.trim().is_empty() {
                report.errors.push("пустой id задачи".to_string());
            } else if !seen.insert(it.id.as_str()) {
                report.errors.push(format!("дублирующийся id «{}»", it.id));
            }
            if it.content.trim().is_empty() {
                report
                    .warnings
                    .push(format!("у задачи «{}» пустое содержимое", it.id));
            }
        }
        let in_prog: Vec<&TodoItem> =
            self.items.iter().filter(|t| t.status == TodoStatus::InProgress).collect();
        if in_prog.len() > 1 {
            let ids = in_prog.iter().map(|t| t.id.as_str()).collect::<Vec<_>>().join(", ");
            report.warnings.push(format!(
                "допустим максимум один in_progress, сейчас {}: {ids}",
                in_prog.len()
            ));
        }
        for t in &in_prog {
            if t.active_form.as_deref().is_none_or(|f| f.trim().is_empty()) {
                report
                    .warnings
                    .push(format!("у in_progress-задачи «{}» нет active_form", t.id));
            }
        }
        report
    }

    /// Прогресс: (число done, всего задач). Cancelled done не считается.
    pub fn progress(&self) -> (usize, usize) {
        let done = self.items.iter().filter(|t| t.status == TodoStatus::Done).count();
        (done, self.items.len())
    }

    /// Рендер markdown-чеклиста: `- ☐/◐/✔/✖ содержимое`, у in_progress — курсивом
    /// `active_form`, у done — длительность, cancelled зачёркивается.
    pub fn render(&self) -> String {
        use std::fmt::Write;
        let (done, total) = self.progress();
        let mut out = String::new();
        let _ = writeln!(out, "## TODO-список — прогресс {done}/{total}");
        if self.items.is_empty() {
            let _ = writeln!(out, "_(план пуст)_");
            return out;
        }
        for it in &self.items {
            let marker = it.status.marker();
            let line = match it.status {
                TodoStatus::Cancelled => format!("~~{}~~ _(отменено)_", it.content),
                TodoStatus::InProgress => {
                    match it.active_form.as_deref().filter(|f| !f.trim().is_empty()) {
                        Some(form) => format!("{} — *{form}*", it.content),
                        None => it.content.clone(),
                    }
                }
                TodoStatus::Done => match it.close_duration() {
                    Some(d) => format!("{} ({d}с)", it.content),
                    None => it.content.clone(),
                },
                TodoStatus::Pending => it.content.clone(),
            };
            let _ = writeln!(out, "- {marker} {line}");
        }
        out
    }

    /// Гейт перед вызовом инструмента: `finish` с незакрытыми задачами — жёсткий
    /// `Err(GateReject)`; пустой список или служебный инструмент — `Allow`; список
    /// непуст и ни одна задача не in_progress — мягкое напоминание строкой
    /// (`Remind`, не блокирует); каждая done-задача с непроверенным артефактом
    /// порождает событие [`TodoEvent::ArtifactUnverified`] (хук наружу) и строку
    /// в напоминании.
    pub fn gate_check(&mut self, tool_name: &str) -> Result<GateVerdict, GateReject> {
        if tool_name == "finish" {
            let pending: Vec<String> = self
                .items
                .iter()
                .filter(|t| !t.is_closed())
                .map(|t| t.content.clone())
                .collect();
            return if pending.is_empty() {
                Ok(GateVerdict::Allow)
            } else {
                Err(GateReject { pending })
            };
        }
        if self.items.is_empty() || GATE_EXEMPT_TOOLS.contains(&tool_name) {
            return Ok(GateVerdict::Allow);
        }
        let mut notes: Vec<String> = Vec::new();
        if !self.items.iter().any(|t| t.status == TodoStatus::InProgress) {
            let mut msg = format!(
                "TodoGate: ни одна задача не отмечена in_progress — \
                 обновите todo_write перед вызовом «{tool_name}»."
            );
            if let Some(next) = self.items.iter().find(|t| t.status == TodoStatus::Pending) {
                msg = format!("{msg} Ближайшая pending-задача: «{}».", next.content);
            }
            notes.push(msg);
        }
        let unverified: Vec<(String, String)> = self
            .items
            .iter()
            .filter(|t| t.status == TodoStatus::Done && !t.artifact_verified)
            .map(|t| (t.id.clone(), t.content.clone()))
            .collect();
        if !unverified.is_empty() {
            let names = unverified
                .iter()
                .map(|(_, content)| content.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            notes.push(format!(
                "TodoGate-hook: артефакты done-задач не проверены \
                 (вызовите mark_artifact_verified): {names}."
            ));
            for (id, content) in unverified {
                self.events.push(TodoEvent::ArtifactUnverified { id, content });
            }
        }
        Ok(if notes.is_empty() {
            GateVerdict::Allow
        } else {
            GateVerdict::Remind(notes.join(" "))
        })
    }

    /// Статистика на момент `now`: счётчики по статусам и длительности.
    pub fn stats(&self, now: u64) -> TodoStats {
        let mut st = TodoStats {
            total: self.items.len(),
            ..TodoStats::default()
        };
        let mut close_sum = 0u64;
        let mut closed_n = 0u64;
        for it in &self.items {
            match it.status {
                TodoStatus::Pending => st.pending += 1,
                TodoStatus::InProgress => st.in_progress += 1,
                TodoStatus::Done => st.done += 1,
                TodoStatus::Cancelled => st.cancelled += 1,
            }
            if let Some(d) = it.close_duration() {
                close_sum = close_sum.saturating_add(d);
                closed_n += 1;
                let m = st.max_close_secs.unwrap_or(d);
                st.max_close_secs = Some(m.max(d));
            } else {
                st.open_secs = st.open_secs.saturating_add(it.lifetime(now));
            }
        }
        // checked_div: без закрытых задач (closed_n == 0) среднее — None.
        st.avg_close_secs = close_sum.checked_div(closed_n);
        st
    }

    /// Накопленные события (без изъятия).
    pub fn events(&self) -> &[TodoEvent] {
        &self.events
    }

    /// Забрать накопленные события (очередь очищается).
    pub fn drain_events(&mut self) -> Vec<TodoEvent> {
        std::mem::take(&mut self.events)
    }

    /// Проверка id: непустые и уникальные.
    fn check_ids(items: &[TodoItem]) -> Result<(), TodoError> {
        let mut seen: HashSet<&str> = HashSet::new();
        for it in items {
            if it.id.trim().is_empty() {
                return Err(TodoError::EmptyId);
            }
            if !seen.insert(it.id.as_str()) {
                return Err(TodoError::DuplicateId(it.id.clone()));
            }
        }
        Ok(())
    }

    /// Нормализация задачи при вставке/смене статуса: наследование меток времени
    /// и проверки артефакта, простановка `closed_at` по переходам статуса.
    fn normalize(it: &mut TodoItem, prev: Option<&TodoItem>, now: u64) {
        match prev {
            Some(p) => {
                it.created_at = p.created_at;
                if p.status == it.status {
                    it.closed_at = p.closed_at;
                    it.artifact_verified = p.artifact_verified || it.artifact_verified;
                } else if it.status.is_closed() {
                    it.closed_at = Some(now); // свежее закрытие
                    it.artifact_verified = false;
                } else if p.status.is_closed() {
                    it.closed_at = None; // переоткрытие
                    it.artifact_verified = false;
                } else {
                    it.closed_at = None; // pending <-> in_progress
                    it.artifact_verified = p.artifact_verified || it.artifact_verified;
                }
            }
            None => {
                if it.created_at == 0 {
                    it.created_at = now;
                }
                if it.status.is_closed() && it.closed_at.is_none() {
                    it.closed_at = Some(now);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TodoStatus::*;
    use super::*;

    /// Короткий конструктор задачи.
    fn item(id: &str, status: TodoStatus) -> TodoItem {
        TodoItem::new(id, &format!("задача {id}"), status)
    }

    /// Список с задачами, вставленными в момент `now`.
    fn list(items: Vec<TodoItem>, now: u64) -> TodoList {
        let mut l = TodoList::new();
        l.set_full_at(items, now).unwrap();
        l
    }

    #[test]
    fn set_full_replaces_previous_list() {
        let mut l = list(vec![item("a", Pending)], 100);
        l.set_full_at(vec![item("b", Pending), item("c", Done)], 200).unwrap();
        let ids: Vec<&str> = l.items().iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["b", "c"]);
        assert!(l.get("a").is_none());
    }

    #[test]
    fn set_full_preserves_created_at_by_id() {
        let mut l = list(vec![item("a", Pending)], 100);
        // LLM перезаписал список: «a» осталась, «b» новая.
        l.set_full_at(vec![item("a", InProgress), item("b", Pending)], 200).unwrap();
        assert_eq!(l.get("a").unwrap().created_at, 100, "created_at наследуется по id");
        assert_eq!(l.get("b").unwrap().created_at, 200, "новая задача получает now");
    }

    #[test]
    fn set_full_rejects_bad_ids_atomically() {
        let mut l = list(vec![item("a", Pending)], 100);
        let err = l
            .set_full_at(vec![item("x", Pending), item("x", Done)], 200)
            .unwrap_err();
        assert!(matches!(err, TodoError::DuplicateId(ref id) if id == "x"));
        // атомарность: старый список нетронут
        assert_eq!(l.len(), 1);
        assert!(l.get("a").is_some());
        let err = l.set_full_at(vec![item("  ", Pending)], 200).unwrap_err();
        assert!(matches!(err, TodoError::EmptyId));
        assert_eq!(l.len(), 1);
    }

    #[test]
    fn validate_warns_on_multiple_in_progress_but_not_error() {
        let mut l = TodoList::new();
        let report = l
            .set_full_at(
                vec![
                    TodoItem::new("a", "первая", InProgress),
                    TodoItem::new("b", "вторая", InProgress),
                ],
                100,
            )
            .unwrap();
        assert!(report.is_ok(), "два in_progress — не ошибка");
        assert!(report.has_warnings());
        assert!(report.warnings.iter().any(|w| w.contains("in_progress")));
        let v = l.validate();
        assert!(v.warnings.iter().any(|w| w.contains("a, b")));
    }

    #[test]
    fn validate_warns_on_missing_active_form_and_empty_content() {
        let l = list(
            vec![
                TodoItem::new("a", "нормальная", InProgress),
                TodoItem::new("b", "   ", Pending),
            ],
            100,
        );
        let v = l.validate();
        assert!(v.warnings.iter().any(|w| w.contains("active_form")));
        assert!(v.warnings.iter().any(|w| w.contains("пустое содержимое")));
    }

    #[test]
    fn validate_reports_duplicate_ids() {
        let mut l = list(vec![item("a", Pending)], 100);
        // обходим set_full (он откажет) — собираем список с дублем вручную
        l.items.push(item("a", Done));
        let v = l.validate();
        assert!(!v.is_ok());
        assert!(v.errors.iter().any(|e| e.contains("дублирующийся")));
    }

    #[test]
    fn progress_counts_done_over_total() {
        let l = list(vec![item("a", Done), item("b", Cancelled), item("c", Pending)], 100);
        assert_eq!(l.progress(), (1, 3), "cancelled не считается done");
        assert_eq!(TodoList::new().progress(), (0, 0));
    }

    #[test]
    fn render_shows_all_markers_progress_and_empty() {
        let l = list(
            vec![
                TodoItem::new("1", "сделать раз", Done),
                TodoItem::new("2", "сделать два", InProgress).with_active_form("делаю два"),
                TodoItem::new("3", "сделать три", Pending),
                TodoItem::new("4", "сделать четыре", Cancelled),
            ],
            1000,
        );
        let md = l.render();
        assert!(md.contains("## TODO-список — прогресс 1/4"));
        assert!(md.contains("- ✔ сделать раз (0с)"), "done с длительностью: {md}");
        assert!(md.contains("- ◐ сделать два — *делаю два*"), "active_form: {md}");
        assert!(md.contains("- ☐ сделать три"));
        assert!(md.contains("- ✖ ~~сделать четыре~~ _(отменено)_"), "strikethrough: {md}");
        let empty = TodoList::new().render();
        assert!(empty.contains("прогресс 0/0"));
        assert!(empty.contains("_(план пуст)_"));
    }

    #[test]
    fn serde_status_uses_snake_case() {
        assert_eq!(serde_json::to_string(&InProgress).unwrap(), "\"in_progress\"");
        assert_eq!(serde_json::to_string(&Cancelled).unwrap(), "\"cancelled\"");
        let s: TodoStatus = serde_json::from_str("\"in_progress\"").unwrap();
        assert_eq!(s, InProgress);
    }

    #[test]
    fn serde_item_roundtrip_and_defaults() {
        let it = TodoItem::new("t1", "написать модуль", InProgress).with_active_form("пишу модуль");
        let json = serde_json::to_string(&it).unwrap();
        assert!(json.contains("\"status\":\"in_progress\""));
        let back: TodoItem = serde_json::from_str(&json).unwrap();
        assert_eq!(back, it);
        // минимальный JSON (как присылает LLM): опциональные поля по умолчанию
        let minimal: TodoItem =
            serde_json::from_str(r#"{"id":"x","content":"c","status":"pending"}"#).unwrap();
        assert_eq!(minimal.active_form, None);
        assert_eq!(minimal.created_at, 0);
        assert_eq!(minimal.closed_at, None);
        assert!(!minimal.artifact_verified);
    }

    #[test]
    fn serde_list_roundtrip_skips_events() {
        let l = list(vec![item("a", Pending)], 100);
        assert!(!l.events().is_empty(), "события накопились");
        let json = serde_json::to_string(&l).unwrap();
        assert!(!json.contains("ListReplaced"), "события не сериализуются");
        let mut back: TodoList = serde_json::from_str(&json).unwrap();
        assert_eq!(back.items(), l.items());
        assert!(back.drain_events().is_empty(), "очередь событий пуста после загрузки");
    }

    #[test]
    fn gate_allows_empty_list_and_exempt_tool() {
        let mut l = TodoList::new();
        assert_eq!(l.gate_check("bash").unwrap(), GateVerdict::Allow);
        l.set_full_at(vec![item("a", Pending)], 100).unwrap();
        assert_eq!(
            l.gate_check("todo_write").unwrap(),
            GateVerdict::Allow,
            "todo-инструмент освобождён от напоминания"
        );
    }

    #[test]
    fn gate_reminds_when_nothing_in_progress() {
        let mut l = list(vec![item("a", Pending), item("b", Pending)], 100);
        match l.gate_check("bash") {
            Ok(GateVerdict::Remind(msg)) => {
                assert!(msg.contains("in_progress"), "напоминание про статус: {msg}");
                assert!(msg.contains("задача a"), "подсказка ближайшей pending: {msg}");
                assert!(msg.contains("bash"), "упоминается инструмент: {msg}");
            }
            other => panic!("ожидали Remind, получили {other:?}"),
        }
        // мягкое напоминание не порождает хук-событий
        assert!(l.drain_events().iter().all(|e| !matches!(e, TodoEvent::ArtifactUnverified { .. })));
    }

    #[test]
    fn gate_hooks_unverified_done_artifacts() {
        let mut l = list(vec![item("a", Done), item("b", InProgress)], 100);
        l.drain_events(); // очистить события замены списка
        match l.gate_check("edit_file") {
            Ok(GateVerdict::Remind(msg)) => {
                assert!(msg.contains("артефакт"), "есть хук-строка: {msg}");
                assert!(!msg.contains("in_progress —"), "in_progress есть — про него молчим: {msg}");
            }
            other => panic!("ожидали Remind, получили {other:?}"),
        }
        let evs = l.drain_events();
        assert!(
            evs.iter()
                .any(|e| matches!(e, TodoEvent::ArtifactUnverified { id, .. } if id == "a")),
            "хук наружу по задаче a: {evs:?}"
        );
        // после подтверждения артефакта — тишина
        l.mark_artifact_verified("a").unwrap();
        assert_eq!(l.gate_check("edit_file").unwrap(), GateVerdict::Allow);
        assert!(l.drain_events().is_empty());
        // несуществующий id — ошибка
        assert!(matches!(
            l.mark_artifact_verified("zzz").unwrap_err(),
            TodoError::UnknownId(_)
        ));
    }

    #[test]
    fn gate_blocks_finish_until_all_closed() {
        let mut l = list(vec![item("a", Done), item("b", Pending)], 100);
        let err = l.gate_check("finish").unwrap_err();
        let text = err.to_string();
        assert!(text.contains("незакрытые задачи"), "{text}");
        assert!(text.contains("задача b"), "{text}");
        assert!(!text.contains("задача a"), "done не блокирует: {text}");
        l.set_status("b", Cancelled).unwrap();
        assert_eq!(l.gate_check("finish").unwrap(), GateVerdict::Allow);
    }

    #[test]
    fn set_status_transitions_update_timestamps() {
        let mut l = list(vec![item("a", Pending)], 100);
        let from = l.set_status_at("a", InProgress, 110).unwrap();
        assert_eq!(from, Pending);
        l.set_status_at("a", Done, 160).unwrap();
        let a = l.get("a").unwrap();
        assert_eq!(a.closed_at, Some(160));
        assert_eq!(a.close_duration(), Some(60));
        // переоткрытие очищает closed_at
        l.set_status_at("a", Pending, 200).unwrap();
        let a = l.get("a").unwrap();
        assert_eq!(a.closed_at, None);
        assert_eq!(a.close_duration(), None);
        // события смены статуса зафиксированы
        let changes = l
            .drain_events()
            .into_iter()
            .filter(|e| matches!(e, TodoEvent::StatusChanged { .. }))
            .count();
        assert_eq!(changes, 3);
        // неизвестный id
        assert!(matches!(
            l.set_status_at("zzz", Done, 300).unwrap_err(),
            TodoError::UnknownId(_)
        ));
    }

    #[test]
    fn stats_open_and_close_durations() {
        let mut l = list(vec![item("a", Pending), item("b", Pending), item("c", Pending)], 100);
        l.set_status_at("a", Done, 160).unwrap(); // жила 60
        l.set_status_at("b", Cancelled, 220).unwrap(); // жила 120
        let st = l.stats(300);
        assert_eq!(st.total, 3);
        assert_eq!((st.done, st.cancelled, st.pending, st.in_progress), (1, 1, 1, 0));
        assert_eq!(st.avg_close_secs, Some(90), "(60+120)/2");
        assert_eq!(st.max_close_secs, Some(120));
        assert_eq!(st.open_secs, 200, "c открыта 300-100");
        // пустой список — без паник
        let empty = TodoList::new().stats(1000);
        assert_eq!(empty.total, 0);
        assert_eq!(empty.avg_close_secs, None);
    }

    #[test]
    fn drain_events_empties_queue() {
        let mut l = list(vec![item("a", Pending)], 100);
        let evs = l.drain_events();
        assert_eq!(evs.first(), Some(&TodoEvent::ListReplaced { total: 1, done: 0 }));
        assert!(l.drain_events().is_empty(), "второй drain пуст");
        assert!(l.events().is_empty());
    }

    #[test]
    fn status_from_str_parses_variants() {
        assert_eq!("pending".parse::<TodoStatus>().unwrap(), Pending);
        assert_eq!("in-progress".parse::<TodoStatus>().unwrap(), InProgress);
        assert_eq!("IN_PROGRESS".parse::<TodoStatus>().unwrap(), InProgress);
        assert_eq!("canceled".parse::<TodoStatus>().unwrap(), Cancelled);
        let err = "weird".parse::<TodoStatus>().unwrap_err();
        assert!(matches!(err, TodoError::UnknownStatus(ref s) if s == "weird"));
        assert!(err.to_string().contains("weird"));
    }

    #[test]
    fn lifetime_saturates_on_clock_skew() {
        let mut it = item("a", InProgress);
        it.created_at = 100;
        assert_eq!(it.lifetime(50), 0, "часы пошли назад — 0, а не underflow");
        it.closed_at = Some(30);
        assert_eq!(it.close_duration(), Some(0), "closed раньше created — 0");
        assert_eq!(it.lifetime(50), 0);
    }
}
