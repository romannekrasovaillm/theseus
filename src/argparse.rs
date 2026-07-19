//! Парсинг аргументов командной строки theseus — библиотечный модуль.
//!
//! Логика вынесена из тонкого бинарника `main.rs` (паттерн «core as lib,
//! cli as thin bin»): набор флагов, дефолты и семантика позиционных
//! аргументов сохранены, но разбор строгий — вместо молчаливого
//! игнорирования битых значений возвращается [`ArgError`] с подсказкой.
//!
//! Поддерживаемые флаги (как в `main.rs`):
//! `-w`/`--workspace`, `-p`/`--prompt`, `--yolo`, `-m`/`--model`,
//! `--base-url`, `--context-limit`, `--max-turns`, `--resume`,
//! `--sessions`, `--fix`, `--inject-after-sec`, `--inject-text`, `--init`,
//! `-h`/`--help`, плюс подкоманда `doctor` первым позиционным аргументом.
//!
//! Пример (текстово, чтобы не зависеть от имени крейта в doctest):
//!
//! ```text
//! theseus doctor --fix             → Args { doctor: true, fix: true, .. }
//! theseus -p "собери отчёт" --yolo → headless-режим с авто-разрешениями
//! theseus --yoloo                  → Err(ArgError { message: «неизвестный
//!                                    флаг», usage_hint: «похожий: --yolo» })
//! ```

use std::fmt;
use std::path::PathBuf;

/// Текст справки (расширенная версия `USAGE` из `main.rs`: перечислены все
/// поддерживаемые флаги, включая тестовые `--inject-*`).
const USAGE: &str = "theseus — собственный агентный харнесс (DeepSeek V4-Pro)

ИСПОЛЬЗОВАНИЕ:
  theseus [опции] [задача]          TUI (задача опционально)
  theseus -p \"задача\" [--yolo]      headless-режим для тестов/CI
  theseus doctor [--fix]            диагностика окружения (как у тройки лидеров)

ОПЦИИ:
  -w, --workspace DIR        рабочий каталог (по умолчанию: текущий)
  -p, --prompt TEXT          headless-режим без TUI (или первая задача для TUI)
      --yolo                 авто-разрешение всех действий (кроме hard-deny)
  -m, --model NAME           модель (по умолчанию deepseek-v4-pro)
      --base-url URL         API-эндпоинт (по умолчанию https://api.deepseek.com/v1)
      --context-limit N      жёсткий лимит контекста в токенах (перекрывает конфиг)
      --max-turns N          лимит ходов агента (по умолчанию 40)
      --resume FILE          продолжить сессию из файла транскрипта
      --sessions             вывести список сессий каталога .theseus и выйти
      --fix                  (с doctor) попытаться автоматически починить проблемы
      --inject-after-sec N   тест: вставить --inject-text в prompt_slot через N сек
      --inject-text TEXT     тест: текст вставки (проверка преемпции стрима)
      --init                 создать пример ~/.config/theseus/config.toml
  -h, --help                 эта справка

ПОЗИЦИОННЫЕ АРГУМЕНТЫ:
  doctor                     подкоманда диагностики (только первым аргументом)
  <задача>                   первый прочий позиционный аргумент = prompt

В TUI: slash-команды (/help — полный список: /goal, /plan, /model, /skills,
  /memory, /sessions, /trace, /compact, /yolo, /quit), ↑/↓ — история ввода
  (~/.theseus/history).

КЛЮЧИ: env DEEPSEEK_API_KEY (обязателен). Транскрипты: <workspace>/.theseus/";

/// Все известные флаги — для подсказки «возможно, вы имели в виду…» при опечатках.
const KNOWN_FLAGS: &[&str] = &[
    "-w",
    "--workspace",
    "-p",
    "--prompt",
    "--yolo",
    "-m",
    "--model",
    "--base-url",
    "--context-limit",
    "--max-turns",
    "--resume",
    "--sessions",
    "--fix",
    "--inject-after-sec",
    "--inject-text",
    "--init",
    "-h",
    "--help",
];

/// Разобранные аргументы командной строки theseus.
///
/// Набор полей и их типы повторяют структуру `Args` из `main.rs`, чтобы
/// тонкий бинарник мог перейти на этот модуль без изменения диспетчеризации.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    /// Рабочий каталог (`-w`/`--workspace`; по умолчанию — текущий).
    pub workspace: PathBuf,
    /// Задача: из `-p`/`--prompt` либо первого позиционного аргумента.
    pub prompt: Option<String>,
    /// Авто-разрешение всех действий, кроме hard-deny (`--yolo`).
    pub yolo: bool,
    /// Имя модели (`-m`/`--model`).
    pub model: Option<String>,
    /// API-эндпоинт (`--base-url`).
    pub base_url: Option<String>,
    /// Жёсткий лимит контекста в токенах (`--context-limit`).
    pub context_limit: Option<usize>,
    /// Лимит ходов агента (`--max-turns`; по умолчанию 40).
    pub max_turns: usize,
    /// Создать пример конфига `~/.config/theseus/config.toml` и выйти (`--init`).
    pub init: bool,
    /// Показать справку и выйти (`-h`/`--help`).
    pub help: bool,
    /// Подкоманда диагностики (`theseus doctor`).
    pub doctor: bool,
    /// Автопочинка найденных проблем в doctor (`--fix`).
    pub fix: bool,
    /// Файл сессии для продолжения (`--resume`).
    pub resume: Option<PathBuf>,
    /// Вывести список сессий `<workspace>/.theseus` и выйти (`--sessions`).
    pub sessions: bool,
    /// Тестовый флаг: через сколько секунд вставить `inject_text` в prompt_slot.
    pub inject_after_sec: u64,
    /// Тестовый флаг: текст вставки (проверка преемпции стрима).
    pub inject_text: String,
}

impl Args {
    /// Значения по умолчанию — в точности как в парсере `main.rs`:
    /// `workspace` = текущий каталог (или `"."` при ошибке `current_dir`),
    /// `max_turns` = 40, `inject_after_sec` = 0, остальное — `None`/`false`/пусто.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            workspace: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            prompt: None,
            yolo: false,
            model: None,
            base_url: None,
            context_limit: None,
            max_turns: 40,
            init: false,
            help: false,
            doctor: false,
            fix: false,
            resume: None,
            sessions: false,
            inject_after_sec: 0,
            inject_text: String::new(),
        }
    }
}

impl Default for Args {
    fn default() -> Self {
        Self::defaults()
    }
}

/// Ошибка разбора аргументов командной строки.
///
/// Наряду с человекочитаемым `message` несёт `usage_hint` — подсказку,
/// как исправить вызов (похожий флаг при опечатке или отсылка к справке).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgError {
    /// Человекочитаемое описание проблемы (на русском).
    pub message: String,
    /// Подсказка по исправлению: похожий флаг или ссылка на `--help`.
    pub usage_hint: String,
}

impl fmt::Display for ArgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ошибка аргументов: {}\nподсказка: {}", self.message, self.usage_hint)
    }
}

impl std::error::Error for ArgError {}

/// Разобрать `argv` — вектор аргументов **включая имя программы** в `argv[0]`
/// (то есть ровно то, что возвращает `std::env::args().collect::<Vec<_>>()`).
///
/// Семантика повторяет парсер `main.rs`:
/// - `doctor` первым позиционным аргументом — подкоманда диагностики;
/// - первый прочий позиционный аргумент становится `prompt`, остальные
///   позиционные игнорируются;
/// - повторный флаг перезаписывает предыдущее значение (последний побеждает);
/// - значение флага берётся из следующего элемента argv как есть — даже если
///   оно начинается с `-` (иначе нельзя передать `--inject-text "-строка"`).
///
/// Отличие от `main.rs`: разбор строгий. Проблемы не проглатываются молча,
/// а возвращаются как [`ArgError`] — бинарнику остаётся напечатать
/// `{ ошибка }` и выйти с ненулевым кодом.
///
/// # Ошибки
/// - неизвестный флаг (начинается с `-` и отсутствует в списке) — с подсказкой
///   ближайшего похожего флага, если расстояние Левенштейна не больше 3;
/// - флаг со значением в конце argv без самого значения (`theseus -p`);
/// - нечисловое или отрицательное значение числового флага (`--max-turns abc`).
pub fn parse(argv: &[String]) -> Result<Args, ArgError> {
    let mut args = Args::defaults();
    // argv[0] — имя программы (конвенция C); пустой argv — валиден, даёт дефолты.
    let rest = argv.get(1..).unwrap_or(&[]);
    // Подкоманда doctor — только первым позиционным (как `codex doctor` / `claude doctor`).
    let rest = match rest.first() {
        Some(first) if first == "doctor" => {
            args.doctor = true;
            &rest[1..]
        }
        _ => rest,
    };

    let mut it = rest.iter();
    while let Some(token) = it.next() {
        let flag = token.as_str();
        match flag {
            "-w" | "--workspace" => args.workspace = PathBuf::from(take_value(&mut it, flag)?),
            "-p" | "--prompt" => args.prompt = Some(take_value(&mut it, flag)?.to_string()),
            "--yolo" => args.yolo = true,
            "-m" | "--model" => args.model = Some(take_value(&mut it, flag)?.to_string()),
            "--base-url" => args.base_url = Some(take_value(&mut it, flag)?.to_string()),
            "--context-limit" => {
                args.context_limit = Some(parse_num(take_value(&mut it, flag)?, flag)?);
            }
            "--max-turns" => args.max_turns = parse_num(take_value(&mut it, flag)?, flag)?,
            "--resume" => args.resume = Some(PathBuf::from(take_value(&mut it, flag)?)),
            "--sessions" => args.sessions = true,
            "--fix" => args.fix = true,
            "--inject-after-sec" => {
                args.inject_after_sec = parse_num(take_value(&mut it, flag)?, flag)?;
            }
            "--inject-text" => args.inject_text = take_value(&mut it, flag)?.to_string(),
            "--init" => args.init = true,
            "-h" | "--help" => args.help = true,
            other if other.starts_with('-') => return Err(unknown_flag(other)),
            other => {
                // Первый позиционный — задача для TUI/headless; лишние игнорируются.
                if args.prompt.is_none() {
                    args.prompt = Some(other.to_string());
                }
            }
        }
    }
    Ok(args)
}

/// Текст справки — тот же, что печатается по `-h`/`--help`.
#[must_use]
pub fn usage() -> &'static str {
    USAGE
}

/// Достать значение флага из итератора argv; ошибка, если значения нет.
fn take_value<'a, I>(it: &mut I, flag: &str) -> Result<&'a str, ArgError>
where
    I: Iterator<Item = &'a String>,
{
    it.next().map(String::as_str).ok_or_else(|| ArgError {
        message: format!("флагу {flag} не хватает значения"),
        usage_hint: format!("синтаксис: {flag} <значение>; справка — `theseus --help`"),
    })
}

/// Распарсить числовое значение флага; мусор — ошибка с подсказкой.
fn parse_num<T: std::str::FromStr>(value: &str, flag: &str) -> Result<T, ArgError> {
    value.parse::<T>().map_err(|_| ArgError {
        message: format!("некорректное числовое значение для {flag}: «{value}»"),
        usage_hint: format!("{flag} ожидает целое неотрицательное число; справка — `theseus --help`"),
    })
}

/// Ошибка «неизвестный флаг» с ближайшей похожей подсказкой (если нашлась).
fn unknown_flag(flag: &str) -> ArgError {
    let usage_hint = match suggest(flag) {
        Some(similar) => format!("похожий флаг: {similar}; полный список — в `theseus --help`"),
        None => "полный список флагов — в `theseus --help`".to_string(),
    };
    ArgError {
        message: format!("неизвестный флаг: «{flag}»"),
        usage_hint,
    }
}

/// Найти ближайший известный флаг (Левенштейн ≤ 3) для подсказки при опечатке.
fn suggest(unknown: &str) -> Option<&'static str> {
    KNOWN_FLAGS
        .iter()
        .copied()
        .map(|flag| (flag, levenshtein(unknown, flag)))
        .filter(|&(_, dist)| dist <= 3)
        .min_by_key(|&(_, dist)| dist)
        .map(|(flag, _)| flag)
}

/// Классическое расстояние Левенштейна по символам (не по байтам) —
/// динамическое программирование с двумя строками матрицы.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    // prev[j] — расстояние между обработанным префиксом `a` и b[..j].
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitution = prev[j] + usize::from(ca != cb);
            cur[j + 1] = substitution.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Собрать argv как у `std::env::args()`: имя программы первым элементом.
    fn argv(args: &[&str]) -> Vec<String> {
        std::iter::once("theseus").chain(args.iter().copied()).map(str::to_string).collect()
    }

    /// Тестовый сахар: разбор заведомо успешной командной строки.
    fn parse_ok(args: &[&str]) -> Args {
        parse(&argv(args)).unwrap()
    }

    /// Тестовый сахар: разбор заведомо ошибочной командной строки.
    fn parse_err(args: &[&str]) -> ArgError {
        parse(&argv(args)).unwrap_err()
    }

    #[test]
    fn defaults_match_main_rs() {
        let a = Args::defaults();
        assert_eq!(a.workspace, std::env::current_dir().unwrap());
        assert!(a.prompt.is_none());
        assert!(!a.yolo);
        assert!(a.model.is_none());
        assert!(a.base_url.is_none());
        assert!(a.context_limit.is_none());
        assert_eq!(a.max_turns, 40);
        assert!(!a.init);
        assert!(!a.help);
        assert!(!a.doctor);
        assert!(!a.fix);
        assert!(a.resume.is_none());
        assert!(!a.sessions);
        assert_eq!(a.inject_after_sec, 0);
        assert!(a.inject_text.is_empty());
    }

    #[test]
    fn default_trait_delegates_to_defaults() {
        assert_eq!(Args::default(), Args::defaults());
    }

    #[test]
    fn empty_argv_yields_defaults() {
        // Пустой argv и argv только с именем программы — дефолты, без паники.
        assert_eq!(parse(&[]).unwrap(), Args::defaults());
        assert_eq!(parse(&argv(&[])).unwrap(), Args::defaults());
    }

    #[test]
    fn workspace_short_and_long() {
        assert_eq!(parse_ok(&["-w", "/tmp/ws-a"]).workspace, PathBuf::from("/tmp/ws-a"));
        assert_eq!(parse_ok(&["--workspace", "/tmp/ws-b"]).workspace, PathBuf::from("/tmp/ws-b"));
    }

    #[test]
    fn prompt_short_and_long() {
        assert_eq!(parse_ok(&["-p", "первый"]).prompt.as_deref(), Some("первый"));
        assert_eq!(parse_ok(&["--prompt", "второй"]).prompt.as_deref(), Some("второй"));
    }

    #[test]
    fn first_positional_becomes_prompt_rest_ignored() {
        let a = parse_ok(&["собери", "отчёт"]);
        // Только первый позиционный — как в main.rs, остальные игнорируются.
        assert_eq!(a.prompt.as_deref(), Some("собери"));
    }

    #[test]
    fn positional_vs_prompt_flag_precedence() {
        // Флаг задан раньше → позиционный игнорируется.
        let a = parse_ok(&["-p", "из-флага", "позиционный"]);
        assert_eq!(a.prompt.as_deref(), Some("из-флага"));
        // Флаг позже → перезаписывает позиционный (семантика main.rs).
        let a = parse_ok(&["позиционный", "-p", "из-флага"]);
        assert_eq!(a.prompt.as_deref(), Some("из-флага"));
    }

    #[test]
    fn bool_flags_set_true() {
        let a = parse_ok(&["--yolo", "--sessions", "--init"]);
        assert!(a.yolo);
        assert!(a.sessions);
        assert!(a.init);
        assert!(!a.help);
        assert!(!a.doctor);
    }

    #[test]
    fn fix_without_doctor_is_allowed() {
        // --fix без doctor — легально: флаг просто выставлен (как в main.rs).
        let a = parse_ok(&["--fix"]);
        assert!(a.fix);
        assert!(!a.doctor);
    }

    #[test]
    fn help_short_and_long() {
        assert!(parse_ok(&["-h"]).help);
        assert!(parse_ok(&["--help"]).help);
    }

    #[test]
    fn model_short_and_long() {
        assert_eq!(parse_ok(&["-m", "deepseek-v4-pro"]).model.as_deref(), Some("deepseek-v4-pro"));
        assert_eq!(parse_ok(&["--model", "qwen3.5-4b"]).model.as_deref(), Some("qwen3.5-4b"));
    }

    #[test]
    fn base_url_takes_value() {
        let a = parse_ok(&["--base-url", "http://localhost:8080/v1"]);
        assert_eq!(a.base_url.as_deref(), Some("http://localhost:8080/v1"));
    }

    #[test]
    fn context_limit_valid_and_invalid() {
        assert_eq!(parse_ok(&["--context-limit", "131072"]).context_limit, Some(131_072));
        let err = parse_err(&["--context-limit", "много"]);
        assert!(err.message.contains("--context-limit"), "message: {}", err.message);
        assert!(err.message.contains("много"), "message: {}", err.message);
    }

    #[test]
    fn max_turns_valid_zero_and_invalid() {
        assert_eq!(parse_ok(&["--max-turns", "7"]).max_turns, 7);
        // Граница: ноль — валидное значение (агент без ходов).
        assert_eq!(parse_ok(&["--max-turns", "0"]).max_turns, 0);
        // Отрицательное и дробное — ошибка (usize).
        assert!(parse_err(&["--max-turns", "-3"]).message.contains("--max-turns"));
        assert!(parse_err(&["--max-turns", "4.5"]).message.contains("--max-turns"));
    }

    #[test]
    fn resume_takes_path() {
        let a = parse_ok(&["--resume", "/tmp/session-1.json"]);
        assert_eq!(a.resume, Some(PathBuf::from("/tmp/session-1.json")));
    }

    #[test]
    fn inject_flags_together_and_invalid_sec() {
        let a = parse_ok(&["--inject-after-sec", "5", "--inject-text", "пинг мир"]);
        assert_eq!(a.inject_after_sec, 5);
        assert_eq!(a.inject_text, "пинг мир");
        let err = parse_err(&["--inject-after-sec", "скоро"]);
        assert!(err.message.contains("--inject-after-sec"), "message: {}", err.message);
    }

    #[test]
    fn doctor_subcommand_first_positional() {
        let a = parse_ok(&["doctor"]);
        assert!(a.doctor);
        assert!(!a.fix);
        // Остальное — дефолты.
        assert_eq!(a.max_turns, 40);
        assert!(a.prompt.is_none());
    }

    #[test]
    fn doctor_with_fix_and_workspace() {
        let a = parse_ok(&["doctor", "--fix", "-w", "/tmp/diag"]);
        assert!(a.doctor);
        assert!(a.fix);
        assert_eq!(a.workspace, PathBuf::from("/tmp/diag"));
    }

    #[test]
    fn doctor_not_first_is_plain_prompt() {
        // doctor распознаётся только первым позиционным (как в main.rs).
        let a = parse_ok(&["--fix", "doctor"]);
        assert!(!a.doctor);
        assert!(a.fix);
        assert_eq!(a.prompt.as_deref(), Some("doctor"));
    }

    #[test]
    fn unknown_long_flag_suggests_similar() {
        let err = parse_err(&["--yoloo"]);
        assert!(err.message.contains("--yoloo"), "message: {}", err.message);
        assert!(err.usage_hint.contains("--yolo"), "hint: {}", err.usage_hint);
    }

    #[test]
    fn unknown_flag_without_close_match_gives_generic_hint() {
        let err = parse_err(&["--blabla"]);
        assert!(err.message.contains("--blabla"), "message: {}", err.message);
        assert!(err.usage_hint.contains("--help"), "hint: {}", err.usage_hint);
    }

    #[test]
    fn unknown_short_flag_errors() {
        let err = parse_err(&["-Z"]);
        assert!(err.message.contains("-Z"), "message: {}", err.message);
    }

    #[test]
    fn missing_value_is_an_error() {
        // main.rs молча игнорировал; строгий парсер — ошибка с именем флага.
        let err = parse_err(&["-p"]);
        assert!(err.message.contains("-p"), "message: {}", err.message);
        assert!(err.usage_hint.contains("--help"), "hint: {}", err.usage_hint);
        assert!(parse_err(&["--max-turns"]).message.contains("--max-turns"));
    }

    #[test]
    fn values_with_spaces_and_unicode_kept_verbatim() {
        let text = "собери отчёт за Q3 — «быстро», пожалуйста";
        assert_eq!(parse_ok(&["-p", text]).prompt.as_deref(), Some(text));
        let a = parse_ok(&["--inject-text", "a b  c\tд"]);
        assert_eq!(a.inject_text, "a b  c\tд");
    }

    #[test]
    fn repeated_flag_last_wins() {
        let a = parse_ok(&["-m", "первая", "--model", "вторая"]);
        assert_eq!(a.model.as_deref(), Some("вторая"));
        let a = parse_ok(&["-w", "/a", "--workspace", "/b"]);
        assert_eq!(a.workspace, PathBuf::from("/b"));
        let a = parse_ok(&["-p", "раз", "-p", "два"]);
        assert_eq!(a.prompt.as_deref(), Some("два"));
        let a = parse_ok(&["--max-turns", "10", "--max-turns", "20"]);
        assert_eq!(a.max_turns, 20);
    }

    #[test]
    fn flag_value_may_start_with_dash() {
        // Значение берётся из следующего argv-элемента как есть (как в main.rs).
        let a = parse_ok(&["--inject-text", "-не-флаг"]);
        assert_eq!(a.inject_text, "-не-флаг");
    }

    #[test]
    fn usage_mentions_every_flag_and_doctor() {
        let u = usage();
        for &flag in KNOWN_FLAGS {
            assert!(u.contains(flag), "в справке нет {flag}");
        }
        assert!(u.contains("doctor"));
    }

    #[test]
    fn error_implements_display_and_std_error() {
        let err = parse_err(&["--yoloo"]);
        let shown = format!("{err}");
        assert!(shown.contains(err.message.as_str()), "display: {shown}");
        assert!(shown.contains(err.usage_hint.as_str()), "display: {shown}");
        // ArgError — полноценная std-ошибка: стыкуется с anyhow в бинарнике.
        fn assert_std_error<E: std::error::Error>(_: &E) {}
        assert_std_error(&err);
    }

    #[test]
    fn suggest_finds_closest_flag() {
        assert_eq!(suggest("--promt"), Some("--prompt"));
        assert_eq!(suggest("--max_turns"), Some("--max-turns"));
        assert_eq!(suggest("--zzzzzzzz"), None);
    }

    #[test]
    fn levenshtein_basic_properties() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("abc", "abc"), 0);
        assert_eq!(levenshtein("abc", "abd"), 1);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("", "abcd"), 4);
        // Юникод считается по символам, а не по байтам.
        assert_eq!(levenshtein("флаг", "флак"), 1);
    }

    #[test]
    fn full_combination_all_fields() {
        let a = parse_ok(&[
            "-w", "/tmp/ws",
            "-p", "собери отчёт",
            "--yolo",
            "-m", "deepseek-v4-pro",
            "--base-url", "http://127.0.0.1:8080/v1",
            "--context-limit", "65536",
            "--max-turns", "12",
            "--resume", "/tmp/s1.json",
            "--sessions",
            "--fix",
            "--inject-after-sec", "3",
            "--inject-text", "пингуй",
            "--init",
        ]);
        assert_eq!(a.workspace, PathBuf::from("/tmp/ws"));
        assert_eq!(a.prompt.as_deref(), Some("собери отчёт"));
        assert!(a.yolo);
        assert_eq!(a.model.as_deref(), Some("deepseek-v4-pro"));
        assert_eq!(a.base_url.as_deref(), Some("http://127.0.0.1:8080/v1"));
        assert_eq!(a.context_limit, Some(65_536));
        assert_eq!(a.max_turns, 12);
        assert_eq!(a.resume, Some(PathBuf::from("/tmp/s1.json")));
        assert!(a.sessions);
        assert!(a.fix);
        assert_eq!(a.inject_after_sec, 3);
        assert_eq!(a.inject_text, "пингуй");
        assert!(a.init);
        assert!(!a.help);
        assert!(!a.doctor);
    }
}
