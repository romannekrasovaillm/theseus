//! Реестр slash-команд TUI (по образцу slash-команд Claude Code и kimi CLI).
//!
//! Slash-команда — короткая команда вида `/help`, вводимая пользователем
//! прямо в строке ввода TUI. Команды бывают трёх видов ([`SlashKind`]):
//! локальные (обрабатываются клиентом без обращения к модели), требующие
//! агента (приводят к запросу к LLM) и мгновенные (выполняются немедленно,
//! не дожидаясь завершения текущего хода агента).
//!
//! Модуль самодостаточный: только `std`. Публичный контракт:
//!
//! - [`SlashCmd`] — описание одной команды (имя, алиасы, сводка, usage, вид);
//! - [`builtin_commands`] — вектор встроенных команд харнесса;
//! - [`parse`] — разбор строки ввода в [`Parsed`];
//! - [`help_page`] / [`help_index`] — страницы справки;
//! - [`validate_commands`] — проверка реестра (уникальность имён и алиасов).

use std::collections::HashSet;

/// Максимальное число подсказок в ответе «неизвестная команда».
const MAX_SUGGESTIONS: usize = 3;

/// Максимальное расстояние Левенштейна, при котором имя ещё считается
/// похожим на введённое (кандидаты с префиксным совпадением проходят всегда).
const MAX_SUGGESTION_DISTANCE: usize = 2;

/// Вид slash-команды: как её исполняет харнесс.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SlashKind {
    /// Локальная: обрабатывается клиентом (TUI) без запроса к модели.
    /// Примеры: `/help`, `/sessions`, `/model`.
    Local,
    /// Требует агента: исполнение приводит к обращению к LLM.
    /// Примеры: `/compact` (суммаризация), `/goal` (goal-режим).
    NeedsAgent,
    /// Мгновенная: выполняется немедленно, не дожидаясь окончания
    /// текущего хода агента. Примеры: `/quit`, `/yolo`.
    Immediate,
}

impl SlashKind {
    /// Короткая русская метка вида для справки и таблиц.
    pub fn label(&self) -> &'static str {
        match self {
            SlashKind::Local => "локальная",
            SlashKind::NeedsAgent => "требует агента",
            SlashKind::Immediate => "мгновенная",
        }
    }
}

/// Описание одной slash-команды.
///
/// Все поля — статические строки: реестр встроенных команд задан
/// в исходниках и не требует аллокаций при чтении.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashCmd {
    /// Основное имя команды без слеша: строчные ASCII-буквы, цифры, `-`.
    pub name: &'static str,
    /// Альтернативные имена (тоже без слеша). Могут быть пустым списком.
    /// Алиасы не должны пересекаться ни с именами, ни с алиасами других
    /// команд — это проверяет [`validate_commands`].
    pub aliases: &'static [&'static str],
    /// Однострочная сводка: что делает команда.
    pub summary: &'static str,
    /// Строка использования, например `/model [имя-модели]`.
    pub usage: &'static str,
    /// Вид исполнения команды.
    pub kind: SlashKind,
}

impl SlashCmd {
    /// Совпадает ли токен (слово после слеша, без слеша) с этой командой —
    /// по имени или любому алиасу, без учёта регистра.
    pub fn matches(&self, token: &str) -> bool {
        self.name.eq_ignore_ascii_case(token)
            || self.aliases.iter().any(|alias| alias.eq_ignore_ascii_case(token))
    }
}

/// Встроенные команды харнесса (статическая таблица).
static BUILTIN: &[SlashCmd] = &[
    SlashCmd {
        name: "help",
        aliases: &["h", "?"],
        summary: "Справка по командам TUI",
        usage: "/help [команда]",
        kind: SlashKind::Local,
    },
    SlashCmd {
        name: "doctor",
        aliases: &["health"],
        summary: "Диагностика окружения и конфигурации",
        usage: "/doctor",
        kind: SlashKind::Local,
    },
    SlashCmd {
        name: "compact",
        aliases: &[],
        summary: "Сжатие истории диалога (суммаризация моделью)",
        usage: "/compact [инструкция]",
        kind: SlashKind::NeedsAgent,
    },
    SlashCmd {
        name: "model",
        aliases: &["m"],
        summary: "Показать или сменить текущую модель",
        usage: "/model [имя-модели]",
        kind: SlashKind::Local,
    },
    SlashCmd {
        name: "mode",
        aliases: &[],
        summary: "Режим разрешений: с подтверждением / полуавтомат / автомат",
        usage: "/mode [ask|semi|yolo]",
        kind: SlashKind::Immediate,
    },
    SlashCmd {
        name: "new",
        aliases: &[],
        summary: "Новая сессия: очистить историю диалога и начать с чистого листа",
        usage: "/new",
        kind: SlashKind::Immediate,
    },
    SlashCmd {
        name: "clear",
        aliases: &[],
        summary: "Очистить лог и историю диалога (как /new)",
        usage: "/clear",
        kind: SlashKind::Immediate,
    },
    SlashCmd {
        name: "skills",
        aliases: &[],
        summary: "Список доступных скиллов",
        usage: "/skills [фильтр]",
        kind: SlashKind::Local,
    },
    SlashCmd {
        name: "memory",
        aliases: &["mem"],
        summary: "Просмотр и редактирование памяти агента",
        usage: "/memory [show|edit|clear]",
        kind: SlashKind::Local,
    },
    SlashCmd {
        name: "goal",
        aliases: &[],
        summary: "Постановка цели для goal-режима",
        usage: "/goal <цель>",
        kind: SlashKind::NeedsAgent,
    },
    SlashCmd {
        name: "plan",
        aliases: &[],
        summary: "Режим планирования: сначала план, потом исполнение",
        usage: "/plan [задача]",
        kind: SlashKind::NeedsAgent,
    },
    SlashCmd {
        name: "sessions",
        aliases: &["ls"],
        summary: "Список сессий и переключение между ними",
        usage: "/sessions",
        kind: SlashKind::Local,
    },
    SlashCmd {
        name: "trace",
        aliases: &[],
        summary: "Просмотр трассы выполнения",
        usage: "/trace [id-сессии]",
        kind: SlashKind::Local,
    },
    SlashCmd {
        name: "theme",
        aliases: &[],
        summary: "Переключение цветовой темы (dark/light/mono)",
        usage: "/theme [dark|light|mono]",
        kind: SlashKind::Local,
    },
    SlashCmd {
        name: "quit",
        aliases: &["q", "exit"],
        summary: "Выход из TUI",
        usage: "/quit",
        kind: SlashKind::Immediate,
    },
    SlashCmd {
        name: "yolo",
        aliases: &[],
        summary: "Переключение режима авто-разрешений (yolo)",
        usage: "/yolo [on|off]",
        kind: SlashKind::Immediate,
    },
    SlashCmd {
        name: "hooks",
        aliases: &[],
        summary: "Список хуков жизненного цикла",
        usage: "/hooks",
        kind: SlashKind::Local,
    },
    SlashCmd {
        name: "mcp",
        aliases: &[],
        summary: "Управление MCP-серверами",
        usage: "/mcp [list|reconnect]",
        kind: SlashKind::Local,
    },
    SlashCmd {
        name: "peers",
        aliases: &["agents"],
        summary: "Внешние CLI-агенты (Claude Code, Kimi, CodeWhale, Hermes, OpenClaw): статус установки",
        usage: "/peers",
        kind: SlashKind::Local,
    },
];

/// Вектор встроенных slash-команд харнесса.
///
/// Возвращает копию статической таблицы — вызывающий может дополнить
/// её собственными командами (например, из плагинов) и проверить итог
/// через [`validate_commands`].
pub fn builtin_commands() -> Vec<SlashCmd> {
    BUILTIN.to_vec()
}

/// Результат разбора строки ввода.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed<'a> {
    /// Распознанная команда: ссылка на описание в реестре и хвост строки
    /// после имени команды (аргументы, обрезанные по краям; может быть пустым).
    Cmd { cmd: &'static SlashCmd, args: &'a str },
    /// Ввод не является slash-командой: обычный текст, пустая строка
    /// или одинокий слеш без имени (`/`, `/ `).
    NotSlash,
    /// Слеш есть, но такой команды нет. `suggestions` — до трёх имён
    /// встроенных команд, похожих на введённое (префикс или Левенштейн).
    Unknown { name: String, suggestions: Vec<String> },
}

/// Разобрать строку ввода пользователя.
///
/// Правила:
///
/// - ввод обрезается по краям; не начинается с `/` — [`Parsed::NotSlash`];
/// - имя команды — первое слово после слеша, сравнение регистронезависимое,
///   учитываются алиасы;
/// - всё после первого пробельного разделителя — аргументы (обрезанные);
/// - неизвестное имя — [`Parsed::Unknown`] со списком подсказок.
///
/// Пример (текстом, чтобы не плодить doc-тесты):
///
/// ```text
/// parse("/model qwen3.5 --temp 0.2")
/// // → Parsed::Cmd { cmd: /model, args: "qwen3.5 --temp 0.2" }
/// ```
pub fn parse(input: &str) -> Parsed<'_> {
    let trimmed = input.trim();
    let Some(body) = trimmed.strip_prefix('/') else {
        return Parsed::NotSlash;
    };
    let (head, args) = match body.find(char::is_whitespace) {
        Some(idx) => (&body[..idx], body[idx..].trim()),
        None => (body, ""),
    };
    if head.is_empty() {
        return Parsed::NotSlash;
    }
    if let Some(cmd) = BUILTIN.iter().find(|cmd| cmd.matches(head)) {
        return Parsed::Cmd { cmd, args };
    }
    Parsed::Unknown { name: head.to_string(), suggestions: suggest(head) }
}

/// Подсказки для неизвестной команды: имена встроенных команд, у которых
/// имя или алиас либо пересекается по префиксу с введённым словом, либо
/// находится от него на расстоянии Левенштейна не больше порога и при этом
/// строго меньше длины большей из строк (иначе односимвольный ввод вроде
/// «/s» «угадывал» бы все односимвольные алиасы — это шум, а не подсказка).
/// Сортировка: по близости, затем по алфавиту; не более трёх штук.
fn suggest(input: &str) -> Vec<String> {
    let input = input.to_lowercase();
    let input_len = input.chars().count();
    let mut scored: Vec<(usize, &'static str)> = BUILTIN
        .iter()
        .filter_map(|cmd| {
            let best = std::iter::once(cmd.name)
                .chain(cmd.aliases.iter().copied())
                .filter_map(|cand| {
                    if cand.starts_with(input.as_str()) || input.starts_with(cand) {
                        Some(0)
                    } else {
                        let dist = levenshtein(&input, cand);
                        (dist <= MAX_SUGGESTION_DISTANCE && dist < input_len.max(cand.chars().count()))
                            .then_some(dist)
                    }
                })
                .min()
                .unwrap_or(usize::MAX);
            (best <= MAX_SUGGESTION_DISTANCE).then_some((best, cmd.name))
        })
        .collect();
    scored.sort_unstable();
    scored.truncate(MAX_SUGGESTIONS);
    scored.into_iter().map(|(_, cmd_name)| cmd_name.to_string()).collect()
}

/// Расстояние Левенштейна между двумя строками (по Unicode-символам).
/// Классическая динамика с двумя строками таблицы, O(n·m) по времени
/// и O(m) по памяти — для коротких имён команд этого достаточно.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Страница справки по одной команде: имя, сводка, usage, алиасы, вид.
///
/// Формат (на примере `/help`):
///
/// ```text
/// /help — Справка по командам TUI
/// Использование: /help [команда]
/// Алиасы: /h, /?
/// Тип: локальная
/// ```
pub fn help_page(cmd: &SlashCmd) -> String {
    let aliases = if cmd.aliases.is_empty() {
        "нет".to_string()
    } else {
        cmd.aliases.iter().map(|alias| format!("/{alias}")).collect::<Vec<_>>().join(", ")
    };
    format!(
        "/{name} — {summary}\nИспользование: {usage}\nАлиасы: {aliases}\nТип: {kind}",
        name = cmd.name,
        summary = cmd.summary,
        usage = cmd.usage,
        kind = cmd.kind.label(),
    )
}

/// Общая справка: выровненная таблица всех встроенных команд.
///
/// Колонки: имя, вид, сводка. Последняя строка — подсказка про
/// `/help <команда>`.
pub fn help_index() -> String {
    let cmds = builtin_commands();
    let width = cmds.iter().map(|cmd| cmd.name.len()).max().unwrap_or(0);
    let mut out = format!("Доступные команды ({}):\n", cmds.len());
    for cmd in &cmds {
        out.push_str(&format!(
            "  /{:<width$}  {:<15}  {}\n",
            cmd.name,
            cmd.kind.label(),
            cmd.summary,
        ));
    }
    out.push_str("Подробнее о команде: /help <команда>");
    out
}

/// Проверить реестр команд на корректность.
///
/// Ошибки (каждая — отдельный элемент вектора):
///
/// - пустое имя или имя не из строчных `a-z`, цифр и `-`;
/// - дублирующееся имя команды;
/// - пустой алиас;
/// - алиас, пересекающийся с именем любой команды (включая своё);
/// - дублирующийся алиас.
///
/// Возвращает `Ok(())`, если ошибок нет, иначе `Err` со всеми найденными
/// проблемами (проверка не останавливается на первой).
pub fn validate_commands(cmds: &[SlashCmd]) -> Result<(), Vec<String>> {
    let mut errors: Vec<String> = Vec::new();
    let mut names: HashSet<&str> = HashSet::new();
    for cmd in cmds {
        if cmd.name.is_empty() {
            errors.push("пустое имя команды".to_string());
        } else if !cmd.name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
            errors.push(format!(
                "имя команды /{} должно состоять из строчных a-z, цифр и '-'",
                cmd.name
            ));
        }
        if !names.insert(cmd.name) {
            errors.push(format!("дублирующееся имя команды: /{}", cmd.name));
        }
    }
    let mut aliases: HashSet<&str> = HashSet::new();
    for cmd in cmds {
        for &alias in cmd.aliases {
            if alias.is_empty() {
                errors.push(format!("пустой алиас у команды /{}", cmd.name));
                continue;
            }
            if names.contains(alias) {
                errors.push(format!(
                    "алиас /{alias} команды /{} пересекается с именем команды",
                    cmd.name
                ));
            }
            if !aliases.insert(alias) {
                errors.push(format!("дублирующийся алиас: /{alias}"));
            }
        }
    }
    if errors.is_empty() { Ok(()) } else { Err(errors) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Короткий хелпер: команда по имени из встроенного реестра.
    fn builtin(name: &str) -> SlashCmd {
        builtin_commands()
            .into_iter()
            .find(|cmd| cmd.name == name)
            .expect("команда должна быть в реестре")
    }

    #[test]
    fn parse_exact_name() {
        match parse("/help") {
            Parsed::Cmd { cmd, args } => {
                assert_eq!(cmd.name, "help");
                assert_eq!(args, "");
            }
            other => panic!("ожидалась команда, получено: {other:?}"),
        }
    }

    #[test]
    fn parse_aliases() {
        for (input, expected) in [("/h", "help"), ("/?", "help"), ("/exit", "quit"), ("/q", "quit"), ("/m", "model"), ("/ls", "sessions"), ("/mem", "memory"), ("/health", "doctor")] {
            match parse(input) {
                Parsed::Cmd { cmd, .. } => assert_eq!(cmd.name, expected, "ввод: {input}"),
                other => panic!("ввод {input}: ожидался алиас {expected}, получено: {other:?}"),
            }
        }
    }

    #[test]
    fn parse_command_with_args() {
        match parse("/model qwen3.5 --temp 0.2") {
            Parsed::Cmd { cmd, args } => {
                assert_eq!(cmd.name, "model");
                assert_eq!(args, "qwen3.5 --temp 0.2");
            }
            other => panic!("ожидалась команда, получено: {other:?}"),
        }
    }

    #[test]
    fn args_are_trimmed_but_keep_inner_spaces() {
        match parse("/compact   сократи  историю   ") {
            Parsed::Cmd { cmd, args } => {
                assert_eq!(cmd.name, "compact");
                assert_eq!(args, "сократи  историю");
            }
            other => panic!("ожидалась команда, получено: {other:?}"),
        }
    }

    #[test]
    fn not_slash_inputs() {
        for input in ["", "   ", "просто текст", "/", "/  ", "/\t"] {
            assert!(matches!(parse(input), Parsed::NotSlash), "ввод: {input:?}");
        }
    }

    #[test]
    fn unknown_with_levenshtein_suggestion() {
        match parse("/hlep") {
            Parsed::Unknown { name, suggestions } => {
                assert_eq!(name, "hlep");
                assert!(suggestions.contains(&"help".to_string()), "подсказки: {suggestions:?}");
            }
            other => panic!("ожидался Unknown, получено: {other:?}"),
        }
    }

    #[test]
    fn unknown_with_prefix_suggestion() {
        match parse("/tra") {
            Parsed::Unknown { suggestions, .. } => assert_eq!(suggestions, vec!["trace".to_string()]),
            other => panic!("ожидался Unknown, получено: {other:?}"),
        }
    }

    #[test]
    fn suggestions_sorted_by_distance_then_alphabet() {
        // «/s» — префикс у sessions и skills; обе на расстоянии 0,
        // порядок — алфавитный.
        match parse("/s") {
            Parsed::Unknown { suggestions, .. } => {
                assert_eq!(suggestions, vec!["sessions".to_string(), "skills".to_string()]);
            }
            other => panic!("ожидался Unknown, получено: {other:?}"),
        }
    }

    #[test]
    fn unknown_matches_alias_prefix_too() {
        // «heal» — префикс алиаса «health» команды doctor.
        match parse("/heal") {
            Parsed::Unknown { suggestions, .. } => {
                assert!(suggestions.contains(&"doctor".to_string()), "подсказки: {suggestions:?}");
            }
            other => panic!("ожидался Unknown, получено: {other:?}"),
        }
    }

    #[test]
    fn unknown_without_suggestions() {
        match parse("/xyzzy42") {
            Parsed::Unknown { name, suggestions } => {
                assert_eq!(name, "xyzzy42");
                assert!(suggestions.is_empty(), "подсказки: {suggestions:?}");
            }
            other => panic!("ожидался Unknown, получено: {other:?}"),
        }
    }

    #[test]
    fn suggestions_capped_at_three() {
        // «/m» — точный алиас model, поэтому берём «/mo»: префикс у model,
        // а mcp и memory — на расстоянии <= 2.
        match parse("/mo") {
            Parsed::Unknown { suggestions, .. } => {
                assert!(suggestions.len() <= MAX_SUGGESTIONS, "подсказки: {suggestions:?}");
                assert!(suggestions.contains(&"model".to_string()), "подсказки: {suggestions:?}");
            }
            other => panic!("ожидался Unknown, получено: {other:?}"),
        }
    }

    #[test]
    fn parse_is_case_insensitive() {
        for (input, expected) in [("/HELP", "help"), ("/Doctor", "doctor"), ("/QuIt", "quit"), ("/Yolo", "yolo")] {
            match parse(input) {
                Parsed::Cmd { cmd, .. } => assert_eq!(cmd.name, expected, "ввод: {input}"),
                other => panic!("ввод {input}: ожидалась {expected}, получено: {other:?}"),
            }
        }
        // Алиасы тоже регистронезависимы, аргументы не трогаем.
        match parse("/MoDeL X-Large") {
            Parsed::Cmd { cmd, args } => {
                assert_eq!(cmd.name, "model");
                assert_eq!(args, "X-Large");
            }
            other => panic!("ожидалась команда, получено: {other:?}"),
        }
    }

    #[test]
    fn unicode_args_are_kept_intact() {
        match parse("/goal построить 🚀 ракету") {
            Parsed::Cmd { cmd, args } => {
                assert_eq!(cmd.name, "goal");
                assert_eq!(args, "построить 🚀 ракету");
            }
            other => panic!("ожидалась команда, получено: {other:?}"),
        }
    }

    #[test]
    fn help_page_contains_usage_aliases_and_kind() {
        let page = help_page(&builtin("help"));
        assert!(page.contains("/help — Справка по командам TUI"), "страница:\n{page}");
        assert!(page.contains("Использование: /help [команда]"), "страница:\n{page}");
        assert!(page.contains("Алиасы: /h, /?"), "страница:\n{page}");
        assert!(page.contains("Тип: локальная"), "страница:\n{page}");
    }

    #[test]
    fn help_page_marks_missing_aliases() {
        let page = help_page(&builtin("compact"));
        assert!(page.contains("Алиасы: нет"), "страница:\n{page}");
        assert!(page.contains("Тип: требует агента"), "страница:\n{page}");
    }

    #[test]
    fn help_index_lists_all_commands() {
        let index = help_index();
        let cmds = builtin_commands();
        assert!(index.contains("Доступные команды (19):"), "индекс:\n{index}");
        for cmd in &cmds {
            assert!(index.contains(&format!("/{}", cmd.name)), "нет /{} в индексе:\n{index}", cmd.name);
        }
        let rows = index.lines().filter(|line| line.starts_with("  /")).count();
        assert_eq!(rows, cmds.len());
        assert!(index.contains("Подробнее о команде: /help <команда>"));
    }

    #[test]
    fn builtin_registry_is_valid() {
        assert_eq!(validate_commands(&builtin_commands()), Ok(()));
    }

    #[test]
    fn validate_detects_duplicate_names() {
        let mut cmds = builtin_commands();
        let dup = cmds[0];
        cmds.push(dup);
        let errors = validate_commands(&cmds).expect_err("дубли имён должны ловиться");
        assert!(errors.iter().any(|e| e.contains("дублирующееся имя команды: /help")), "ошибки: {errors:?}");
    }

    #[test]
    fn validate_detects_alias_vs_name_conflict() {
        let mut cmds = builtin_commands();
        let mut rogue = builtin("hooks");
        rogue.name = "roguecmd";
        rogue.aliases = &["trace"]; // алиас совпадает с именем другой команды
        cmds.push(rogue);
        let errors = validate_commands(&cmds).expect_err("конфликт алиаса и имени должен ловиться");
        assert!(errors.iter().any(|e| e.contains("пересекается с именем команды")), "ошибки: {errors:?}");
    }

    #[test]
    fn validate_detects_duplicate_aliases() {
        let mut cmds = builtin_commands();
        let mut rogue = builtin("mcp");
        rogue.name = "roguecmd";
        rogue.aliases = &["q"]; // уже алиас /quit
        cmds.push(rogue);
        let errors = validate_commands(&cmds).expect_err("дубли алиасов должны ловиться");
        assert!(errors.iter().any(|e| e.contains("дублирующийся алиас: /q")), "ошибки: {errors:?}");
    }

    #[test]
    fn validate_rejects_uppercase_names() {
        let mut rogue = builtin("mcp");
        rogue.name = "MCP";
        let errors = validate_commands(&[rogue]).expect_err("верхний регистр должен ловиться");
        assert!(errors.iter().any(|e| e.contains("строчных a-z")), "ошибки: {errors:?}");
    }

    #[test]
    fn kinds_are_assigned_by_semantics() {
        assert_eq!(builtin("help").kind, SlashKind::Local);
        assert_eq!(builtin("doctor").kind, SlashKind::Local);
        assert_eq!(builtin("sessions").kind, SlashKind::Local);
        assert_eq!(builtin("compact").kind, SlashKind::NeedsAgent);
        assert_eq!(builtin("goal").kind, SlashKind::NeedsAgent);
        assert_eq!(builtin("plan").kind, SlashKind::NeedsAgent);
        assert_eq!(builtin("quit").kind, SlashKind::Immediate);
        assert_eq!(builtin("yolo").kind, SlashKind::Immediate);
    }

    #[test]
    fn builtin_registry_shape() {
        let cmds = builtin_commands();
        assert_eq!(cmds.len(), 19);
        for cmd in &cmds {
            assert!(!cmd.name.is_empty());
            assert!(cmd.usage.starts_with(&format!("/{}", cmd.name)), "usage {} не начинается с имени", cmd.name);
            assert!(!cmd.summary.is_empty());
            assert!(!cmd.kind.label().is_empty());
        }
    }

    #[test]
    fn levenshtein_basics() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", ""), 3);
        assert_eq!(levenshtein("abc", "abc"), 0);
        assert_eq!(levenshtein("abc", "abd"), 1);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        // Счёт идёт по символам, а не по байтам.
        assert_eq!(levenshtein("модель", "модели"), 1);
    }
}
