//! Rollout-трейсинг по образцу `codex-rs/rollout-trace` и модели спанов OpenTelemetry.
//!
//! Один прогон агента (rollout) описывается деревом [`Span`]: у каждого спана
//! есть имя, родитель, монотонные метки старта/конца (мс от создания реестра,
//! часы [`Instant`]), wall-clock метка старта ([`SystemTime`], для корреляции
//! с внешними логами) и атрибуты. [`TraceRegistry`] — владеющий реестр без
//! глобального состояния: создаётся на сессию и передаётся по ссылке в точки
//! инструментации.
//!
//! Экспорт:
//! - потоковый JSONL ([`JsonlTraceWriter`]) — append в файл на каждое открытие
//!   и закрытие спана, каждая строка сбрасывается на диск немедленно, чтобы
//!   трасса пережила падение процесса;
//! - chrome-trace ([`to_chrome_trace`], [`export_chrome_trace`]) — JSON
//!   `{"traceEvents": [...]}` с событиями B/E для просмотра в chrome://tracing
//!   или Perfetto;
//! - ASCII-дерево ([`render_tree`]) для быстрого просмотра в терминале.
//!
//! Утечки спанов (забытый `close_span`) не теряются: [`TraceRegistry::snapshot_auto_close`]
//! дозакрывает их по текущему времени, сохраняя флаг [`Span::open`] как маркер,
//! а [`TraceRegistry::leaked_ids`] возвращает их id.

#![forbid(unsafe_code)]

use serde::Serialize;
use serde_json::{json, Map as JsonMap, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Идентификатор спана внутри одного реестра: порядковый номер открытия, с 1.
pub type SpanId = u64;

/// Один спан трассы: именованный интервал работы с родителем и атрибутами.
///
/// Метки `start_ms`/`end_ms` — миллисекунды от момента создания реестра
/// (монотонные часы, не зависят от перевода системного времени);
/// `wall_start_ms` — UNIX-время старта по системным часам.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Span {
    /// уникальный id (порядковый номер открытия в реестре, с 1)
    pub id: SpanId,
    /// id родителя; `None` — корневой спан. Несуществующий в выборке родитель
    /// делает спан сиротой (помечается при рендере и chrome-экспорте)
    pub parent: Option<SpanId>,
    /// человекочитаемое имя операции (`agent.turn`, `tool.call`, `llm.call`, ...)
    pub name: String,
    /// старт, мс от создания реестра
    pub start_ms: u64,
    /// конец, мс от создания реестра; `None` — спан ещё не закрыт
    pub end_ms: Option<u64>,
    /// старт по wall-clock, UNIX-мс
    pub wall_start_ms: u64,
    /// атрибуты спана (ключи отсортированы)
    pub attrs: BTreeMap<String, String>,
    /// `true`, пока спан не закрыт штатно через `close_span`. После auto-close
    /// при снапшоте остаётся `true` — это маркер утечки
    pub open: bool,
}

impl Span {
    /// Длительность в мс для спана с зафиксированным концом; `None`, если
    /// спан ещё выполняется. Насыщающее вычитание — устойчиво к битым меткам.
    pub fn duration_ms(&self) -> Option<u64> {
        self.end_ms.map(|end| end.saturating_sub(self.start_ms))
    }
}

/// Событие жизненного цикла спана для потокового JSONL-экспорта.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanEvent {
    /// спан открыт
    Open,
    /// спан закрыт штатно (`close_span`)
    Close,
    /// спан дозакрыт принудительно при снапшоте (утечка)
    AutoClose,
}

impl SpanEvent {
    /// Строковый тег события в JSONL-строке.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Close => "close",
            Self::AutoClose => "auto_close",
        }
    }
}

/// JSONL-строка события: тег, wall-clock момент записи и поля спана плашмя.
#[derive(Serialize)]
struct JsonlRecord<'a> {
    event: &'static str,
    wall_ms: u64,
    #[serde(flatten)]
    span: &'a Span,
}

/// Сериализовать событие в JSON-строку (без перевода строки).
fn event_line(event: SpanEvent, span: &Span) -> io::Result<String> {
    let record = JsonlRecord { event: event.as_str(), wall_ms: wall_now_ms(), span };
    serde_json::to_string(&record).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Потоковый JSONL-писатель трассы: одна строка JSON на событие open/close.
///
/// Файл открывается в режиме append; каждая запись сбрасывается на диск
/// немедленно — трасса должна пережить падение процесса.
pub struct JsonlTraceWriter {
    file: BufWriter<File>,
}

impl JsonlTraceWriter {
    /// Открыть файл в режиме append (создаётся при отсутствии).
    ///
    /// # Ошибки
    /// Ошибка открытия файла (нет прав, не существует каталог и т.п.).
    pub fn append(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self { file: BufWriter::new(file) })
    }

    /// Записать событие жизненного цикла спана одной JSON-строкой.
    ///
    /// # Ошибки
    /// Ошибка сериализации (теоретическая) или записи/сброса на диск.
    pub fn record(&mut self, event: SpanEvent, span: &Span) -> io::Result<()> {
        let line = event_line(event, span)?;
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.flush()
    }
}

/// Реестр спанов одного прогона (rollout). Глобального состояния нет:
/// реестр создаётся на сессию и передаётся по ссылке.
///
/// id спанов — порядковые номера открытия (с 1), поэтому поиск по id —
/// прямое индексирование, без хэш-таблиц.
pub struct TraceRegistry {
    /// ноль монотонных часов (момент создания реестра)
    origin: Instant,
    /// все спаны в порядке открытия; спан с id лежит по индексу `id - 1`
    spans: Vec<Span>,
    /// подключённый JSONL-поток; `None` после первой ошибки записи
    jsonl: Option<JsonlTraceWriter>,
    /// первая ошибка записи в JSONL (поток после неё «отравлен» и отключён)
    write_error: Option<io::Error>,
}

impl TraceRegistry {
    /// Пустой реестр; монотонные часы стартуют в этот момент.
    pub fn new() -> Self {
        Self { origin: Instant::now(), spans: Vec::new(), jsonl: None, write_error: None }
    }

    /// Реестр с подключённым JSONL-потоком: open/close спанов сразу
    /// дописываются в `path`.
    ///
    /// # Ошибки
    /// Ошибка открытия файла.
    pub fn with_jsonl(path: impl AsRef<Path>) -> io::Result<Self> {
        let writer = JsonlTraceWriter::append(path)?;
        let mut registry = Self::new();
        registry.attach_jsonl(writer);
        Ok(registry)
    }

    /// Подключить JSONL-писатель к уже созданному реестру.
    pub fn attach_jsonl(&mut self, writer: JsonlTraceWriter) {
        self.jsonl = Some(writer);
    }

    /// Открыть спан и вернуть его id. При подключённом JSONL-потоке событие
    /// `open` пишется в файл сразу.
    ///
    /// Родитель не валидируется: спан с несуществующим родителем остаётся
    /// в трассе и помечается сиротой при рендере и chrome-экспорте.
    pub fn open_span(&mut self, name: &str, parent: Option<SpanId>) -> SpanId {
        let id = self.spans.len() as u64 + 1;
        let span = Span {
            id,
            parent,
            name: name.to_owned(),
            start_ms: self.elapsed_ms(),
            end_ms: None,
            wall_start_ms: wall_now_ms(),
            attrs: BTreeMap::new(),
            open: true,
        };
        self.spans.push(span);
        self.emit(SpanEvent::Open, id);
        id
    }

    /// Закрыть спан штатно, зафиксировав конец по текущему времени.
    /// Возвращает `false`, если спана нет или он уже закрыт (в том числе
    /// auto-close при снапшоте).
    pub fn close_span(&mut self, id: SpanId) -> bool {
        let now = self.elapsed_ms();
        let Some(span) = self.get_mut(id) else { return false };
        if span.end_ms.is_some() {
            return false;
        }
        span.end_ms = Some(now);
        span.open = false;
        self.emit(SpanEvent::Close, id);
        true
    }

    /// Поставить/перезаписать атрибут спана. `false` — спана нет.
    /// Атрибуты можно ставить в любой момент жизни спана (в JSONL уже ушедшие
    /// события при этом, разумеется, не переписываются).
    pub fn attr(&mut self, id: SpanId, key: &str, value: &str) -> bool {
        let Some(span) = self.get_mut(id) else { return false };
        span.attrs.insert(key.to_owned(), value.to_owned());
        true
    }

    /// Спан по id, если существует.
    pub fn get(&self, id: SpanId) -> Option<&Span> {
        self.spans.get(index_of(id)?)
    }

    /// Изменяемый доступ к спану по id.
    fn get_mut(&mut self, id: SpanId) -> Option<&mut Span> {
        self.spans.get_mut(index_of(id)?)
    }

    /// Текущее число спанов в реестре.
    pub fn span_count(&self) -> usize {
        self.spans.len()
    }

    /// Миллисекунды от создания реестра (монотонные часы).
    pub fn elapsed_ms(&self) -> u64 {
        millis_u64(self.origin.elapsed().as_millis())
    }

    /// Полная копия трассы в порядке открытия спанов. Незакрытые спаны —
    /// с `end_ms == None` и `open == true`.
    pub fn snapshot(&self) -> Vec<Span> {
        self.spans.clone()
    }

    /// Снапшот с дозакрытием незакрытых спанов: им проставляется `end_ms`
    /// по текущему времени, но флаг `open` остаётся `true` — маркер утечки
    /// (спан не был закрыт штатно). События уходят в JSONL как `auto_close`;
    /// последующий [`close_span`](Self::close_span) по таким id вернёт `false`.
    pub fn snapshot_auto_close(&mut self) -> Vec<Span> {
        let now = self.elapsed_ms();
        let mut auto_closed = Vec::new();
        for span in &mut self.spans {
            if span.end_ms.is_none() {
                span.end_ms = Some(now);
                auto_closed.push(span.id);
            }
        }
        for id in auto_closed {
            self.emit(SpanEvent::AutoClose, id);
        }
        self.snapshot()
    }

    /// id спанов без `end_ms` — выполняются прямо сейчас.
    pub fn unfinished_ids(&self) -> Vec<SpanId> {
        self.spans.iter().filter(|s| s.end_ms.is_none()).map(|s| s.id).collect()
    }

    /// id спанов, не закрытых штатно (утечки; auto-closed сюда тоже входят).
    pub fn leaked_ids(&self) -> Vec<SpanId> {
        self.spans.iter().filter(|s| s.open).map(|s| s.id).collect()
    }

    /// Первая ошибка записи в JSONL, если была; после неё поток отключён.
    pub fn write_error(&self) -> Option<&io::Error> {
        self.write_error.as_ref()
    }

    /// Отправить событие в JSONL-поток. При первой ошибке поток «отравляется»:
    /// отключается, ошибка фиксируется в `write_error` (трейсинг не должен
    /// ронять основной процесс, но и ошибку мы не прячем).
    fn emit(&mut self, event: SpanEvent, id: SpanId) {
        if self.write_error.is_some() {
            return;
        }
        // Деструктуризация даёт раздельные займы полей: писатель и спан
        // одновременно, без клонирования спана.
        let Self { spans, jsonl, write_error, .. } = self;
        let Some(idx) = index_of(id) else { return };
        if let (Some(writer), Some(span)) = (jsonl.as_mut(), spans.get(idx)) {
            if let Err(err) = writer.record(event, span) {
                *write_error = Some(err);
                *jsonl = None;
            }
        }
    }
}

impl Default for TraceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Индекс спана в векторе (id — порядковый номер с 1); `None` для id = 0.
fn index_of(id: SpanId) -> Option<usize> {
    usize::try_from(id.checked_sub(1)?).ok()
}

/// u128-миллисекунды в u64 с насыщением.
fn millis_u64(ms: u128) -> u64 {
    u64::try_from(ms).unwrap_or(u64::MAX)
}

/// Текущее UNIX-время в мс; 0, если системные часы ушли назад.
fn wall_now_ms() -> u64 {
    let elapsed = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    millis_u64(elapsed.as_millis())
}

/// Виртуальный pid выгрузки: весь прогон — один «процесс» chrome-trace.
const CHROME_PID: u64 = 1;

/// Сериализовать спаны в chrome-trace JSON (`{"traceEvents": [...]}`).
///
/// События B/E в микросекундах (как требует формат); `pid` всегда 1;
/// `tid` — id корневого предка спана в выборке: каждый корень и каждая
/// сирота получают свою «нить» (с метаданными `thread_name`). Незакрытые
/// спаны представлены только B-событием — просмотрщик покажет их как
/// выполняющиеся до конца трассы. Вывод детерминирован: события
/// отсортированы по `(ts, порядок спана в выборке)`.
pub fn to_chrome_trace(spans: &[Span]) -> String {
    let by_id: HashMap<SpanId, &Span> = spans.iter().map(|s| (s.id, s)).collect();

    // tid назначается по корневому предку; номера — по первому появлению корня
    let mut tid_of_root: HashMap<SpanId, u64> = HashMap::new();
    let mut root_order: Vec<SpanId> = Vec::new();
    let mut tid_of_span: Vec<u64> = Vec::with_capacity(spans.len());
    for span in spans {
        let root = root_ancestor(&by_id, span);
        let tid = match tid_of_root.get(&root) {
            Some(&tid) => tid,
            None => {
                let tid = tid_of_root.len() as u64 + 1;
                tid_of_root.insert(root, tid);
                root_order.push(root);
                tid
            }
        };
        tid_of_span.push(tid);
    }

    // метаданные: имя «процесса» и имён «нитей»
    let mut events: Vec<Value> = Vec::with_capacity(root_order.len() + spans.len() + 1);
    events.push(json!({
        "name": "process_name", "ph": "M", "pid": CHROME_PID, "tid": 0,
        "args": { "name": "theseus rollout" },
    }));
    for root in &root_order {
        let tid = tid_of_root[root];
        let root_name = by_id.get(root).map_or("?", |s| s.name.as_str());
        events.push(json!({
            "name": "thread_name", "ph": "M", "pid": CHROME_PID, "tid": tid,
            "args": { "name": format!("{root}: {root_name}") },
        }));
    }

    // события B/E с детерминированным порядком: B раньше E при равных ts
    let mut timed: Vec<(u64, u64, Value)> = Vec::with_capacity(spans.len() * 2);
    for (seq, (span, &tid)) in spans.iter().zip(&tid_of_span).enumerate() {
        let mut args = JsonMap::new();
        args.insert("span_id".to_owned(), json!(span.id));
        if span.open {
            args.insert("open".to_owned(), json!(true));
        }
        for (key, value) in &span.attrs {
            args.insert(key.clone(), json!(value));
        }
        let begin = json!({
            "name": span.name, "ph": "B", "ts": span.start_ms * 1000,
            "pid": CHROME_PID, "tid": tid, "args": Value::Object(args),
        });
        timed.push((span.start_ms * 1000, seq as u64 * 2, begin));
        if let Some(end_ms) = span.end_ms {
            let end = json!({
                "name": span.name, "ph": "E", "ts": end_ms * 1000,
                "pid": CHROME_PID, "tid": tid,
            });
            timed.push((end_ms * 1000, seq as u64 * 2 + 1, end));
        }
    }
    timed.sort_by_key(|(ts, order, _)| (*ts, *order));
    events.extend(timed.into_iter().map(|(_, _, event)| event));

    json!({ "traceEvents": events }).to_string()
}

/// Записать chrome-trace выгрузку в файл (UTF-8 JSON, одна строка).
///
/// # Ошибки
/// Ошибка создания/записи файла.
pub fn export_chrome_trace(spans: &[Span], path: impl AsRef<Path>) -> io::Result<()> {
    let mut out = to_chrome_trace(spans);
    out.push('\n');
    fs::write(path, out)
}

/// Корневой предок спана внутри выборки: идём по `parent`, пока он есть
/// в `by_id`. Циклические parent-ссылки обрываются по счётчику шагов
/// (длиннее честной цепочки они быть не могут) — текущий спан считается
/// корнем, чтобы экспорт не зависал на битых данных.
fn root_ancestor(by_id: &HashMap<SpanId, &Span>, span: &Span) -> SpanId {
    let mut current = span;
    let mut hops = 0usize;
    while let Some(parent) = current.parent {
        match by_id.get(&parent) {
            Some(next) if hops <= by_id.len() => {
                current = next;
                hops += 1;
            }
            _ => break,
        }
    }
    current.id
}

/// ASCII-дерево спанов с отступами и длительностями.
///
/// Корни (без родителя) и сироты (родитель отсутствует в выборке) — на
/// верхнем уровне; сироты помечены `(сирота: родитель #N не найден)`.
/// Незакрытые спаны — `(открыт)`, auto-closed утечки — `(N ms, не закрыт)`.
/// Сиблинги упорядочены по `(start_ms, id)`. Спаны со циклическими
/// parent-ссылками не достижимы из корней и выводятся отдельной секцией,
/// чтобы не пропасть молча. Пустой вход — строка `(пустая трасса)`.
pub fn render_tree(spans: &[Span]) -> String {
    if spans.is_empty() {
        return "(пустая трасса)\n".to_owned();
    }
    let ids: HashSet<SpanId> = spans.iter().map(|s| s.id).collect();
    let mut children: HashMap<SpanId, Vec<&Span>> = HashMap::new();
    let mut roots: Vec<&Span> = Vec::new();
    for span in spans {
        match span.parent {
            Some(parent) if ids.contains(&parent) => children.entry(parent).or_default().push(span),
            _ => roots.push(span),
        }
    }
    // порядок сиблингов: по времени старта, при равенстве — по id
    roots.sort_by_key(|s| (s.start_ms, s.id));
    for kids in children.values_mut() {
        kids.sort_by_key(|s| (s.start_ms, s.id));
    }

    let mut out = String::new();
    // страховка от зацикливания на битых данных (дубли id, циклы через детей)
    let mut emitted: HashSet<SpanId> = HashSet::new();
    for root in &roots {
        if emitted.insert(root.id) {
            out.push_str(&span_line(root, &ids));
            out.push('\n');
            render_children(root, &children, &ids, "", &mut emitted, &mut out);
        }
    }
    // недостижимые из корней спаны (циклические parent-ссылки)
    let cyclic: Vec<&Span> = spans.iter().filter(|s| !emitted.contains(&s.id)).collect();
    if !cyclic.is_empty() {
        out.push_str("!! циклические parent-ссылки (некорректная трасса):\n");
        for span in cyclic {
            emitted.insert(span.id);
            out.push_str("!! ");
            out.push_str(&span_line(span, &ids));
            out.push('\n');
        }
    }
    out
}

/// Рекурсивно вывести детей `parent` с ASCII-отступами (`+-- `, `|   `).
fn render_children(
    parent: &Span,
    children: &HashMap<SpanId, Vec<&Span>>,
    ids: &HashSet<SpanId>,
    prefix: &str,
    emitted: &mut HashSet<SpanId>,
    out: &mut String,
) {
    let Some(kids) = children.get(&parent.id) else { return };
    let last = kids.len() - 1;
    for (i, kid) in kids.iter().enumerate() {
        if !emitted.insert(kid.id) {
            continue;
        }
        out.push_str(prefix);
        out.push_str("+-- ");
        out.push_str(&span_line(kid, ids));
        out.push('\n');
        let next_prefix = format!("{prefix}{}", if i == last { "    " } else { "|   " });
        render_children(kid, children, ids, &next_prefix, emitted, out);
    }
}

/// Одна строка дерева: имя, длительность/статус, атрибуты, маркер сироты.
fn span_line(span: &Span, ids: &HashSet<SpanId>) -> String {
    let mut line = span.name.clone();
    match span.duration_ms() {
        Some(ms) if span.open => line.push_str(&format!(" ({ms} ms, не закрыт)")),
        Some(ms) => line.push_str(&format!(" ({ms} ms)")),
        None => line.push_str(" (открыт)"),
    }
    if !span.attrs.is_empty() {
        let attrs = span
            .attrs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(", ");
        line.push_str(&format!(" [{attrs}]"));
    }
    if let Some(parent) = span.parent {
        if !ids.contains(&parent) {
            line.push_str(&format!(" (сирота: родитель #{parent} не найден)"));
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Синтетический спан с фиксированными метками времени: рендер и экспорт
    /// тестируем детерминированно, без реальных часов.
    fn fixed_span(id: SpanId, parent: Option<SpanId>, name: &str, start_ms: u64, end_ms: Option<u64>) -> Span {
        Span {
            id,
            parent,
            name: name.to_owned(),
            start_ms,
            end_ms,
            wall_start_ms: 1_700_000_000_000,
            attrs: BTreeMap::new(),
            open: end_ms.is_none(),
        }
    }

    /// Уникальный путь во временном каталоге (тесты идут параллельно).
    fn temp_path(tag: &str) -> std::path::PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "theseus_trace_test_{}_{}_{seq}_{tag}.jsonl",
            std::process::id(),
            wall_now_ms()
        ))
    }

    #[test]
    fn open_span_assigns_sequential_ids_and_links_parents() {
        let mut reg = TraceRegistry::new();
        let root = reg.open_span("agent.turn", None);
        let child = reg.open_span("tool.call", Some(root));
        let grand = reg.open_span("fs.read", Some(child));
        assert_eq!((root, child, grand), (1, 2, 3));
        let spans = reg.snapshot();
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].parent, None);
        assert_eq!(spans[1].parent, Some(root));
        assert_eq!(spans[2].parent, Some(child));
        // монотонность: старт потомка не раньше старта родителя
        assert!(spans[0].start_ms <= spans[1].start_ms);
        assert!(spans[1].start_ms <= spans[2].start_ms);
        assert_eq!(reg.span_count(), 3);
    }

    #[test]
    fn close_span_sets_end_and_rejects_repeats() {
        let mut reg = TraceRegistry::new();
        let id = reg.open_span("llm.call", None);
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(reg.close_span(id));
        let span = reg.get(id).unwrap();
        assert!(!span.open);
        let end = span.end_ms.unwrap();
        assert!(end >= span.start_ms);
        assert_eq!(span.duration_ms(), Some(end - span.start_ms));
        assert!(span.duration_ms().unwrap() >= 5);
        // повторное закрытие и закрытие несуществующего/нулевого id — false
        assert!(!reg.close_span(id));
        assert!(!reg.close_span(999));
        assert!(!reg.close_span(0));
    }

    #[test]
    fn attr_sets_overwrites_and_reports_missing() {
        let mut reg = TraceRegistry::new();
        let id = reg.open_span("tool.call", None);
        assert!(reg.attr(id, "tool", "Bash"));
        assert!(reg.attr(id, "cmd", "ls"));
        assert!(reg.attr(id, "tool", "Read")); // перезапись
        assert!(!reg.attr(42, "k", "v"));
        let span = reg.get(id).unwrap();
        assert_eq!(span.attrs.len(), 2);
        assert_eq!(span.attrs.get("tool").unwrap(), "Read");
        // BTreeMap: ключи отсортированы
        let keys: Vec<&String> = span.attrs.keys().collect();
        assert_eq!(keys, ["cmd", "tool"]);
    }

    #[test]
    fn snapshot_copies_spans_and_keeps_open_state() {
        let mut reg = TraceRegistry::new();
        let a = reg.open_span("a", None);
        let b = reg.open_span("b", Some(a));
        reg.close_span(a);
        let spans = reg.snapshot();
        assert_eq!(spans.len(), 2);
        assert!(spans.iter().any(|s| s.id == a && s.end_ms.is_some() && !s.open));
        assert!(spans.iter().any(|s| s.id == b && s.end_ms.is_none() && s.open));
        assert_eq!(reg.unfinished_ids(), vec![b]);
        // снапшот — независимая копия, переживает реестр
        drop(reg);
        assert_eq!(spans.len(), 2);
    }

    #[test]
    fn snapshot_auto_close_marks_leaks_but_fills_end() {
        let mut reg = TraceRegistry::new();
        let closed = reg.open_span("closed", None);
        let leaked = reg.open_span("leaked", Some(closed));
        reg.close_span(closed);
        let spans = reg.snapshot_auto_close();
        let leaked_span = spans.iter().find(|s| s.id == leaked).unwrap();
        // конец проставлен, но флаг open сохранён — маркер утечки
        assert!(leaked_span.end_ms.is_some());
        assert!(leaked_span.open);
        assert!(leaked_span.duration_ms().is_some());
        assert_eq!(reg.unfinished_ids(), Vec::<SpanId>::new());
        assert_eq!(reg.leaked_ids(), vec![leaked]);
        // штатное закрытие после auto-close отвергается
        assert!(!reg.close_span(leaked));
        // корректно закрытый спан утечкой не считается
        assert!(!reg.leaked_ids().contains(&closed));
    }

    #[test]
    fn orphan_parent_is_preserved_and_rendered_as_root_with_marker() {
        let spans = vec![
            fixed_span(1, None, "root", 0, Some(100)),
            fixed_span(2, Some(42), "orphan", 10, Some(20)),
        ];
        let tree = render_tree(&spans);
        assert!(tree.contains("orphan (10 ms) (сирота: родитель #42 не найден)"), "дерево:\n{tree}");
        // сирота — на верхнем уровне дерева (без отступа и коннектора)
        let orphan_line = tree.lines().find(|l| l.contains("orphan")).unwrap();
        assert!(!orphan_line.starts_with(' '));
        assert!(!orphan_line.starts_with('+'));
    }

    #[test]
    fn chrome_trace_is_valid_json_with_b_e_pairs() {
        let mut child = fixed_span(2, Some(1), "child", 5, Some(15));
        child.attrs.insert("tool".to_owned(), "Bash".to_owned());
        let spans = vec![
            fixed_span(1, None, "root", 0, Some(20)),
            child,
            fixed_span(3, Some(1), "running", 10, None),
        ];
        let parsed: Value = serde_json::from_str(&to_chrome_trace(&spans)).unwrap();
        let events = parsed["traceEvents"].as_array().unwrap();
        // метаданные (process + 1 thread) + B/E root + B/E child + B running
        assert_eq!(events.len(), 7);
        let pos = |name: &str, ph: &str| {
            events.iter().position(|e| e["name"] == name && e["ph"] == ph).unwrap()
        };
        assert!(pos("root", "B") < pos("child", "B"));
        assert!(pos("child", "B") < pos("child", "E"));
        assert!(pos("root", "B") < pos("root", "E"));
        // ts в микросекундах
        assert_eq!(events[pos("root", "B")]["ts"], json!(0));
        assert_eq!(events[pos("root", "E")]["ts"], json!(20_000));
        // атрибуты и служебные поля попали в args
        assert_eq!(events[pos("child", "B")]["args"]["tool"], json!("Bash"));
        assert_eq!(events[pos("child", "B")]["args"]["span_id"], json!(2));
        // у незакрытого спана только B и маркер open
        assert!(events.iter().all(|e| !(e["name"] == "running" && e["ph"] == "E")));
        assert_eq!(events[pos("running", "B")]["args"]["open"], json!(true));
    }

    #[test]
    fn chrome_trace_groups_tid_by_root_and_names_threads() {
        let spans = vec![
            fixed_span(1, None, "turn#1", 0, Some(10)),
            fixed_span(2, Some(1), "tool", 2, Some(8)),
            fixed_span(3, None, "turn#2", 20, Some(30)),
            fixed_span(4, Some(99), "orphan", 25, Some(26)),
        ];
        let parsed: Value = serde_json::from_str(&to_chrome_trace(&spans)).unwrap();
        let events = parsed["traceEvents"].as_array().unwrap();
        let tid_of = |name: &str| {
            events.iter().find(|e| e["name"] == name && e["ph"] == "B").unwrap()["tid"].as_u64().unwrap()
        };
        assert_eq!(tid_of("turn#1"), tid_of("tool")); // потомок — в нити корня
        assert_ne!(tid_of("turn#1"), tid_of("turn#2")); // разные корни — разные нити
        assert_ne!(tid_of("orphan"), tid_of("turn#1")); // у сироты своя нить
        // метаданные thread_name для каждой нити (2 корня + 1 сирота)
        let thread_names: Vec<&str> = events
            .iter()
            .filter(|e| e["ph"] == "M" && e["name"] == "thread_name")
            .filter_map(|e| e["args"]["name"].as_str())
            .collect();
        assert_eq!(thread_names.len(), 3);
        assert!(thread_names.iter().any(|n| n.contains("turn#1")));
        assert!(thread_names.iter().any(|n| n.contains("orphan")));
    }

    #[test]
    fn chrome_trace_export_writes_parseable_file() {
        let spans = vec![fixed_span(1, None, "root", 0, Some(7))];
        let path = temp_path("chrome");
        export_chrome_trace(&spans, &path).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: Value = serde_json::from_str(&content).unwrap();
        assert!(parsed["traceEvents"].is_array());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn render_tree_indents_children_and_shows_durations() {
        let mut child = fixed_span(2, Some(1), "tool.call", 2, Some(8));
        child.attrs.insert("exit".to_owned(), "0".to_owned());
        let spans = vec![
            fixed_span(1, None, "agent.turn", 0, Some(20)),
            child,
            fixed_span(3, Some(2), "fs.read", 3, Some(4)),
            fixed_span(4, Some(1), "llm.call", 10, None),
        ];
        let expected = "\
agent.turn (20 ms)
+-- tool.call (6 ms) [exit=0]
|   +-- fs.read (1 ms)
+-- llm.call (открыт)
";
        assert_eq!(render_tree(&spans), expected);
    }

    #[test]
    fn render_tree_handles_empty_and_cyclic_input() {
        assert_eq!(render_tree(&[]), "(пустая трасса)\n");
        // цикл parent-ссылок: оба спана попадают в вывод, без зависания
        let spans = vec![
            fixed_span(1, Some(2), "a", 0, Some(1)),
            fixed_span(2, Some(1), "b", 0, Some(1)),
        ];
        let tree = render_tree(&spans);
        assert!(tree.contains("циклические"), "дерево:\n{tree}");
        assert!(tree.contains("a (1 ms)"));
        assert!(tree.contains("b (1 ms)"));
    }

    #[test]
    fn render_tree_marks_auto_closed_leaks() {
        let mut reg = TraceRegistry::new();
        reg.open_span("forgotten", None);
        let spans = reg.snapshot_auto_close();
        assert!(render_tree(&spans).contains("не закрыт"));
    }

    #[test]
    fn jsonl_writer_appends_parseable_lines() {
        let path = temp_path("writer");
        let span = fixed_span(7, None, "tool.call", 3, Some(9));
        {
            let mut writer = JsonlTraceWriter::append(&path).unwrap();
            writer.record(SpanEvent::Open, &span).unwrap();
            writer.record(SpanEvent::Close, &span).unwrap();
        }
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        let open: Value = serde_json::from_str(lines[0]).unwrap();
        let close: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(open["event"], json!("open"));
        assert_eq!(close["event"], json!("close"));
        assert_eq!(open["id"], json!(7));
        assert_eq!(open["name"], json!("tool.call"));
        assert_eq!(open["start_ms"], json!(3));
        assert!(open["wall_ms"].as_u64().unwrap() > 0);
        // append: повторное открытие файла не затирает старые строки
        {
            let mut writer = JsonlTraceWriter::append(&path).unwrap();
            writer.record(SpanEvent::AutoClose, &span).unwrap();
        }
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().count(), 3);
        let last: Value = serde_json::from_str(content.lines().next_back().unwrap()).unwrap();
        assert_eq!(last["event"], json!("auto_close"));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn registry_streams_open_close_to_jsonl() {
        let path = temp_path("registry");
        {
            let mut reg = TraceRegistry::with_jsonl(&path).unwrap();
            let id = reg.open_span("turn", None);
            assert!(reg.attr(id, "model", "qwen"));
            reg.close_span(id);
            reg.open_span("leaked", Some(id));
            reg.snapshot_auto_close();
            assert!(reg.write_error().is_none());
        }
        let content = std::fs::read_to_string(&path).unwrap();
        let events: Vec<String> = content
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap()["event"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(events, ["open", "close", "open", "auto_close"]);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn wall_clock_start_is_unix_millis() {
        let mut reg = TraceRegistry::new();
        let before = wall_now_ms();
        let id = reg.open_span("x", None);
        let after = wall_now_ms();
        let span = reg.get(id).unwrap();
        assert!(span.wall_start_ms >= before && span.wall_start_ms <= after);
    }

    #[test]
    fn zero_duration_span_exports_b_before_e_at_same_ts() {
        let spans = vec![fixed_span(1, None, "instant", 5, Some(5))];
        let parsed: Value = serde_json::from_str(&to_chrome_trace(&spans)).unwrap();
        let events = parsed["traceEvents"].as_array().unwrap();
        let be: Vec<&Value> = events.iter().filter(|e| e["name"] == "instant").collect();
        assert_eq!(be.len(), 2);
        assert_eq!(be[0]["ph"], json!("B"));
        assert_eq!(be[1]["ph"], json!("E"));
        assert_eq!(be[0]["ts"], be[1]["ts"]);
        // дерево: нулевая длительность — честные «0 ms»
        assert!(render_tree(&spans).contains("instant (0 ms)"));
    }
}
