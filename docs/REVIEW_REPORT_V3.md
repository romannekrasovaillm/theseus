# 🔍 REVIEW REPORT v3: Theseus TUI Agent Harness

**Дата:** 2026-07-19
**Обозреватель:** Claude Code (Rust skills package — 6 навыков)
**Контекст:** третье ревью после REVIEW_REPORT.md (статический аудит) и RUNTIME_REVIEW.md (рантайм-прогоны)
**Метод:** полный обзор 76 исходных файлов (~48 000 строк) по всем 6 навыкам Rust-пакета

---

## 0. Статистика проекта

| Метрика | Значение |
|---------|----------|
| Строк Rust (src/) | ~48 000 |
| Файлов .rs | 76 (src/) + 4 (tests/) + 1 (benches/) |
| Модулей | 65 |
| Юнит-тестов | 1 100+ (все проходят) |
| Интеграционных тестов (мок) | 12 (все проходят) |
| Живых тестов (DeepSeek) | 19 (написаны) |
| Нагрузочных тестов | 3 (написаны) |
| Criterion-бенчмарков | 6 групп |
| Clippy | чист (0 warnings, deny-список из 24 правил) |
| Rust edition | 2021 |
| MSRV заявленный | 1.85 |

---

## 1. Новые находки — CRITICAL

### 1.1 `BgRegistry::output/stop` — `{id}` не интерполируется

**Файл:** `src/background.rs:75, 99`
**Серьёзность:** 🔴 CRITICAL
**Категория:** correctness

```rust
// СТРОКА 75
return "ERROR: задача {id} не найдена".to_string();
//                           ^^^^^ литерал, не переменная!

// СТРОКА 99 — тот же баг
return "ERROR: задача {id} не найдена".to_string();
```

**Почему баг:** `"..."` — строковый литерал, не `format!()`. Пользователь увидит буквально `ERROR: задача {id} не найдена` вместо `ERROR: задача 42 не найдена`. Полная потеря диагностики при сбое `task_output`/`task_stop`.

**Сценарий:** агент запустил фоновую задачу → задача упала/завершилась → агент вызывает `task_output(id)` → `BgRegistry` не находит id (гонка с удалением?) → пользователь видит бессмысленное сообщение, не может понять какая задача пропала.

**Исправление:**
```rust
format!("ERROR: задача {id} не найдена")
```

---

## 2. Новые находки — MEDIUM

### 2.1 `PermissionEngine::hard_deny` — теоретическая паника на пустом `deny_res`

**Файл:** `src/permissions.rs:193-198`
**Серьёзность:** 🟡 MEDIUM
**Категория:** correctness

```rust
fn hard_deny(&self, cmd: &str) -> Decision {
    let idx = self.deny_set.matches(cmd).into_iter().next().unwrap_or(0);
    let pat = self.deny_res[idx].as_str();  // ПАНИКА если deny_res пуст
    ...
}
```

**Сценарий:** `RegexSet::new()` на пользовательских паттернах падает → fallback на пустой `RegexSet::new::<_, &&str>(&[])` (успех, 0 паттернов). При этом `deny_res` строится из тех же паттернов через `filter_map` — если ВСЕ паттерны не скомпилировались по отдельности, `deny_res` пуст. Индекс `0` → panic.

**На практике:** встроенные `bash_deny_patterns` жёстко зашиты и валидны. Но при кастомном конфиге с битым regex — креш всего процесса.

**Исправление:**
```rust
let pat = self.deny_res.get(idx).map(|r| r.as_str()).unwrap_or("<?>");
```

### 2.2 Дублирование движков хуков

**Файлы:** `src/hooks.rs` + `src/hooks_ext.rs`
**Серьёзность:** 🟡 MEDIUM
**Категория:** architecture / simplification

В коде одновременно живут две системы хуков:

| | hooks.rs (старый) | hooks_ext.rs (новый) |
|---|---|---|
| События | PreToolUse, PostToolUse, UserPromptSubmit, SessionStart, SessionEnd | PreCompact, PostCompact, SessionStart, SessionEnd, GoalSet |
| Запуск | `run_hooks()` в `execute()` | `fire_ext()` в `run_with()`/`maybe_compact()` |
| Блокировка | exit 2 = block | нет блокировки |

События `SessionStart` и `SessionEnd` **дублируются** — срабатывают ОБЕ системы:
- `run_with()` строка 548: `fire_ext(ExtHookEvent::SessionStart, ...)`
- `run_with()` строка 549: `run_hooks(HookEvent::UserPromptSubmit, ...)` — старый

Аргументировано «обратной совместимостью», но удваивает вызовы хуков, усложняет отладку и конфигурацию. Рекомендуется мигрировать на hooks_ext полностью в v0.4.

---

## 3. Новые находки — LOW

### 3.1 `SESSION_CLOSED` протекает в пользовательский вывод

**Файл:** `src/tools.rs:201`
**Серьёзность:** 🔵 LOW
**Категория:** correctness
**Статус:** отмечено в RUNTIME_REVIEW.md, не исправлено

```rust
Ok(format!("SESSION_CLOSED: {s}"))
```

Префикс `SESSION_CLOSED` — внутренний маркер инструмента `finish`. Попадает в `AgentEvent::Finished` → пользователь видит `=== SESSION_CLOSED: ...`. Нужно отрезать префикс перед показом пользователю.

### 3.2 `DefaultHasher` нестабилен между запусками

**Файл:** `src/agent/mod.rs:239` и `src/agent/detectors.rs:4`
**Серьёзность:** 🔵 LOW
**Категория:** reference

```rust
fn fingerprint(name: &str, args: &serde_json::Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
```

**Уже документировано** в комментарии: «нестабилен между запусками процесса — допустимо, т.к. fingerprint используется только внутри одной сессии». Для персистентных хэшей есть `compact_v2::simhash64`.

### 3.3 `PersistentShell::TOKEN_COUNTER` — Relaxed ordering

**Файл:** `src/shell.rs:47, 52-56`
**Серьёзность:** 🔵 LOW
**Категория:** reference

```rust
static TOKEN_COUNTER: AtomicU64 = AtomicU64::new(0);
fn next_token() -> String {
    let n = TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed);
    ...
}
```

`Ordering::Relaxed` для атомарного `fetch_add` — **корректно** (гарантирует уникальность даже с Relaxed). Но `PersistentShell` не `Sync` — конкурентный `exec` на одном экземпляре невозможен. Стоит документировать в rustdoc.

### 3.4 Блокирующий HTTP-клиент

**Файл:** `src/api.rs:78`
**Серьёзность:** 🔵 LOW
**Категория:** architecture

```rust
pub struct ApiClient {
    http: reqwest::blocking::Client,
```

Весь API-клиент — `reqwest::blocking`. Для TUI-приложения допустимо (API-вызовы блокируют цикл), но ограничивает распараллеливание API-вызовов и субагентов. Рефакторинг на async — полная переделка цикла.

---

## 4. Неисправленное из предыдущих ревью

| # | Находка | Файл | Серьёзность | Статус |
|---|---------|------|-------------|--------|
| 1 | `compact_v2.rs` — 593 строки не подключены к проекту | `src/lib.rs` | 🟡 MEDIUM | ❌ не исправлено |
| 2 | `SESSION_CLOSED` протекает в вывод | `src/tools.rs:201` | 🔵 LOW | ❌ не исправлено |
| 3 | `#[non_exhaustive]` на публичных enum'ах конфига | `src/config.rs` | 🔵 LOW | ❌ не исправлено |
| 4 | `convo.len() < 800` → chars count (кириллица) | `src/agent/mod.rs:437` | 🔵 LOW | ✅ исправлено |

---

## 5. Сильные стороны (подтверждено и дополнено)

### Безопасность
- **Sandbox:** Landlock (ядро 5.13+) + bubblewrap fallback-матрица. 4 уровня изоляции. Тесты проверяют реальную ФС-изоляцию
- **Permissions:** 4 слоя (hard-deny → user rules → whitelist → mode). `execpolicy` с каноникализацией shell-команд (кавычки, пайпы, экранирование). 35 unit-тестов
- **Secrets:** `Redactor` с 7 встроенными правилами + env-keys. Маскировка перед записью в транскрипт. Идемпотентна. Бенчмарк: <5 сек на 1 МиБ текста
- **read-confinement:** read-only команды отслеживают абсолютные пути вне workspace
- **anti-bypass:** редиректы вывода вне workspace детектируются отдельно

### Надёжность
- **Shell:** `PersistentShell` — coproc с маркерным протоколом. 25 тестов: cd/export/functions persist, таймауты, восстановление после `exit`, многобайтовый вывод
- **Детекторы цикла:** doom-loop (fingerprint в окне 20), exploration spiral (5+ read), deny-repeat, doom-text (идентичный ответ модели). Reminder-лимиты предотвращают спам
- **Компактификация:** трёхуровневая L1 (mask 70%) → L2 (dedup+prune 80%) → L3 (LLM-summary 95%). On-error триггер на context length
- **Трейсинг:** JSONL-поток спанов (api_call, tool_exec, compact) с атрибутами. Сессии snapshot для `--resume`

### Код
- **Архитектура:** «Core as lib, CLI as thin bin» (main.rs ~130 строк). 76 модулей с чёткими границами ответственности
- **Конфиг:** слоёный (defaults < global < workspace < CLI < env) с валидацией. Две схемы (legacy + codex-стиль)
- **Тесты:** 1100+ unit, 12 integration mock, 19 live DeepSeek, 3 stress, 6 criterion benchmarks
- **API-дизайн:** `BwrapSpec::builder()`, `SecretRule::new()`, `PromptBuilder` — чистые билдеры
- **Обработка ошибок:** `thiserror` для библиотечных типов (`BwrapError`), `anyhow` для `main()`

---

## 6. Чеклист по 6 навыкам пакета

### `rust-idiomatic-code`
- [x] Newtype, typestate, RAII-guard — паттерны документированы
- [x] `thiserror` для библиотечных ошибок
- [x] `anyhow` с `.context()` для приложения
- [x] Конструкторы `new()`, билдер `BwrapSpecBuilder`
- [x] `let else`, `matches!` — используются
- [x] Clippy deny-список из 24 правил (стиль codex-rs)
- [⚠] `compact_v2.rs` не в lib.rs — 593 строки мёртвого кода
- [⚠] `#[non_exhaustive]` отсутствует на публичных enum'ах конфига

### `rust-async-concurrency`
- [x] `std::thread::scope` для параллельных read-only
- [x] `mpsc::channel` с таймаутами — корректно
- [⚠] Блокирующий HTTP-клиент (осознанно, но ограничивает)
- [⚠] `Ordering::Relaxed` для `TOKEN_COUNTER` — документировать

### `rust-testing`
- [x] 1100+ unit-тестов, 12 integration mock
- [x] 19 живых тестов DeepSeek v4-pro
- [x] `criterion` с `black_box` — 6 групп бенчмарков
- [x] Мок SSE для интеграционных тестов
- [⚠] Property-based тесты только на конфиге — добавить proptest на roundtrip сессий и каноникализацию команд

### `rust-project-setup`
- [x] `Cargo.toml`: edition="2021", rust-version="1.85", license, description
- [x] `[profile.release]` lto="thin"
- [⚠] Нет CI (`.github/workflows/ci.yml`)
- [⚠] Нет `cargo-deny`, `cargo-audit`
- [⚠] Edition 2021 → 2024 (Rust 1.85+ полностью поддерживает)

### `rust-performance`
- [x] `lto = "thin"` в release
- [x] Criterion-бенчмарки: simhash 87µs/KB, hamming 12µs/10K, est_tokens <500ns
- [x] `with_capacity` в каноникализаторе команд
- [x] Итераторы вместо индексации
- [⚠] `emit()` клонирует `AgentEvent` (String) — для TUI приемлемо

### `rust-unsafe-ffi`
- [x] `lib.rs:159` — `isatty(fd)` с SAFETY-комментарием
- [x] `sandbox.rs:51-57` — `pre_exec` unsafe с контрактом
- [x] `sandbox_bwrap.rs:495-503` — аналогично
- [x] `#![forbid(unsafe_code)]` в модулях без unsafe
- [x] Miri-ready: unsafe изолирован в трёх модулях

---

## 7. Рекомендации (приоритет)

### 🔴 Высокий
1. **Починить `format!` в `BgRegistry::output/stop`** — `background.rs:75, 99` (2 строки)
2. **Добавить `.get(idx)` в `hard_deny`** — `permissions.rs:197` (1 строка)

### 🟡 Средний
3. **Интегрировать `compact_v2.rs`** — добавить `pub mod compact_v2;` в `lib.rs`, провести A/B сравнение с текущим `compact.rs`
4. **Консолидировать движки хуков** — мигрировать на `hooks_ext.rs`, удалить дублирующиеся SessionStart/SessionEnd
5. **Добавить CI** — минимальный набор: fmt, clippy, test, doc. Шаблон в rust-project-setup skill
6. **Мигрировать на Edition 2024** — `unsafe_op_in_unsafe_fn` по умолчанию, улучшенный RPIT

### 🔵 Низкий
7. **Отрезать `SESSION_CLOSED` префикс** — `tools.rs:201` или `agent/mod.rs:814`
8. **Добавить `#[non_exhaustive]`** на публичные enum'ы конфига
9. **Добавить proptest** на roundtrip сессий и каноникализацию команд
10. **Обновить зависимости**: reqwest 0.11→0.13, ratatui 0.26→0.30

---

## 8. Заключение

**Theseus — зрелый промышленный харнесс.** Качество кода, тестовое покрытие и архитектурные решения находятся на уровне референсных реализаций (codex-rs). Двойной sandbox (Landlock+bubblewrap), 4-слойные разрешения с каноникализатором shell-команд, трёхуровневая компактификация, MCP-клиент, трейсинг спанов и маскировка секретов — полный набор для production-использования.

**Ключевые новые находки этого ревью:**
1. 🔴 `background.rs:75,99` — `{id}` не интерполируется (потеря диагностики)
2. 🟡 `permissions.rs:197` — потенциальная паника на пустом `deny_res`
3. 🟡 Дублирование движков хуков (hooks.rs + hooks_ext.rs)
4. 🔵 `compact_v2.rs` всё ещё не интегрирован

**Всего находок:** 2 CRITICAL/MEDIUM (новые), 4 LOW (новые + неподтверждённые старые), 4 архитектурных замечания.
