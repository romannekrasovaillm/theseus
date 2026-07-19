# Заметки по тройке лидеров — чек-лист для Theseus

Источники: `/home/roman/experiments/harness-review/{codex,grok-build,claude-code-source-code-main}`.
Сверяться с этим файлом при ЛЮБОМ изменении архитектуры Тесея. При конфликте с Rust-скиллами
(`0710_v1/Rust/*`) — приоритет тройки (прямое указание пользователя 18.07.2026).

## 1. Структура кода

| Паттерн | Codex (codex-rs) | Grok Build (crates/) | Claude Code (TS) | Theseus |
|---|---|---|---|---|
| core-lib + thin bin | workspace ~100 крейтов: `core/` (логика) + `cli/`, `tui/`, `exec/` тонкие | `crates/codegen/xai-grok-*`: логика в lib-крейтах, `bin/` тонкий | `src/` монолит, но слои разделены | ✅ `lib.rs` (вся логика) + тонкий `main.rs` (args→диспетчер) |
| Модульность агента | `core/src/agent*.rs`, `compact*.rs`, `client*.rs` — по файлу на ответственность | отдельные крейты: http, mcp, workspace, test-support | — | ✅ `src/agent/{mod,events,compact,execute,detectors}.rs` |
| Тесты рядом с кодом | `*_tests.rs` рядом с модулем | `xai-grok-test-support` крейт | — | ✅ `#[cfg(test)]` в модулях (23 unit) + live e2e скрипты |

## 2. Агентный цикл

- **Codex**: mailbox-преемпция — пользовательский ввод не ждёт границу хода, прерывает стрим;
  `compact.rs` + `compact_token_budget.rs` + remote-компактификация; `command_canonicalization` для прав.
- **Grok Build**: серверный `doom_loop` сигнал; триггер on-error compact&resubmit; SSE-стриминг
  с дельтами; тесты через `test-support/sse.rs` (мок SSE — как наши live с DeepSeek, но быстрее).
- **Claude Code**: хуки (pre/post tool), skills как SKILL.md, subagents с изолированным контекстом,
  `/doctor` для диагностики окружения, permissions с правилами `Tool(pattern)`.
- **Theseus**: ✅ преемпция по prompt_slot; ✅ doom/spiral детекторы (эвристики окна);
  ✅ on-error L3-компактификация и повтор; ✅ хуки/скиллы/субагент-explore/doctor;
  ✅ permission rules `Bash(rm)` стиля.

## 3. Правки файлов (edit_file)

- **Claude Code**: fuzzy-каскад из ~9 матчеров (exact → line-trim → block-anchor → whitespace-normalized → indent-flexible → escape-normalized → trimmed-block → context-match → multi-occurrence error).
- **Codex**: `apply-patch` крейт с Lark-грамматикой в API (патч-формат частью контракта модели).
- **Theseus**: ✅ каскад из 5 матчеров (exact, line-trim, whitespace-normalized, indent-flex, block-anchor). При проблемах с точностью правок — расширять в сторону Claude (escape-normalized, context-match).

## 4. Безопасность / sandbox

- **Codex**: крейты `linux-sandbox` (landlock), `bwrap`, `windows-sandbox-rs`, `process-hardening`,
  `execpolicy`, `shell-escalation` — kernel-enforced + политика эскалации.
- **Claude Code**: seatbelt (macOS) / bubblewrap (Linux), доменные allow-list для web_fetch.
- **Theseus**: ✅ landlock probe→enforce (Partial принимается), deny-правила hard, web_allowed_domains,
  секреты только через env/подстановку. Ограничение: нет seccomp/bwrap-слоя поверх — принято.

## 5. MCP

- **Codex**: `rmcp-client`, `mcp-server`, `stdio-to-uds` (полные транспорты).
- **Grok**: `xai-grok-mcp` + `acp_transport`.
- **Claude Code**: stdio + HTTP + OAuth + elicitation.
- **Theseus**: ✅ stdio + HTTP с bearer/elicit (минимум). OAuth не нужен для локальных серверов.

## 6. Компактификация (трёхуровневая, по статье из библиотеки + Grok/Codex)

- L1 (70%): маскировка старых tool_result («[содержимое скрыто]»), system+goal неприкосновенны.
- L2 (80%): прунинг + дедуп повторных чтений одного файла, сохранение скелета диалога.
- L3 (95% или on-error): LLM-саммари всей истории в «заметки для продолжения».
- Пороги — в конфиге (`compact_l1/l2/l3` доли лимита контекста). ✅ реализовано + unit-тесты.

## 7. Диагностика

- **Claude Code `/doctor`**, **Kimi `kimi doctor config`**: проверки окружения до запуска.
- **Theseus**: ✅ `theseus doctor [--fix]` — api_key, GET /models, landlock, workspace, regex правил,
  web-домены, MCP-коннект, скиллы, память, пороги компакта. Не звать платные chat-эндпоинты.

## 8. Наблюдаемость

- **Codex**: `otel`, `rollout-trace`, `analytics`, `feedback` — полный трейсинг.
- **Theseus**: ✅ транскрипты `session-*.jsonl` в `<workspace>/.theseus/`, события AgentEvent,
  учёт токенов (Accounting); v0.4 добавлены `trace.rs` (спаны + chrome-trace экспорт,
  jsonl-поток в `.theseus/trace-*.jsonl`) и `telemetry.rs` (counter/gauge/histogram +
  Prometheus-экспорт). Полный otel не тащим (YAGNI для локального харнесса).

## 9. Волны расширения v0.4 (маппинг новых модулей на паттерны лидеров)

| Паттерн лидера | Модуль Theseus |
|---|---|
| codex `apply-patch` (+Lark-грамматика в API) | `patch.rs`, `larkpatch.rs` (strict-валидатор + digest для промпта) |
| Claude fuzzy-каскад ~9 матчеров | `matchers.rs` (8 уровней + multi-occurrence вентиль), интегрирован в `tools.rs::edit_file` |
| codex `command_canonicalization` + `execpolicy` | `execpolicy.rs`, интегрирован в `permissions.rs` (worst-of двух проходов) |
| codex `rollout-trace` / `otel` | `trace.rs`, `telemetry.rs` |
| codex `prompts` / environment_context | `prompts.rs` (PromptBuilder, EnvContext), интегрирован в `agent/mod.rs` |
| codex `model-provider-info` | `models.rs` (deepseek/kimi/moonshot/openai-compatible + nearest-подсказки) |
| codex `thread-store` / message-history | `session.rs` (дерево resume, fork, lazy-листинг) |
| codex `shell-command` / user-shell | `shell.rs` (PersistentShell, cwd/env переживают вызовы) |
| codex `file-watcher` | `filewatcher.rs` (поллинг + EditGuard конфликт-детект) |
| codex `linux-sandbox` / `bwrap` | `sandbox.rs` (landlock), `sandbox_bwrap.rs` (probe + fallback-матрица) |
| codex `core-skills` / Claude skills | `skills.rs` + digest в промпте |
| Claude subagents | `agents.rs` (спеки explore/plan/code_review/test_runner + бюджеты), `subagent.rs` (рантайм) |
| Claude TodoWrite | `todo.rs` (set_full, гейт finish), интегрирован в `tools.rs` + `agent` |
| codex `notify` | `notify.rs` (bell/command/log + троттлинг) |
| kimi CLI cron | `cron.rs` (5-полевой парсер, коалесцинг, джиттер) |
| codex `memories` | `memory.rs` + `memory_v2.rs` (теги, confidence, decay, конфликты) |
| codex `config` (слои) | `config_layers.rs`, интегрирован в `Config::load` |
| Claude Code /doctor, /init | `doctor.rs`, `doctor_ext.rs`, `doctor_fix.rs` (--fix), `onboarding.rs` |
| codex `mcp-*` / grok `xai-grok-mcp` (+acp) | `mcp.rs`, `mcp_ext.rs` (resources/prompts), `acp.rs` |
| codex `hooks` | `hooks.rs` + `hooks_ext.rs` (8 событий, exit-2 блок, параллель) |
| codex retry/клиент | `retry.rs` (матрица классов ошибок, экспонента + jitter) |
| codex `git-utils` | `gitutil.rs` (branch/status/log/diff-stat, таймауты) |
| goal-аудит (Claude finish-дискриплина) | `audit.rs` (критерии, доказательства, вердикты) |
| reedline/bash history | `history.rs` (draft-буфер, персистентность) |
| Claude slash-команды | `slash.rs` (реестр 14 команд, подсказки), интегрирован в TUI |
| текст-утилиты core | `textutil.rs` (est_tokens, truncate, wrap, strip_ansi) |
| codex `secrets` (редакция) | `secrets.rs` (7 правил + env-значения) |
| codex tui styles / keybindings | `theme.rs`, `keymap.rs` |
| markdown-рендер ответов | `markdown.rs` (ANSI + strip режимы) |
| codex mailbox (преемпция) | `scheduler.rs` (приоритеты, TTL, merge) |
| codex limits/quotas | `limits.rs` (квоты сессии, warn 80%) |
| codex `process-hardening` | landlock + execpolicy + `shell_escape.rs` |
| OpenDev ACC (семантический дедуп) | `compact_v2.rs` (simhash64, hamming-кластеры) |
| prompt caching практики | `prompt_cache.rs` (LRU префиксов, hit_rate) |
| codex `file-search` | `workspace_map.rs` (scan + глоббалка) |
| CLI-парсинг | `argparse.rs` (строгий, с подсказками) |
| semver/MSRV | `semver.rs` |
| grok `test-support/sse.rs` | `mock_sse.rs` (+ фасад MockServer для бинарных e2e) |
| тестовые пирамиды тройки | 1032 unit + 12 integration (мок) + 25 live DeepSeek (#[ignore]) + criterion-бенчи |

## Что осознанно НЕ перенимаем

- Монорепо на ~100 крейтов (codex) — для одного разработчика overkill; один крейт + lib.rs.
- Bazel/remote-компактификация/облачные таски — не наш масштаб.
- OAuth для MCP — нет внешних серверов с auth.
- Полный otel-трейсинг — хватает trace.rs/telemetry.rs.
