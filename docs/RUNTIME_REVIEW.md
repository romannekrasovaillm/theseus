# Ревью запуска Theseus — рантайм-анализ

**Дата:** 2026-07-18
**Метод:** 3 headless-прогона с DeepSeek v4-pro (--yolo, --max-turns 5)

---

## Результаты прогонов

| # | Задача | Ходов | API-вызовов | Токены | Время | Итог |
|---|--------|-------|-------------|--------|-------|------|
| 1 | read Cargo.toml + описать | 2 | 3 | 6,620+1,136 | 19s | ✅ finish |
| 2 | bash с падающей командой | 2 | 3 | 4,945+1,260 | 23s | ✅ finish (exit 127) |
| 3 | write_file → read_file → finish | 4 | 5 | 9,348+572 | 12s | ✅ finish |

---

## Проблемы

### 1. CRITICAL: Sandbox блокирует `/dev/null`

```
/etc/profile.d/Z99-cloudinit-warnings.sh: строка 7: /dev/null: Отказано в доступе
```

Landlock sandbox (`src/sandbox.rs`) даёт rw доступ только на workspace и `/tmp`. Bash-профиль пытается писать в `/dev/null` → Permission denied. Это не ломает агента, но:
- Каждый bash-вызов начинается с мусора в stderr от профильных скриптов
- Может сломать скрипты, полагающиеся на `>/dev/null` для подавления вывода
- Нужно добавить `/dev/null` в rw-бинды Landlock

**Файл:** `src/sandbox.rs` — `enforce_workspace()`  
**Исправление:** добавить `landlock::path_beneath("/dev/null", landlock::AccessFs::WriteFile)` или аналогичный rw-бинд.

### 2. MEDIUM: Фрагментация дельт в ANSI-выводе

```
[ход 2 | ~2089 ток | Yolo]
[90m##[0m[90m These[0m[90mus[0m[90m —[0m[90m крат[0m[90mкое[0m[90m оп[0m[90mисание[0m[90m
```

Каждый чанк `AgentTextDelta` оборачивается в `\x1b[90m...\x1b[0m` (print_event, lib.rs:107-110). При стриминге это создаёт визуальный шум — русские буквы разрываются ANSI-кодами посреди слова.

**Файл:** `src/lib.rs:107-110` — `print_event()`  
**Причина:** `print!` + `flush` на каждый чанк, каждый со своими ANSI-кодами  
**Исправление:** либо буферизовать дельты и печатать строками, либо не сбрасывать ANSI между чанками (один открывающий код в начале стрима, один закрывающий в конце).

### 3. LOW: `SESSION_CLOSED` протекает в вывод пользователя

```
=== SESSION_CLOSED: Создан файл test.txt с содержимым PLAN_MODE_TEST.
```

Префикс `SESSION_CLOSED` — внутренний маркер инструмента finish (tools.rs:201). Он попадает в финальный вывод пользователю, хотя пользователю нужно только резюме.

**Файл:** `src/tools.rs:201` — `Ok(format!("SESSION_CLOSED: {s}"))`  
**Исправление:** разделить tool_result (для модели) и user_output (для пользователя). Либо отрезать префикс в `run_with` перед `AgentEvent::Finished`.

---

## Положительные находки

| Аспект | Наблюдение |
|--------|------------|
| **Error recovery** | Агент получил exit code 127, НЕ повторил команду, сразу finish — цикл контроля работает |
| **Sandbox enforced** | `[debug] run_bash sandbox_on=true status=Available` — Landlock активен и применяется |
| **Parallel readonly** | read_file помечен `[Allow (parallel)]` — parallel_readonly работает |
| **MCP live** | Оба MCP-сервера (mock + httpmock) подключились, дали 3 инструмента |
| **Accounting** | Точный учёт: calls, prompt_tokens, completion_tokens, latency |
| **Memory** | После finish — консолидация в MEMORY.md (+4-5 фактов за сессию) |
| **Turn limit** | --max-turns соблюдается: агент не превышает лимит |
| **Tool chaining** | write_file → read_file → finish — полный цикл без ошибок |

---

## Статистика API-расхода (3 прогона)

| Метрика | Значение |
|---------|----------|
| Всего API-вызовов | 11 |
| Prompt токенов | 20,913 |
| Completion токенов | 2,968 |
| Общее время | 54 секунды |
| Среднее за ход | 1,901 prompt + 270 completion токенов |
| Консолидация памяти | +14 фактов |

---

## Итог

**Агент работоспособен.** Три прогона — три успешных finish. Ключевой баг — `/dev/null` заблокирован Landlock'ом, ломает bash-профили. Остальные проблемы косметические (ANSI-фрагментация, SESSION_CLOSED в выводе).

**Рекомендуемый порядок исправлений:**
1. Sandbox: добавить `/dev/null` в rw-бинды (1 строка в sandbox.rs)
2. ANSI: не закрывать escape-код между дельтами (2 строки в print_event)
3. SESSION_CLOSED: отрезать префикс в финальном выводе (1 строка в agent/mod.rs)
