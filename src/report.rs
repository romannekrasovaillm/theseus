//! Отчёт по сессии агента из транскрипта `events-*.jsonl` (каталог `.theseus/`):
//! одна JSON-строка на событие, `{"ts": <unix>, "event": "<Debug AgentEvent>"}`.
//! Конвейер: [`parse_transcript`] / [`parse_transcript_full`] — толерантный
//! разбор JSONL в [`EventRecord`] (битые строки — в счётчик пропусков);
//! [`compute_stats`] — статистика [`SessionStats`]; [`render_markdown`] —
//! отчёт в Markdown; [`compare`] — дельты двух сессий.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;

/// Одно событие транскрипта: имя варианта (`kind`: `"ToolCall"`, `"Status"`,
/// `"Finished"`, ...) и структурированные поля (`payload`). Метка `ts`
/// (unix-секунды) подмешивается полем `"ts"`; для кортежных событий
/// (`Finished("...")`) значение лежит в поле `"value"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventRecord {
    /// имя варианта события (`"ToolCall"`, `"Accounting"`, ...)
    pub kind: String,
    /// поля события (+ `"ts"`, если метка была в строке-конверте)
    pub payload: Value,
}

/// Результат разбора транскрипта: события и диагностика пропущенных строк.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedTranscript {
    /// успешно разобранные события в порядке строк транскрипта
    pub events: Vec<EventRecord>,
    /// всего непустых строк во входе
    pub total_lines: usize,
    /// строк, которые не удалось разобрать (битый JSON, нет поля `event`/`kind`,
    /// нечитаемое Debug-представление события)
    pub skipped_lines: usize,
}

/// Разобрать текст транскрипта полностью, с диагностикой пропусков.
/// Пустые и чисто пробельные строки игнорируются (не брак и не данные).
pub fn parse_transcript_full(text: &str) -> ParsedTranscript {
    let mut out = ParsedTranscript::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        out.total_lines += 1;
        match parse_line(line) {
            Some(record) => out.events.push(record),
            None => out.skipped_lines += 1,
        }
    }
    out
}

/// Разобрать текст транскрипта, вернув только события.
/// Битые строки молча пропускаются; их счётчик — в [`parse_transcript_full`].
pub fn parse_transcript(text: &str) -> Vec<EventRecord> {
    parse_transcript_full(text).events
}

/// Длительность одного вызова инструмента, восстановленная по паре событий
/// `ToolCall`/`ToolResult` (по меткам `ts`; гранулярность — секунды).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolTiming {
    /// имя инструмента
    pub name: String,
    /// длительность, секунды (результат минус вызов, с насыщением до нуля)
    pub duration_secs: u64,
    /// итог вызова (`ok` из `ToolResult`)
    pub ok: bool,
}

/// Сводная статистика одной сессии, собранная из событий транскрипта.
/// Кумулятивные счётчики `Accounting` берутся по максимуму (нарастающий итог);
/// `duration_secs` — размах меток `ts` (max − min).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionStats {
    /// число ходов агента: максимум `Status.turns` (0, если `Status` не было)
    pub turns: u64,
    /// сколько раз вызывался каждый инструмент: имя → число вызовов
    pub tool_calls: HashMap<String, u64>,
    /// вызовов API (максимум кумулятивного `Accounting.calls`)
    pub api_calls: u64,
    /// prompt-токены (максимум кумулятивного `Accounting.prompt_t`)
    pub prompt_tokens: u64,
    /// completion-токены (максимум кумулятивного `Accounting.completion_t`)
    pub completion_tokens: u64,
    /// сколько раз происходила компактификация контекста
    pub compactions: u64,
    /// длительность сессии по меткам событий, секунды
    pub duration_secs: u64,
    /// итог последнего `Finished(...)`; `None` — сессия не завершилась штатно
    pub finish_summary: Option<String>,
    /// длительности вызовов по убыванию (источник «топ-5 долгих» в отчёте)
    pub slowest: Vec<ToolTiming>,
}

impl SessionStats {
    /// Всего токенов (prompt + completion), с насыщением.
    pub fn total_tokens(&self) -> u64 {
        self.prompt_tokens.saturating_add(self.completion_tokens)
    }

    /// Всего вызовов инструментов (сумма по всем именам).
    pub fn total_tool_calls(&self) -> u64 {
        self.tool_calls.values().sum()
    }
}

/// Собрать статистику сессии из разобранных событий. Пары `ToolCall` →
/// `ToolResult` сопоставляются FIFO по имени инструмента (по меткам `ts`);
/// вызовы без результата и результаты без вызова в длительности не попадают.
pub fn compute_stats(events: &[EventRecord]) -> SessionStats {
    let mut stats = SessionStats::default();
    let mut status_turns: Option<u64> = None;
    let mut min_ts: Option<u64> = None;
    let mut max_ts: Option<u64> = None;
    // незакрытые вызовы по имени инструмента: FIFO-очередь меток ts
    let mut pending: HashMap<String, VecDeque<u64>> = HashMap::new();
    let mut timings: Vec<ToolTiming> = Vec::new();

    for ev in events {
        let ts = ev.payload.get("ts").and_then(Value::as_u64);
        if let Some(ts) = ts {
            min_ts = Some(min_ts.map_or(ts, |m| m.min(ts)));
            max_ts = Some(max_ts.map_or(ts, |m| m.max(ts)));
        }
        match ev.kind.as_str() {
            "Status" => {
                if let Some(t) = ev.payload.get("turns").and_then(Value::as_u64) {
                    status_turns = Some(status_turns.map_or(t, |m| m.max(t)));
                }
            }
            "ToolCall" => {
                if let Some(name) = ev.payload.get("name").and_then(Value::as_str) {
                    *stats.tool_calls.entry(name.to_owned()).or_insert(0) += 1;
                    if let Some(ts) = ts {
                        pending.entry(name.to_owned()).or_default().push_back(ts);
                    }
                }
            }
            "ToolResult" => {
                if let Some(name) = ev.payload.get("name").and_then(Value::as_str) {
                    let ok = ev.payload.get("ok").and_then(Value::as_bool).unwrap_or(true);
                    let pair = ts.zip(pending.get_mut(name).and_then(VecDeque::pop_front));
                    if let Some((end, start)) = pair {
                        timings.push(ToolTiming {
                            name: name.to_owned(),
                            duration_secs: end.saturating_sub(start),
                            ok,
                        });
                    }
                }
            }
            "Accounting" => {
                stats.api_calls = stats.api_calls.max(get_u64(&ev.payload, "calls"));
                stats.prompt_tokens = stats.prompt_tokens.max(get_u64(&ev.payload, "prompt_t"));
                stats.completion_tokens = stats.completion_tokens.max(get_u64(&ev.payload, "completion_t"));
            }
            "Compact" => stats.compactions += 1,
            "Finished" => {
                // theseus-формат: текст в поле "value"; структурный — сам payload
                stats.finish_summary = ev
                    .payload
                    .get("value")
                    .and_then(Value::as_str)
                    .or_else(|| ev.payload.as_str())
                    .map(str::to_owned);
            }
            _ => {}
        }
    }

    stats.turns = status_turns.unwrap_or(0);
    if let (Some(lo), Some(hi)) = (min_ts, max_ts) {
        stats.duration_secs = hi.saturating_sub(lo);
    }
    timings.sort_by(|x, y| y.duration_secs.cmp(&x.duration_secs).then_with(|| x.name.cmp(&y.name)));
    stats.slowest = timings;
    stats
}

/// Метаданные для шапки отчёта [`render_markdown`]. Всё опционально:
/// пустые поля просто не выводятся.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportMeta {
    /// заголовок отчёта; по умолчанию «Отчёт по сессии theseus»
    pub title: Option<String>,
    /// рабочий каталог сессии
    pub workspace: Option<String>,
    /// модель, на которой шла сессия
    pub model: Option<String>,
    /// источник данных (например, имя файла транскрипта)
    pub source: Option<String>,
}

impl ReportMeta {
    /// Метаданные с заданным заголовком; остальные поля пусты.
    pub fn new(title: impl Into<String>) -> Self {
        Self { title: Some(title.into()), ..Self::default() }
    }
}

/// Отрендерить статистику сессии в Markdown: заголовок (из `meta`), сводка,
/// токены, таблица инструментов (по убыванию вызовов), топ-5 долгих вызовов,
/// завершение. Пустые данные не роняют рендер — выводятся текстовые пометки.
pub fn render_markdown(stats: &SessionStats, meta: &ReportMeta) -> String {
    let mut out = String::new();
    let title = meta.title.as_deref().unwrap_or("Отчёт по сессии theseus");
    let _ = writeln!(out, "# {title}\n");
    for (label, value) in [("Источник", &meta.source), ("Каталог", &meta.workspace), ("Модель", &meta.model)] {
        if let Some(value) = value {
            let _ = writeln!(out, "- {label}: `{value}`");
        }
    }
    out.push('\n');

    let _ = writeln!(out, "## Сводка\n");
    let _ = writeln!(out, "| Метрика | Значение |");
    let _ = writeln!(out, "|---|---:|");
    let _ = writeln!(out, "| Ходы | {} |", stats.turns);
    let _ = writeln!(out, "| Вызовы API | {} |", stats.api_calls);
    let _ = writeln!(out, "| Вызовы инструментов | {} |", stats.total_tool_calls());
    let _ = writeln!(out, "| Компактификации | {} |", stats.compactions);
    let duration = fmt_duration(stats.duration_secs);
    let _ = writeln!(out, "| Длительность | {duration} |\n");

    let _ = writeln!(out, "## Токены\n");
    let _ = writeln!(out, "| Метрика | Значение |");
    let _ = writeln!(out, "|---|---:|");
    let _ = writeln!(out, "| Prompt | {} |", fmt_num(stats.prompt_tokens));
    let _ = writeln!(out, "| Completion | {} |", fmt_num(stats.completion_tokens));
    let _ = writeln!(out, "| Всего | {} |\n", fmt_num(stats.total_tokens()));

    let _ = writeln!(out, "## Инструменты\n");
    if stats.tool_calls.is_empty() {
        let _ = writeln!(out, "Инструменты не вызывались.\n");
    } else {
        let _ = writeln!(out, "| Инструмент | Вызовов |");
        let _ = writeln!(out, "|---|---:|");
        let mut rows: Vec<(&String, &u64)> = stats.tool_calls.iter().collect();
        rows.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        for (name, count) in rows {
            let _ = writeln!(out, "| {} | {count} |", md_cell(name));
        }
        out.push('\n');
    }

    let _ = writeln!(out, "## Топ-5 долгих вызовов\n");
    if stats.slowest.is_empty() {
        let _ = writeln!(out, "Нет данных о длительностях вызовов.\n");
    } else {
        let _ = writeln!(out, "| # | Инструмент | Длительность | Итог |");
        let _ = writeln!(out, "|--:|---|---:|---|");
        for (i, timing) in stats.slowest.iter().take(5).enumerate() {
            let n = i + 1;
            let duration = fmt_duration(timing.duration_secs);
            let status = if timing.ok { "ok" } else { "ошибка" };
            let _ = writeln!(out, "| {n} | {} | {duration} | {status} |", md_cell(&timing.name));
        }
        out.push('\n');
    }

    let _ = writeln!(out, "## Завершение\n");
    match &stats.finish_summary {
        Some(summary) => {
            let _ = writeln!(out, "> {summary}");
        }
        None => {
            let _ = writeln!(out, "Событие `Finished` отсутствует — сессия, похоже, оборвалась.");
        }
    }
    out
}

/// Сравнить две сессии: дельты (`B − A`) по скалярным метрикам и по составу
/// вызовов инструментов (объединение имён, сортировка по модулю дельты),
/// плюс итоги завершения обеих. Числа — с разрядками, дельты — со знаком.
pub fn compare(a: &SessionStats, b: &SessionStats) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Сравнение сессий\n");
    let _ = writeln!(out, "| Метрика | A | B | Δ (B − A) |");
    let _ = writeln!(out, "|---|---:|---:|---:|");
    compare_row(&mut out, "Ходы", a.turns, b.turns);
    compare_row(&mut out, "Вызовы API", a.api_calls, b.api_calls);
    compare_row(&mut out, "Вызовы инструментов", a.total_tool_calls(), b.total_tool_calls());
    compare_row(&mut out, "Токены prompt", a.prompt_tokens, b.prompt_tokens);
    compare_row(&mut out, "Токены completion", a.completion_tokens, b.completion_tokens);
    compare_row(&mut out, "Токены всего", a.total_tokens(), b.total_tokens());
    compare_row(&mut out, "Компактификации", a.compactions, b.compactions);
    compare_row(&mut out, "Длительность, с", a.duration_secs, b.duration_secs);
    out.push('\n');

    let _ = writeln!(out, "## Инструменты: дельты\n");
    let names: HashSet<&str> = a
        .tool_calls
        .keys()
        .map(String::as_str)
        .chain(b.tool_calls.keys().map(String::as_str))
        .collect();
    if names.is_empty() {
        let _ = writeln!(out, "В обеих сессиях инструменты не вызывались.\n");
    } else {
        let _ = writeln!(out, "| Инструмент | A | B | Δ |");
        let _ = writeln!(out, "|---|---:|---:|---:|");
        let mut rows: Vec<(&str, u64, u64)> = names
            .into_iter()
            .map(|name| {
                let av = a.tool_calls.get(name).copied().unwrap_or(0);
                let bv = b.tool_calls.get(name).copied().unwrap_or(0);
                (name, av, bv)
            })
            .collect();
        rows.sort_by(|x, y| {
            let dx = delta_i128(x.1, x.2).abs();
            let dy = delta_i128(y.1, y.2).abs();
            dy.cmp(&dx).then_with(|| x.0.cmp(y.0))
        });
        for (name, av, bv) in rows {
            let delta = fmt_signed(delta_i128(av, bv));
            let _ = writeln!(out, "| {} | {} | {} | {delta} |", md_cell(name), fmt_num(av), fmt_num(bv));
        }
        out.push('\n');
    }

    let _ = writeln!(out, "## Завершение\n");
    let _ = writeln!(out, "- A: {}", finish_line(a));
    let _ = writeln!(out, "- B: {}", finish_line(b));
    out
}

/// Разбор одной строки транскрипта; `None` — строка не похожа на событие.
/// Приоритет у «живого» формата theseus (`event` + `ts`); затем — структурный.
fn parse_line(line: &str) -> Option<EventRecord> {
    let value: Value = serde_json::from_str(line).ok()?;
    let obj = value.as_object()?;
    let ts = obj.get("ts").and_then(Value::as_u64);
    if let Some(event) = obj.get("event").and_then(Value::as_str) {
        let (kind, payload) = parse_debug_event(event)?;
        return Some(EventRecord { kind, payload: with_ts(payload, ts) });
    }
    if let Some(kind) = obj.get("kind").and_then(Value::as_str) {
        let payload = obj.get("payload").cloned().unwrap_or(Value::Null);
        return Some(EventRecord { kind: kind.to_owned(), payload: with_ts(payload, ts) });
    }
    None
}

/// Подмешать метку `ts` в payload: объектам — полем `"ts"`, скалярам —
/// через обёртку `{"value": ..., "ts": ...}`. Без `ts` payload не трогаем.
fn with_ts(payload: Value, ts: Option<u64>) -> Value {
    match (payload, ts) {
        (Value::Object(mut map), Some(ts)) => {
            map.insert("ts".to_owned(), Value::from(ts));
            Value::Object(map)
        }
        (other, Some(ts)) => {
            let mut map = JsonMap::new();
            map.insert("value".to_owned(), other);
            map.insert("ts".to_owned(), Value::from(ts));
            Value::Object(map)
        }
        (payload, None) => payload,
    }
}

/// Разбор Debug-представления события: `Name { field: value, ... }`,
/// `Name("...")` / `Name(123)` или «пустой» вариант `Name`.
/// `None` — представление нечитаемо (оборванная строка, мусор после имени).
fn parse_debug_event(text: &str) -> Option<(String, Value)> {
    let text = text.trim();
    let head_end = text.find([' ', '(', '{']).unwrap_or(text.len());
    let kind = text[..head_end].trim();
    if kind.is_empty() {
        return None;
    }
    let rest = text[head_end..].trim();
    let payload = if rest.is_empty() {
        Value::Null
    } else if rest.starts_with('{') && rest.ends_with('}') {
        parse_struct_fields(&rest[1..rest.len() - 1])?
    } else if rest.starts_with('(') && rest.ends_with(')') {
        parse_tuple_value(&rest[1..rest.len() - 1])?
    } else {
        return None;
    };
    Some((kind.to_owned(), payload))
}

/// Разбор тела struct-варианта `field: value, ...` в JSON-объект. Запятые
/// и скобки внутри кавычек разделителями не считаются.
fn parse_struct_fields(body: &str) -> Option<Value> {
    let mut map = JsonMap::new();
    let mut rest = body.trim();
    while !rest.is_empty() {
        let colon = rest.find(':')?;
        let key = rest[..colon].trim();
        if key.is_empty() || !key.chars().all(|c: char| c.is_alphanumeric() || c == '_') {
            return None;
        }
        rest = rest[colon + 1..].trim_start();
        let (value, tail) = if rest.starts_with('"') {
            let (text, tail) = take_debug_string(rest)?;
            (Value::String(text), tail)
        } else {
            let (raw, tail) = take_bare_value(rest);
            (parse_bare_token(raw)?, tail)
        };
        map.insert(key.to_owned(), value);
        rest = tail.trim_start();
        if let Some(stripped) = rest.strip_prefix(',') {
            rest = stripped.trim_start();
        } else if !rest.is_empty() {
            // мусор после значения без запятой — строка битая
            return None;
        }
    }
    Some(Value::Object(map))
}

/// Значение кортежного варианта: строка в кавычках (целиком, иначе брак)
/// или «голый» токен; пустые скобки — `null`.
fn parse_tuple_value(inner: &str) -> Option<Value> {
    let inner = inner.trim();
    if inner.starts_with('"') {
        let (text, tail) = take_debug_string(inner)?;
        if tail.trim().is_empty() {
            return Some(Value::String(text));
        }
        return None;
    }
    if inner.is_empty() {
        return Some(Value::Null);
    }
    parse_bare_token(inner)
}

/// Снять Debug-строку в кавычках с начала `input`: расэкранировать текст,
/// вернуть его и хвост после закрывающей кавычки. `None` — строка оборвана.
fn take_debug_string(input: &str) -> Option<(String, &str)> {
    debug_assert!(input.starts_with('"'));
    let mut out = String::new();
    let mut it = input.char_indices().peekable();
    it.next(); // открывающая кавычка гарантирована вызывающей стороной
    while let Some((i, c)) = it.next() {
        match c {
            '"' => return Some((out, &input[i + 1..])),
            '\\' => {
                let (_, esc) = it.next()?;
                match esc {
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    '0' => out.push('\0'),
                    'u' if matches!(it.peek(), Some(&(_, '{'))) => {
                        it.next(); // '{'
                        let mut hex = String::new();
                        let mut closed = false;
                        for (_, h) in it.by_ref() {
                            if h == '}' {
                                closed = true;
                                break;
                            }
                            hex.push(h);
                        }
                        if !closed {
                            return None;
                        }
                        let ch = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)?;
                        out.push(ch);
                    }
                    other => {
                        out.push('\\');
                        out.push(other);
                    }
                }
            }
            _ => out.push(c),
        }
    }
    None
}

/// Снять «голое» значение до ближайшей запятой (или до конца строки).
fn take_bare_value(input: &str) -> (&str, &str) {
    match input.find(',') {
        Some(i) => (input[..i].trim(), &input[i..]),
        None => (input.trim(), ""),
    }
}

/// «Голое» значение — ровно один токен без пробелов: число или bool.
fn parse_bare_token(raw: &str) -> Option<Value> {
    if raw.is_empty() || raw.chars().any(char::is_whitespace) {
        return None;
    }
    Some(parse_bare_value(raw))
}

/// Типизация «голого» токена: bool или целое (u64/i64); иначе — строка.
fn parse_bare_value(raw: &str) -> Value {
    if raw == "true" {
        return Value::Bool(true);
    }
    if raw == "false" {
        return Value::Bool(false);
    }
    if let Ok(n) = raw.parse::<u64>() {
        return Value::from(n);
    }
    if let Ok(n) = raw.parse::<i64>() {
        return Value::from(n);
    }
    Value::String(raw.to_owned())
}

/// u64-поле из payload; отсутствие или другой тип — 0.
fn get_u64(payload: &Value, key: &str) -> u64 {
    payload.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// Строка сводной таблицы сравнения: `| name | A | B | Δ |` со знаковой дельтой.
fn compare_row(out: &mut String, name: &str, a: u64, b: u64) {
    let delta = fmt_signed(delta_i128(a, b));
    let _ = writeln!(out, "| {name} | {} | {} | {delta} |", fmt_num(a), fmt_num(b));
}

/// Разность счётчиков как i128: значения u64 гарантированно помещаются.
fn delta_i128(a: u64, b: u64) -> i128 {
    i128::from(b) - i128::from(a)
}

/// Строка завершения для сравнения: цитата итога или пометка об обрыве.
fn finish_line(stats: &SessionStats) -> String {
    stats.finish_summary.as_ref().map_or_else(
        || "событие `Finished` отсутствует".to_owned(),
        |summary| format!("«{summary}»"),
    )
}

/// Знаковое число с разрядками: `+20 000`, `-1 234`, `0`.
fn fmt_signed(n: i128) -> String {
    let abs = u64::try_from(n.unsigned_abs()).unwrap_or(u64::MAX);
    if n > 0 {
        format!("+{}", fmt_num(abs))
    } else if n < 0 {
        format!("-{}", fmt_num(abs))
    } else {
        "0".to_owned()
    }
}

/// Число с разрядками по-русски: `120 000`.
fn fmt_num(n: u64) -> String {
    let digits = n.to_string();
    // группы по 3 цифры справа; цифры ASCII — from_utf8 не упадёт
    digits
        .as_bytes()
        .rchunks(3)
        .rev()
        .map(|chunk| std::str::from_utf8(chunk).unwrap_or(""))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Длительность человечески: `42 с`, `3 мин 05 с`, `1 ч 02 мин 03 с`.
fn fmt_duration(secs: u64) -> String {
    if secs >= 3600 {
        let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
        format!("{h} ч {m:02} мин {s:02} с")
    } else if secs >= 60 {
        let (m, s) = (secs / 60, secs % 60);
        format!("{m} мин {s:02} с")
    } else {
        format!("{secs} с")
    }
}

/// Экранирование ячейки Markdown-таблицы: вертикальная черта рвёт разметку.
fn md_cell(text: &str) -> String {
    text.replace('|', "\\|")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn theseus_line(ts: u64, event: &str) -> String {
        json!({"ts": ts, "event": event}).to_string()
    }

    fn event(kind: &str, payload: Value) -> EventRecord {
        EventRecord { kind: kind.to_owned(), payload }
    }

    fn sample_stats() -> SessionStats {
        SessionStats {
            turns: 7,
            tool_calls: HashMap::from([("Bash".to_owned(), 5), ("Read".to_owned(), 3), ("Edit".to_owned(), 1)]),
            api_calls: 9,
            prompt_tokens: 123_456,
            completion_tokens: 7_890,
            compactions: 1,
            duration_secs: 3_725,
            finish_summary: Some("все тесты зелёные".to_owned()),
            slowest: vec![
                ToolTiming { name: "Bash".to_owned(), duration_secs: 95, ok: true },
                ToolTiming { name: "Read".to_owned(), duration_secs: 12, ok: false },
            ],
        }
    }

    #[test]
    fn parse_transcript_reads_theseus_format() {
        let text = [
            theseus_line(100, "UserMsg(\"почини тесты\")"),
            theseus_line(101, "ToolCall { name: \"Bash\", args: \"cargo test\", decision: \"allow\" }"),
            theseus_line(103, "ToolResult { name: \"Bash\", preview: \"ok\", ok: true }"),
        ]
        .join("\n");
        let parsed = parse_transcript_full(&text);
        assert_eq!((parsed.total_lines, parsed.skipped_lines, parsed.events.len()), (3, 0, 3));
        let call = &parsed.events[1];
        assert_eq!(call.kind, "ToolCall");
        assert_eq!(call.payload["name"], json!("Bash"));
        assert_eq!(call.payload["ts"], json!(101));
        // числа и bool — типизированы, а не строки
        assert!(call.payload["ts"].is_u64());
        assert_eq!(parsed.events[2].payload["ok"], json!(true));
        // кортежное событие — значение в поле "value"
        assert_eq!(parsed.events[0].payload["value"], json!("почини тесты"));
    }

    #[test]
    fn parse_transcript_skips_broken_lines_with_counter() {
        let text = [
            theseus_line(1, "Compact { from_msgs: 10, to_msgs: 4 }"),
            "{not json at all".to_owned(),
            json!({"ts": 2, "note": "нет ни event, ни kind"}).to_string(),
            theseus_line(3, "Status { turns: 2, est_tokens: 500, mode: \"normal\" }"),
            theseus_line(4, "ToolCall { name: \"Bash\", args: \"оборванная строка"),
        ]
        .join("\n");
        let parsed = parse_transcript_full(&text);
        assert_eq!((parsed.total_lines, parsed.skipped_lines, parsed.events.len()), (5, 3, 2));
        assert_eq!(parsed.events[0].kind, "Compact");
        assert_eq!(parsed.events[0].payload["from_msgs"], json!(10));
        assert_eq!(parsed.events[1].kind, "Status");
        // parse_transcript возвращает тот же набор событий, но без диагностики
        assert_eq!(parse_transcript(&text), parsed.events);
    }

    #[test]
    fn parse_transcript_reads_generic_kind_payload_format() {
        // структурный формат {"kind","payload"} — в т.ч. сериализация EventRecord
        let record = event("ToolCall", json!({"name": "Read", "args": "f.rs", "ts": 7}));
        let parsed = parse_transcript(&serde_json::to_string(&record).unwrap());
        assert_eq!(parsed, vec![record]);
        // скалярный payload без ts не заворачивается в объект
        let events = parse_transcript(&json!({"kind": "Finished", "payload": "готово"}).to_string());
        assert_eq!(events[0].payload, json!("готово"));
    }

    #[test]
    fn parse_debug_string_with_nested_quotes_and_braces() {
        // args — JSON-строка внутри Debug-строки: escaped-кавычки и фигурные скобки
        let event = r#"ToolCall { name: "Bash", args: "{\"cmd\": \"echo }\", \"workdir\": \"/tmp\"}", decision: "allow" }"#;
        let events = parse_transcript(&theseus_line(42, event));
        assert_eq!(events.len(), 1);
        let payload = &events[0].payload;
        assert_eq!(payload["name"], json!("Bash"));
        assert_eq!(payload["args"], json!(r#"{"cmd": "echo }", "workdir": "/tmp"}"#));
        assert_eq!(payload["decision"], json!("allow"));
    }

    #[test]
    fn parse_debug_tuple_variant_with_escapes() {
        let line = theseus_line(9, r#"Finished("сделано: \"пункт а\" и \\ и перевод\nстроки")"#);
        let events = parse_transcript(&line);
        assert_eq!(events[0].kind, "Finished");
        assert_eq!(events[0].payload["value"], json!("сделано: \"пункт а\" и \\ и перевод\nстроки"));
        // юникодный escape \u{XXXX} разворачивается в символ
        let line = theseus_line(9, r#"Finished("check \u{2713}")"#);
        let events = parse_transcript(&line);
        assert_eq!(events[0].payload["value"], json!("check \u{2713}"));
    }

    #[test]
    fn parse_debug_event_rejects_malformed() {
        // нет скобок после имени; незакрытая кавычка; «голое» значение с пробелами
        assert!(parse_transcript(&theseus_line(1, "garbage without parens")).is_empty());
        assert!(parse_transcript(&theseus_line(1, "ToolCall { name: \"Bash")).is_empty());
        assert!(parse_transcript(&theseus_line(1, "Compact { from_msgs: 3 to_msgs: 1 }")).is_empty());
        // а валидный кортежный вариант с bool читается
        let events = parse_transcript(&theseus_line(1, "PlanChanged(true)"));
        assert_eq!(events[0].payload["value"], json!(true));
    }

    #[test]
    fn compute_stats_counts_counters_and_maxima() {
        let events = vec![
            event("Status", json!({"turns": 3, "est_tokens": 900, "mode": "normal", "ts": 100})),
            event("Status", json!({"turns": 7, "est_tokens": 1800, "mode": "normal", "ts": 200})),
            event("Accounting", json!({"calls": 4, "prompt_t": 1000, "completion_t": 200, "ts": 150})),
            event("Accounting", json!({"calls": 9, "prompt_t": 2500, "completion_t": 700, "ts": 250})),
            event("Compact", json!({"from_msgs": 40, "to_msgs": 12, "ts": 260})),
            event("Compact", json!({"from_msgs": 12, "to_msgs": 5, "ts": 270})),
        ];
        let stats = compute_stats(&events);
        assert_eq!(stats.turns, 7); // максимум Status.turns
        assert_eq!(stats.api_calls, 9); // кумулятивный счётчик — максимум
        assert_eq!(stats.prompt_tokens, 2500);
        assert_eq!(stats.completion_tokens, 700);
        assert_eq!(stats.compactions, 2);
        assert_eq!(stats.duration_secs, 170); // 270 - 100
    }

    #[test]
    fn compute_stats_finish_summary_and_duration_edges() {
        // последнее Finished побеждает
        let events = vec![
            event("Finished", json!({"value": "черновой итог", "ts": 10})),
            event("Finished", json!({"value": "финальный итог", "ts": 20})),
        ];
        let stats = compute_stats(&events);
        assert_eq!(stats.finish_summary.as_deref(), Some("финальный итог"));
        assert_eq!(stats.duration_secs, 10);
        // без Finished — None; без ts — длительность 0
        let stats = compute_stats(&[event("AgentText", json!({"value": "текст"}))]);
        assert_eq!((stats.finish_summary.as_deref(), stats.duration_secs), (None, 0));
    }

    #[test]
    fn compute_stats_pairs_calls_with_results_fifo() {
        let events = vec![
            event("ToolCall", json!({"name": "Bash", "args": "a", "decision": "allow", "ts": 100})),
            event("ToolCall", json!({"name": "Read", "args": "f", "decision": "allow", "ts": 101})),
            event("ToolCall", json!({"name": "Bash", "args": "b", "decision": "allow", "ts": 102})),
            // результаты: первый Bash — 30 с, второй — 38 с (FIFO по имени)
            event("ToolResult", json!({"name": "Bash", "preview": "ok", "ok": true, "ts": 130})),
            event("ToolResult", json!({"name": "Bash", "preview": "fail", "ok": false, "ts": 140})),
            event("ToolResult", json!({"name": "Read", "preview": "ok", "ok": true, "ts": 105})),
            // результат без вызова — сирота, в длительности не идёт
            event("ToolResult", json!({"name": "Grep", "preview": "сирота", "ok": true, "ts": 150})),
        ];
        let stats = compute_stats(&events);
        assert_eq!(stats.tool_calls.len(), 2);
        assert_eq!(stats.tool_calls["Bash"], 2);
        assert_eq!(stats.total_tool_calls(), 3);
        // slowest отсортирован по убыванию длительности
        let durations: Vec<(&str, u64, bool)> =
            stats.slowest.iter().map(|t| (t.name.as_str(), t.duration_secs, t.ok)).collect();
        assert_eq!(durations, [("Bash", 38, false), ("Bash", 30, true), ("Read", 4, true)]);
        // метка результата раньше метки вызова — насыщение до нуля, не паника
        let events = vec![
            event("ToolCall", json!({"name": "Bash", "args": "a", "decision": "allow", "ts": 200})),
            event("ToolResult", json!({"name": "Bash", "preview": "ok", "ok": true, "ts": 150})),
        ];
        assert_eq!(compute_stats(&events).slowest[0].duration_secs, 0);
    }

    #[test]
    fn empty_inputs_stay_clean() {
        // пустой транскрипт: ни событий, ни брака; пробелы — не данные
        let parsed = parse_transcript_full("\n   \n\t\n");
        assert_eq!((parsed.total_lines, parsed.skipped_lines), (0, 0));
        assert!(parsed.events.is_empty());
        // пустой набор событий — нулевая статистика
        let stats = compute_stats(&[]);
        assert_eq!(stats, SessionStats::default());
        assert_eq!((stats.total_tokens(), stats.total_tool_calls()), (0, 0));
    }

    #[test]
    fn render_markdown_contains_all_sections() {
        let meta = ReportMeta {
            title: Some("Сессия #42".to_owned()),
            workspace: Some("/home/roman/project".to_owned()),
            model: Some("qwen3.5-4b".to_owned()),
            source: Some("events-1784387374.jsonl".to_owned()),
        };
        let md = render_markdown(&sample_stats(), &meta);
        // заголовок, мета-строки, секции
        assert!(md.contains("# Сессия #42"), "отчёт:\n{md}");
        assert!(md.contains("- Источник: `events-1784387374.jsonl`"));
        for section in ["## Сводка", "## Токены", "## Инструменты", "## Топ-5 долгих вызовов", "## Завершение"] {
            assert!(md.contains(section), "нет секции {section}:\n{md}");
        }
        // таблица инструментов: сортировка по убыванию числа вызовов
        assert!(md.find("| Bash | 5 |").unwrap() < md.find("| Read | 3 |").unwrap());
        // токены с разрядками, длительность человечески
        assert!(md.contains("| Prompt | 123 456 |"));
        assert!(md.contains("1 ч 02 мин 05 с"));
        // топ-5: имена, длительности, статусы; завершение цитатой
        assert!(md.contains("| 1 | Bash | 1 мин 35 с | ok |"));
        assert!(md.contains("| 2 | Read | 12 с | ошибка |"));
        assert!(md.contains("> все тесты зелёные"));
    }

    #[test]
    fn render_markdown_limits_slowest_to_five() {
        let mut stats = sample_stats();
        stats.slowest = (1..=7)
            .map(|i| ToolTiming { name: format!("tool{i}"), duration_secs: 100 - i * 10, ok: true })
            .collect();
        let md = render_markdown(&stats, &ReportMeta::default());
        let section = md.split("## Топ-5 долгих вызовов").nth(1).unwrap();
        let section = section.split("##").next().unwrap();
        assert!(section.contains("| 5 | tool5 | 50 с | ok |"));
        assert!(!section.contains("tool6"), "лишняя строка в топ-5:\n{section}");
    }

    #[test]
    fn compare_shows_signed_deltas_and_tool_union() {
        let a = sample_stats();
        let mut b = sample_stats();
        b.turns = 10;
        b.prompt_tokens = 200_000;
        b.compactions = 0;
        b.duration_secs = 3_000;
        b.tool_calls = HashMap::from([("Bash".to_owned(), 8), ("Grep".to_owned(), 2)]);
        let md = compare(&a, &b);
        assert!(md.contains("# Сравнение сессий"), "сравнение:\n{md}");
        assert!(md.contains("| Ходы | 7 | 10 | +3 |"));
        assert!(md.contains("| Токены prompt | 123 456 | 200 000 | +76 544 |"));
        assert!(md.contains("| Компактификации | 1 | 0 | -1 |"));
        assert!(md.contains("| Длительность, с | 3 725 | 3 000 | -725 |"));
        // объединение инструментов: Bash (5→8), Grep (0→2), Read (3→0), Edit (1→0)
        assert!(md.contains("| Bash | 5 | 8 | +3 |"));
        assert!(md.contains("| Grep | 0 | 2 | +2 |"));
        assert!(md.contains("| Read | 3 | 0 | -3 |"));
        assert!(md.contains("| Edit | 1 | 0 | -1 |"));
        // сортировка по модулю дельты: Bash/Read (3) раньше Grep (2)
        assert!(md.find("| Bash |").unwrap() < md.find("| Grep |").unwrap());
        // завершение обеих сессий
        assert!(md.contains("- A: «все тесты зелёные»"));
    }

    #[test]
    fn compare_equal_and_empty_sessions() {
        let a = sample_stats();
        let md = compare(&a, &a);
        assert!(md.contains("| Ходы | 7 | 7 | 0 |"));
        assert!(md.contains("| Токены всего | 131 346 | 131 346 | 0 |"));
        // обе сессии пустые: без таблицы инструментов, с пометкой об обрыве
        let md = compare(&SessionStats::default(), &SessionStats::default());
        assert!(md.contains("В обеих сессиях инструменты не вызывались."));
        assert!(md.contains("- A: событие `Finished` отсутствует"));
    }

    #[test]
    fn end_to_end_realistic_transcript() {
        let text = [
            theseus_line(1_784_387_374, "UserMsg(\"почини падающий тест\")"),
            theseus_line(1_784_387_375, "Status { turns: 1, est_tokens: 1200, mode: \"normal\" }"),
            theseus_line(1_784_387_380, "ToolCall { name: \"Read\", args: \"src/lib.rs\", decision: \"allow\" }"),
            theseus_line(1_784_387_382, "ToolResult { name: \"Read\", preview: \"pub fn ...\", ok: true }"),
            theseus_line(1_784_387_390, "ToolCall { name: \"Bash\", args: \"cargo test\", decision: \"allow\" }"),
            theseus_line(1_784_387_430, "ToolResult { name: \"Bash\", preview: \"test result: ok\", ok: true }"),
            theseus_line(1_784_387_431, "Accounting { calls: 2, prompt_t: 5200, completion_t: 900 }"),
            theseus_line(1_784_387_432, "Status { turns: 2, est_tokens: 6100, mode: \"normal\" }"),
            theseus_line(1_784_387_433, "Compact { from_msgs: 18, to_msgs: 7 }"),
            theseus_line(1_784_387_435, "Finished(\"тест починен: поправлена граница диапазона\")"),
        ]
        .join("\n");
        let parsed = parse_transcript_full(&text);
        assert_eq!((parsed.total_lines, parsed.skipped_lines), (10, 0));
        let stats = compute_stats(&parsed.events);
        assert_eq!(stats.turns, 2);
        assert_eq!(stats.duration_secs, 61);
        assert_eq!(stats.slowest[0].name, "Bash");
        assert_eq!(stats.slowest[0].duration_secs, 40);
        assert_eq!(stats.finish_summary.as_deref(), Some("тест починен: поправлена граница диапазона"));
        let md = render_markdown(&stats, &ReportMeta::new("Прогон #1"));
        assert!(md.contains("# Прогон #1"));
        assert!(md.contains("| Bash | 40 с | ok |"));
        assert!(md.contains("> тест починен: поправлена граница диапазона"));
    }

    #[test]
    fn formatting_helpers() {
        assert_eq!(fmt_num(0), "0");
        assert_eq!(fmt_num(1_234_567), "1 234 567");
        assert_eq!(fmt_duration(5), "5 с");
        assert_eq!(fmt_duration(65), "1 мин 05 с");
        assert_eq!(fmt_signed(12), "+12");
        assert_eq!(fmt_signed(-1_234), "-1 234");
        assert_eq!(fmt_signed(0), "0");
        assert_eq!(md_cell("a|b"), "a\\|b");
    }
}
