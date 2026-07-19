//! Слоистые разрешения (урок обзора: правила → эвристики → вопрос пользователю).
//! Слои: hard-deny паттерны → auto-allow (read-only + белый список bash) → режим (ask/yolo/dontAsk).
//! v0.4: bash-решения переведены на `crate::execpolicy` — каноникализация
//! составных команд (кавычки/пайпы) + PolicyEngine (deny-правила конфига как
//! Regex-правила + классификация подкоманд). execpolicy — движок классификации,
//! а не источник новых решений: семантика режимов и тексты сообщений прежние.

use crate::config::PermissionConfig;
use crate::execpolicy;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// спрашивать пользователя при не-авто решениях (с подтверждением)
    Ask,
    /// полуавтомат: read-only и правки файлов в workspace — авто;
    /// bash с побочными эффектами, peer-агенты, деструктив — с подтверждением
    SemiAuto,
    /// разрешать всё, кроме hard-deny (автомат)
    Yolo,
    /// авто-запрет всего не-авто (для CI/headless без yolo)
    DontAsk,
}

impl Mode {
    /// Русская метка для статус-строки (названия в стиле codex: suggest/auto-edit/full-auto).
    pub fn label(self) -> &'static str {
        match self {
            Mode::Ask => "Совет",
            Mode::SemiAuto => "Авто-правки",
            Mode::Yolo => "Автомат",
            Mode::DontAsk => "headless",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask(String),  // причина/описание для попапа
    Deny(String), // причина запрета
}

/// Префиксы команд, считающихся безопасными read-only (урок Grok: tee туда не входит)
const SAFE_BASH_PREFIXES: &[&str] = &[
    "ls", "pwd", "cat", "head", "tail", "wc", "sort", "uniq", "find", "rg", "grep",
    "git status", "git diff", "git log", "git show", "git branch", "git rev-parse",
    "python3 --version", "python --version", "pip list", "pip show",
    "cargo check", "cargo --version", "rustc --version",
    "echo", "true", "date", "uname", "df", "du", "free", "stat", "file", "which", "env",
    "python3 -c", "python -c",
];

pub struct PermissionEngine {
    mode: Mode,
    /// общий оверрайд режима из Controls (атомик — переключение в рантайме
    /// из TUI-команды /mode, действует и посреди хода агента)
    mode_override: Option<std::sync::Arc<std::sync::atomic::AtomicU8>>,
    cfg: PermissionConfig,
    workspace: PathBuf,
    /// разрешённые навсегда за сессию (ответ «всегда»)
    always_allowed: Vec<String>,
    deny_set: regex::RegexSet,
    deny_res: Vec<regex::Regex>,
    rules: Vec<crate::config::PermissionRule>,
    /// v0.4: PolicyEngine из execpolicy — deny-правила конфига (Regex) + белый
    /// список префиксов (Allow-правила); каноникализация + classify внутри decide
    policy: execpolicy::PolicyEngine,
}

/// Коды режимов в общем атомике Controls.mode_atomic.
pub const MODE_ASK: u8 = 0;
pub const MODE_SEMI: u8 = 1;
pub const MODE_YOLO: u8 = 2;
/// THS-QA-01: headless-режим DontAsk (в /mode не предлагается — только запуск
/// с `-p` без `--yolo`); раньше в атомик не мапился и был недостижим из CLI.
pub const MODE_DONTASK: u8 = 3;
/// «Не задано» — используется режим из конструктора.
pub const MODE_UNSET: u8 = 255;

impl PermissionEngine {
    pub fn new(mode: Mode, cfg: PermissionConfig, workspace: &Path) -> Self {
        let deny_set = regex::RegexSet::new(&cfg.bash_deny_patterns)
            .unwrap_or_else(|_| regex::RegexSet::new::<_, &&str>(&[]).unwrap());
        let deny_res = cfg.bash_deny_patterns.iter()
            .filter_map(|p| regex::Regex::new(p).ok()).collect();
        let policy = execpolicy::PolicyEngine::new(policy_rules(&cfg));
        PermissionEngine { mode, mode_override: None, cfg, workspace: workspace.to_path_buf(),
                           always_allowed: vec![], deny_set, deny_res, rules: vec![], policy }
    }

    pub fn with_rules(mut self, rules: Vec<crate::config::PermissionRule>) -> Self {
        self.rules = rules;
        self
    }

    /// Подключить общий атомик режима (переключение /mode в рантайме).
    pub fn with_mode_override(mut self, atomic: std::sync::Arc<std::sync::atomic::AtomicU8>) -> Self {
        self.mode_override = Some(atomic);
        self
    }

    /// Эффективный режим: оверрайд из Controls, если задан, иначе базовый.
    pub fn mode(&self) -> Mode {
        match &self.mode_override {
            Some(a) => match a.load(std::sync::atomic::Ordering::Relaxed) {
                MODE_SEMI => Mode::SemiAuto,
                MODE_YOLO => Mode::Yolo,
                MODE_DONTASK => Mode::DontAsk,
                MODE_ASK => Mode::Ask,
                _ => self.mode,
            },
            None => self.mode,
        }
    }

    pub fn grant_always(&mut self, key: String) {
        self.always_allowed.push(key);
    }

    fn in_workspace(&self, p: &Path) -> bool {
        let abs = if p.is_absolute() { p.to_path_buf() } else { self.workspace.join(p) };
        // каноникализация может упасть для несуществующего файла — нормализуем вручную
        let norm = normalize(&abs);
        norm.starts_with(&self.workspace)
    }

    fn protected(&self, p: &Path) -> bool {
        let abs = if p.is_absolute() { normalize(p) } else { normalize(&self.workspace.join(p)) };
        abs.components().any(|c| c.as_os_str() == ".git")
    }

    /// Решение для файловых инструментов
    pub fn file_write(&self, path: &str) -> Decision {
        let p = Path::new(path);
        if self.protected(p) {
            return Decision::Deny(format!("запись в защищённый путь (.git): {path}"));
        }
        if self.in_workspace(p) {
            Decision::Allow
        } else {
            Decision::Deny(format!("запись вне рабочего дерева: {path}"))
        }
    }

    /// Пользовательские правила из конфига (v0.3, урок Claude Code):
    /// срабатывают ДО режима; формат "Bash(prefix)" | "Read(prefix)" | "Write(prefix)" | "Tool".
    /// THS-QA-02: для bash префикс матчится не только по сырой строке, но и по
    /// подкомандам `execpolicy::canonicalize_command` (и их формам без обёрток
    /// `env`/`nice`/`sudo`/...) — иначе `cd /tmp && rm file` или `env rm x`
    /// обходят deny-правило Bash(rm). Решение — worst-of по всем сматчившимся
    /// правилам (deny > ask > allow), см. `worst`.
    pub fn rule_decision(&self, tool: &str, target: &str) -> Option<Decision> {
        let mut out: Option<Decision> = None;
        for r in &self.rules {
            let (rtool, prefix) = parse_rule_pattern(&r.pattern);
            let tool_match = rtool == "*" || rtool.eq_ignore_ascii_case(tool);
            let prefix_match = prefix.is_empty() || rule_target_matches(tool, target, &prefix);
            if tool_match && prefix_match {
                let d = match r.decision.as_str() {
                    "allow" => Decision::Allow,
                    "deny" => Decision::Deny(if r.reason.is_empty() {
                        format!("deny-правило `{}`", r.pattern)
                    } else {
                        r.reason.clone()
                    }),
                    _ => Decision::Ask(if r.reason.is_empty() {
                        format!("ask-правило `{}`", r.pattern)
                    } else {
                        r.reason.clone()
                    }),
                };
                // deny строже ask, ask строже allow — оставляем самое строгое
                out = Some(worst(out.unwrap_or(Decision::Allow), d));
            }
        }
        out
    }

    pub fn file_read(&self, path: &str) -> Decision {
        let p = Path::new(path);
        if self.in_workspace(p) || self.mode() == Mode::Yolo {
            Decision::Allow
        } else {
            Decision::Ask(format!("чтение вне рабочего дерева: {path}"))
        }
    }

    /// Команда из белого списка read-only (для plan-режима, v0.3)
    pub fn is_readonly_bash(&self, cmd: &str) -> bool {
        let cmd = cmd.trim();
        if cmd.contains('$') || cmd.contains('`') { return false; }
        SAFE_BASH_PREFIXES.iter().any(|p| cmd == *p || cmd.starts_with(&format!("{p} ")))
    }

    /// Решение для bash: слои hard-deny → always → белый список → режим.
    /// v0.2: сплит простых compound-цепочек (урок Codex) — решение по самой строгой части.
    /// v0.4: поверх — каноникализация + PolicyEngine из execpolicy: решение по
    /// подкомандам с учётом кавычек/пайпов; итог — худшее из двух проходов.
    pub fn bash(&self, command: &str) -> Decision {
        let cmd = command.trim();
        let mut decision = Decision::Allow;
        // legacy-сплит v0.2 (оставлен для совместимости поведения и тестов)
        for part in &split_simple_chain(cmd) {
            let d = self.bash_single(part);
            decision = worst(decision, d);
            if matches!(decision, Decision::Deny(_)) { return decision; }
        }
        worst(decision, self.bash_via_policy(cmd))
    }

    /// v0.4: перекрёстное решение через execpolicy. `canonicalize_command`
    /// корректно разбирает кавычки/пайпы (внутри `PolicyEngine::decide`),
    /// PolicyEngine применяет deny-правила конфига и класс подкоманды (`classify`).
    /// Таблица «режим × класс» в execpolicy совпадает с семантикой режимов здесь
    /// (см. слой 4), а белый список подан в движок Allow-правилами, поэтому
    /// execpolicy — движок классификации, а не источник новых решений: проход
    /// может лишь ужесточить обходы legacy-сплита (напр. `ls && make > x`).
    fn bash_via_policy(&self, cmd: &str) -> Decision {
        if cmd.is_empty() { return Decision::Allow; }
        let (d, _reasons) = self.policy.decide(cmd, ep_mode(self.mode()));
        match d {
            execpolicy::Decision::Allow => Decision::Allow,
            // deny-правило бьёт allow даже в yolo; текст отказа — legacy-формат
            execpolicy::Decision::Deny => match self.deny_matched_text(cmd) {
                Some(text) => self.hard_deny(&text),
                None => Decision::Deny(format!("не в белом списке (dontAsk): {cmd}")),
            },
            execpolicy::Decision::Ask => Decision::Ask(format!("выполнить команду: {cmd}")),
        }
    }

    /// Текст, сматчивший hard-deny: целая команда либо каноническая подкоманда
    /// (нужно для deny-правил с якорем `^`, видимых только на уровне подкоманды).
    fn deny_matched_text(&self, cmd: &str) -> Option<String> {
        if self.deny_set.is_match(cmd) {
            return Some(cmd.to_string());
        }
        execpolicy::canonicalize_command(cmd).into_iter()
            .find(|s| self.deny_set.is_match(s))
    }

    /// Слой 0: hard-deny — сообщение строго в legacy-формате (его проверяют тесты).
    fn hard_deny(&self, cmd: &str) -> Decision {
        // deny_set построен успешно ⇒ все паттерны скомпилированы ⇒ deny_res полон
        // и в том же порядке, что cfg.bash_deny_patterns.
        // Защита от теоретической паники (V3 #2.1): при битом пользовательском
        // regex deny_set может быть собран из 0 паттернов, а deny_res — пуст;
        // is_match тогда false и сюда не дойдём, но индекс страхуем всё равно.
        let idx = self.deny_set.matches(cmd).into_iter().next().unwrap_or(0);
        let pat = self.deny_res.get(idx).map_or("<?>", regex::Regex::as_str);
        Decision::Deny(format!("hard-deny паттерн `{pat}`: {cmd}"))
    }

    fn bash_single(&self, command: &str) -> Decision {
        let cmd = command.trim();
        if cmd.is_empty() { return Decision::Allow; }
        // слой 0: hard-deny всегда
        if self.deny_set.is_match(cmd) {
            return self.hard_deny(cmd);
        }
        // слой 1: «разрешено навсегда» за сессию
        let key = bash_key(cmd);
        if self.always_allowed.iter().any(|k| k == &key) {
            return Decision::Allow;
        }
        // слой 2: белый список read-only префиксов
        if SAFE_BASH_PREFIXES.iter().any(|p| cmd == *p || cmd.starts_with(&format!("{p} "))) {
            // подстановка $()/backtick может прятать произвольный код (`echo $(rm -rf ~)`):
            // белый список применяем только к «чистым» командам (урок Codex)
            if cmd.contains('$') || cmd.contains('`') {
                return match self.mode() {
                    Mode::Yolo => Decision::Allow,
                    Mode::DontAsk => Decision::Deny(format!("подстановка в команде (dontAsk): {cmd}")),
                    Mode::Ask | Mode::SemiAuto => Decision::Ask(format!("команда с подстановкой: {cmd}")),
                };
            }
            // v0.2: read-confinement — read-only команды не должны читать абсолютные пути
            // вне workspace без спроса (урок Grok shell_access; находка T5b: чтение /sys)
            if self.mode() != Mode::Yolo {
                if let Some(p) = absolute_arg_outside(cmd, &self.workspace) {
                    return Decision::Ask(format!("чтение вне рабочего дерева: {p}"));
                }
            }
            return Decision::Allow;
        }
        if self.cfg.bash_allow_prefixes.iter().any(|p| cmd.starts_with(p.as_str())) {
            return Decision::Allow;
        }
        // слой 3: запись вне рабочего дерева через редирект — анти-байпасс (урок Grok shell_access)
        if let Some(target) = redirect_target(cmd) {
            if !self.in_workspace(Path::new(&target)) && self.mode() != Mode::Yolo {
                return Decision::Ask(format!("запись редиректом вне рабочего дерева: {target}"));
            }
        }
        // слой 4: режим
        match self.mode() {
            Mode::Yolo => Decision::Allow,
            Mode::DontAsk => Decision::Deny(format!("не в белом списке (dontAsk): {cmd}")),
            Mode::Ask | Mode::SemiAuto => Decision::Ask(format!("выполнить команду: {cmd}")),
        }
    }
}

/// Более строгое из двух решений (Deny > Ask > Allow)
fn worst(a: Decision, b: Decision) -> Decision {
    fn rank(d: &Decision) -> u8 {
        match d { Decision::Deny(_) => 2, Decision::Ask(_) => 1, Decision::Allow => 0 }
    }
    if rank(&b) > rank(&a) { b } else { a }
}

/// Режим permissions → зеркальный режим execpolicy
const fn ep_mode(mode: Mode) -> execpolicy::Mode {
    match mode {
        Mode::Ask | Mode::SemiAuto => execpolicy::Mode::Ask,
        Mode::DontAsk => execpolicy::Mode::DontAsk,
        Mode::Yolo => execpolicy::Mode::Yolo,
    }
}

/// Правила для PolicyEngine (v0.4):
/// - `cfg.bash_deny_patterns` → Deny-правила (Regex) — проверяются до эвристики
///   классов и бьют allow даже в yolo, как слой 0 в `bash_single`;
/// - белый список префиксов (SAFE + `cfg.bash_allow_prefixes`) → Allow-правила
///   с якорем `^prefix(\s|$)` — та же семантика «префикс + пробел/конец», что
///   у слоя 2; так PolicyEngine учитывает whitelist permissions.rs и не выносит
///   по whitelisted-командам («python3 -c …», «pip list») решений строже текущих.
///
/// Некомпилирующиеся паттерны молча пропускаются, как в `PermissionEngine::new`.
fn policy_rules(cfg: &PermissionConfig) -> Vec<execpolicy::Rule> {
    let mut rules = Vec::new();
    for p in &cfg.bash_deny_patterns {
        if let Ok(r) = execpolicy::Rule::compile(p, execpolicy::Decision::Deny, "") {
            rules.push(r);
        }
    }
    let whitelist = SAFE_BASH_PREFIXES.iter().copied()
        .chain(cfg.bash_allow_prefixes.iter().map(String::as_str));
    for prefix in whitelist {
        let pat = format!("^{}(\\s|$)", regex::escape(prefix));
        if let Ok(r) = execpolicy::Rule::compile(&pat, execpolicy::Decision::Allow, "") {
            rules.push(r);
        }
    }
    rules
}

/// Сплит «простой» цепочки по &&, ||, ;, | — только если нет подстановок/редиректов/глобов
/// (урок Codex: небезопасные конструкции НЕ сплитим, оцениваем как единый вызов).
///
/// NB (ревью 2.3/3): функция помечалась «legacy v0.2», но сознательно ОСТАВЛЕНА как
/// первый проход `PermissionEngine::bash()`: итоговое решение — worst-of(legacy, execpolicy),
/// т.е. этот проход ловит обходы, которые execpolicy-каноникализатор пропускает
/// (и наоборот). Удаление возможно только после A/B-сравнения покрытия на корпусе команд.
fn split_simple_chain(cmd: &str) -> Vec<String> {
    const DANGEROUS: &[char] = &['$', '`', '*', '?', '>', '<', '(', ')', '{', '}', '[', ']'];
    if cmd.chars().any(|c| DANGEROUS.contains(&c)) {
        return vec![cmd.to_string()];
    }
    let mut parts = vec![];
    let mut cur = String::new();
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let two: String = chars.get(i..(i + 2).min(chars.len())).unwrap_or(&[]).iter().collect();
        if two == "&&" || two == "||" {
            parts.push(cur.trim().to_string());
            cur.clear();
            i += 2;
            continue;
        }
        if chars[i] == ';' || chars[i] == '|' {
            parts.push(cur.trim().to_string());
            cur.clear();
            i += 1;
            continue;
        }
        cur.push(chars[i]);
        i += 1;
    }
    parts.push(cur.trim().to_string());
    parts.into_iter().filter(|p| !p.is_empty()).collect()
}

/// Абсолютный путь-аргумент вне workspace (для read-confinement); игнорируем флаги и /dev/stdin
fn absolute_arg_outside(cmd: &str, workspace: &Path) -> Option<String> {
    for tok in cmd.split_whitespace().skip(1) {
        if tok.starts_with('/') && !tok.starts_with("/dev/") {
            let p = normalize(Path::new(tok));
            if !p.starts_with(workspace) {
                return Some(tok.to_string());
            }
        }
    }
    None
}

/// Ключ «серии» команды для «разрешить навсегда»
fn bash_key(cmd: &str) -> String {
    cmd.split_whitespace().take(2).collect::<Vec<_>>().join(" ")
}

/// Разбор паттерна правила: "Bash(prefix)" | "Read(prefix)" | "Write(prefix)" | "Tool"
fn parse_rule_pattern(pattern: &str) -> (String, String) {
    if let Some(rest) = pattern.strip_suffix(')') {
        if let Some(pos) = rest.find('(') {
            return (rest[..pos].to_string(), rest[pos + 1..].to_string());
        }
    }
    (pattern.to_string(), String::new())
}

/// Матч префикса правила по цели (THS-QA-02). Для bash: сырая строка, каждая
/// подкоманда каноникализации и её форма без ведущих обёрток (`env rm x` →
/// `rm x`). Для прочих инструментов — прежняя семантика: префикс сырой строки.
fn rule_target_matches(tool: &str, target: &str, prefix: &str) -> bool {
    if !tool.eq_ignore_ascii_case("bash") {
        return target.starts_with(prefix);
    }
    if target.starts_with(prefix) {
        return true;
    }
    execpolicy::canonicalize_command(target).iter().any(|sub| {
        sub.starts_with(prefix)
            || unwrapped_subcommand(sub).is_some_and(|s| s.starts_with(prefix))
    })
}

/// Подкоманда без ведущих обёрток (`env`, `nice`, `sudo`, `timeout`, ...) и
/// префиксных присваиваний `VAR=value`. `None` — обёрток нет (матчить нечего)
/// или после разворачивания не осталось слов.
fn unwrapped_subcommand(sub: &str) -> Option<String> {
    let words = execpolicy::split_words(sub);
    let start = unwrapped_start(&words);
    if start > 0 && start < words.len() {
        Some(words[start..].join(" "))
    } else {
        None
    }
}

/// Индекс первого слова настоящей программы в подкоманде: пропускаем
/// присваивания `VAR=value` и обёртки с их флагами. Компактное зеркало
/// разворачивания из execpolicy (там оно приватно, в `classify_by_words`) —
/// нужно, чтобы deny-правило Bash(rm) ловило `env rm x`, `nice rm x`,
/// `sudo -u root rm x` (THS-QA-02).
fn unwrapped_start(words: &[String]) -> usize {
    let mut i = 0;
    while let Some(w) = words.get(i) {
        if is_env_assignment(w) {
            i += 1;
            continue;
        }
        let prog = w.rsplit('/').next().unwrap_or(w);
        i = match prog {
            "command" | "builtin" | "nohup" | "exec" => i + 1,
            "env" => skip_wrapper_args(words, i + 1,
                &["-u", "-C", "-S", "--unset", "--chdir", "--split-string"]),
            "sudo" => skip_wrapper_args(words, i + 1,
                &["-u", "-g", "-h", "-p", "-C", "-T", "--user", "--group", "--host", "--prompt"]),
            "nice" => skip_wrapper_args(words, i + 1, &["-n", "--adjustment"]),
            "stdbuf" => skip_wrapper_args(words, i + 1,
                &["-i", "-o", "-e", "--input", "--output", "--error"]),
            // после флагов timeout идёт ДЛИТЕЛЬНОСТЬ, и только потом команда
            "timeout" => skip_wrapper_args(words, i + 1, &["-k", "-s", "--kill-after", "--signal"]) + 1,
            _ => break,
        };
    }
    i
}

/// Пропустить флаги обёртки; флаги из `value_flags` забирают и следующее слово.
/// Присваивания `VAR=value` тоже пропускаются (для `env A=1 cmd`).
fn skip_wrapper_args(words: &[String], mut i: usize, value_flags: &[&str]) -> usize {
    while let Some(w) = words.get(i) {
        if value_flags.contains(&w.as_str()) {
            i += 2;
        } else if w.starts_with('-') || is_env_assignment(w) {
            i += 1;
        } else {
            break;
        }
    }
    i
}

/// Похоже ли слово на префиксное присваивание `VAR=value`.
fn is_env_assignment(w: &str) -> bool {
    let Some(eq) = w.find('=') else { return false };
    let key = &w[..eq];
    !key.is_empty()
        && !key.starts_with(|c: char| c.is_ascii_digit())
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Грубый поиск цели редиректа `>` или `>>` (последнее вхождение)
fn redirect_target(cmd: &str) -> Option<String> {
    if let Some(pos) = cmd.rfind('>') {
        let rest = &cmd[pos + 1..];
        let rest = rest.trim_start_matches('>').trim();
        let tok = rest.split_whitespace().next().unwrap_or("");
        if !tok.is_empty() && !tok.starts_with('&') {
            return Some(tok.trim_matches(|c| c == '"' || c == '\'').to_string());
        }
    }
    None
}

/// Нормализация пути без fs (убрать ./ и ../)
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        use std::path::Component::*;
        match c {
            CurDir => {}
            ParentDir => { out.pop(); }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(mode: Mode) -> PermissionEngine {
        PermissionEngine::new(mode, PermissionConfig::default(), Path::new("/tmp/ws"))
    }

    #[test]
    fn deny_rm_rf_root() {
        let e = engine(Mode::Yolo);
        assert!(matches!(e.bash("rm -rf /"), Decision::Deny(_)));
        assert!(matches!(e.bash("rm -rf /*"), Decision::Deny(_)));
        assert!(matches!(e.bash("rm -rf ~"), Decision::Deny(_)));
        assert!(matches!(e.bash("rm -rf $HOME"), Decision::Deny(_)));
    }

    #[test]
    fn safe_prefixes_auto_allowed() {
        let e = engine(Mode::DontAsk);
        assert!(matches!(e.bash("ls -la"), Decision::Allow));
        assert!(matches!(e.bash("git status"), Decision::Allow));
        assert!(matches!(e.bash("cargo check"), Decision::Allow));
        assert!(matches!(e.bash("python3 -c \"print(1)\""), Decision::Allow));
    }

    #[test]
    fn unknown_by_mode() {
        assert!(matches!(engine(Mode::Yolo).bash("make install"), Decision::Allow));
        assert!(matches!(engine(Mode::Ask).bash("make install"), Decision::Ask(_)));
        assert!(matches!(engine(Mode::DontAsk).bash("make install"), Decision::Deny(_)));
    }

    #[test]
    fn tee_not_safe() {
        assert!(matches!(engine(Mode::DontAsk).bash("tee /tmp/ws/x"), Decision::Deny(_)));
    }

    #[test]
    fn file_write_confined() {
        let e = engine(Mode::Ask);
        assert!(matches!(e.file_write("src/main.rs"), Decision::Allow));
        assert!(matches!(e.file_write("/etc/passwd"), Decision::Deny(_)));
        assert!(matches!(e.file_write("../escape.txt"), Decision::Deny(_)));
        assert!(matches!(e.file_write(".git/config"), Decision::Deny(_)));
    }

    #[test]
    fn redirect_outside_asks() {
        let e = engine(Mode::Ask);
        assert!(matches!(e.bash("make > /tmp/outside.txt"), Decision::Ask(_)));
        assert!(matches!(e.bash("make > /tmp/ws/inside.txt"), Decision::Ask(_))); // сама make — ask
    }

    #[test]
    fn hard_deny_real_regex() {
        let e = engine(Mode::Yolo);
        assert!(matches!(e.bash("mkfs.ext4 /dev/sda"), Decision::Deny(_)));
        assert!(matches!(e.bash("dd if=/dev/zero > /dev/sda"), Decision::Deny(_)));
        assert!(matches!(e.bash("cargo build"), Decision::Allow));
    }

    #[test]
    fn compound_split_and_worst_decision() {
        // простая цепочка сплитится; самая строгая часть решает
        let e = engine(Mode::Yolo);
        assert!(matches!(e.bash("ls && rm -rf /"), Decision::Deny(_)));
        assert!(matches!(e.bash("ls && git status"), Decision::Allow));
        // подстановка отменяет auto-allow даже у «безопасных» команд
        let e2 = engine(Mode::Ask);
        assert!(matches!(e2.bash("cat $(pwd)/x"), Decision::Ask(_)));
        // а hard-deny ловит запрещённое даже внутри подстановки (слои работают вместе)
        assert!(matches!(e2.bash("echo $(rm -rf ~)"), Decision::Deny(_)));
        let e3 = engine(Mode::DontAsk);
        assert!(matches!(e3.bash("echo $(id)"), Decision::Deny(_)));
    }

    #[test]
    fn read_confinement() {
        let e = engine(Mode::Ask);
        // cat вне workspace → Ask (хотя cat в белом списке)
        assert!(matches!(e.bash("cat /etc/passwd"), Decision::Ask(_)));
        // относительный путь внутри workspace → Allow
        assert!(matches!(e.bash("cat src/main.rs"), Decision::Allow));
        // yolo — без вопросов
        assert!(matches!(engine(Mode::Yolo).bash("cat /etc/passwd"), Decision::Allow));
    }

    // --- v0.4: интеграция execpolicy (каноникализация + PolicyEngine) ---

    #[test]
    fn canon_splits_quotes_and_pipes() {
        // кавычки защищают операторы, пайп остаётся разделителем (урок Codex)
        assert_eq!(
            execpolicy::canonicalize_command("echo 'a;b' && ls | grep x"),
            vec!["echo 'a;b'", "ls", "grep x"]
        );
        assert_eq!(
            execpolicy::canonicalize_command("echo \"x|y\" | cat"),
            vec!["echo \"x|y\"", "cat"]
        );
        // экранированный разделитель — не разделитель
        assert_eq!(
            execpolicy::canonicalize_command("echo a\\;b ; ls"),
            vec!["echo a\\;b", "ls"]
        );
    }

    #[test]
    fn canon_catches_compound_bypass() {
        // `>` отключал legacy-сплит: целая строка начиналась с «ls » и попадала
        // в белый список; каноникализация достаёт вторую подкоманду
        let e = engine(Mode::DontAsk);
        assert!(matches!(e.bash("ls && make > x"), Decision::Deny(_)));
        let e = engine(Mode::Ask);
        assert!(matches!(e.bash("ls && make > x"), Decision::Ask(_)));
    }

    #[test]
    fn git_reset_hard_is_destructive() {
        assert_eq!(
            execpolicy::classify("git reset --hard HEAD"),
            execpolicy::CmdClass::Destructive
        );
        // семантика режимов прежняя: Ask → вопрос, DontAsk → запрет, Yolo → разрешено
        assert!(matches!(engine(Mode::Ask).bash("git reset --hard HEAD"), Decision::Ask(_)));
        assert!(matches!(engine(Mode::DontAsk).bash("git reset --hard HEAD"), Decision::Deny(_)));
        assert!(matches!(engine(Mode::Yolo).bash("git reset --hard HEAD"), Decision::Allow));
    }

    #[test]
    fn fork_bomb_denied_via_policy() {
        // deny-правило конфига срабатывает через PolicyEngine даже в yolo
        let e = engine(Mode::Yolo);
        assert!(matches!(e.bash(":(){ :|:& };:"), Decision::Deny(_)));
    }

    #[test]
    fn deny_rule_beats_allow_prefix() {
        let cfg = PermissionConfig {
            bash_deny_patterns: vec![r"\bsecret\b".into()],
            bash_allow_prefixes: vec!["echo".into()],
        };
        let e = PermissionEngine::new(Mode::Yolo, cfg, Path::new("/tmp/ws"));
        match e.bash("echo secret") {
            Decision::Deny(m) => assert!(m.contains("hard-deny паттерн"), "msg: {m}"),
            d => panic!("ожидали Deny, получили {d:?}"),
        }
        // без deny-слова allow-префикс по-прежнему разрешает
        assert!(matches!(e.bash("echo hello"), Decision::Allow));
    }

    #[test]
    fn readonly_git_status_allowed_in_dontask() {
        assert_eq!(execpolicy::classify("git status"), execpolicy::CmdClass::Readonly);
        let e = engine(Mode::DontAsk);
        assert!(matches!(e.bash("git status"), Decision::Allow));
        // и внутри составной команды
        assert!(matches!(e.bash("git status && git log -1"), Decision::Allow));
    }

    /// Полуавтомат (v0.5.6): readonly bash и правки файлов в workspace — авто,
    /// bash с побочными эффектами и деструктив — с подтверждением.
    #[test]
    fn semi_auto_decision_matrix() {
        let e = engine(Mode::SemiAuto);
        // readonly bash — авто
        assert!(matches!(e.bash("git status"), Decision::Allow));
        assert!(matches!(e.bash("ls -la"), Decision::Allow));
        // bash с побочными эффектами — с подтверждением
        assert!(matches!(e.bash("make install"), Decision::Ask(_)));
        // деструктив — с подтверждением (не yolo!)
        assert!(matches!(e.bash("git reset --hard HEAD"), Decision::Ask(_)));
        // hard-deny — всегда запрет, даже в полуавтомате
        assert!(matches!(e.bash("rm -rf /"), Decision::Deny(_)));
        // метка режима
        assert_eq!(Mode::SemiAuto.label(), "Авто-правки");
    }

    /// Оверрайд режима через общий атомик (переключение /mode в рантайме).
    #[test]
    fn mode_override_atomic_switches_decisions() {
        let atomic = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(crate::permissions::MODE_UNSET));
        let e = PermissionEngine::new(Mode::Ask, PermissionConfig::default(), Path::new("/tmp/ws"))
            .with_mode_override(atomic.clone());
        // без оверрайда — базовый Ask: побочный bash → Ask
        assert!(matches!(e.bash("make install"), Decision::Ask(_)));
        // переключили в автомат → Allow
        atomic.store(crate::permissions::MODE_YOLO, std::sync::atomic::Ordering::Relaxed);
        assert!(matches!(e.bash("make install"), Decision::Allow));
        // переключили в полуавтомат → обратно Ask
        atomic.store(crate::permissions::MODE_SEMI, std::sync::atomic::Ordering::Relaxed);
        assert!(matches!(e.bash("make install"), Decision::Ask(_)));
        // и обратно в с-подтверждением
        atomic.store(crate::permissions::MODE_ASK, std::sync::atomic::Ordering::Relaxed);
        assert!(matches!(e.bash("make install"), Decision::Ask(_)));
    }

    #[test]
    fn policy_keeps_safe_prefix_allowance() {
        // белый список подан в PolicyEngine Allow-правилами: whitelisted-команды
        // не получают от классификатора решений строже текущих
        let e = engine(Mode::DontAsk);
        assert!(matches!(e.bash("python3 -c \"print(1)\""), Decision::Allow));
        assert!(matches!(e.bash("pip list"), Decision::Allow));
    }

    // --- THS-QA-02: правила по подкомандам (обход deny Bash(rm)) ---

    /// deny-правило Bash(rm) обязано ловить составные и «обёрнутые» команды:
    /// префикс проверяется по сырой строке, подкомандам каноникализации и их
    /// формам без обёрток (`env`, `nice`, `sudo`, `timeout`, `VAR=value`).
    #[test]
    fn rule_deny_catches_compound_and_wrapped_rm() {
        let e = engine(Mode::Yolo).with_rules(vec![crate::config::PermissionRule {
            decision: "deny".into(),
            pattern: "Bash(rm)".into(),
            reason: String::new(),
        }]);
        for cmd in [
            "rm x",
            "cd /tmp && rm x",
            "ls; rm -rf build",
            "ls | rm x",
            "env rm x",
            "nice rm x",
            "nice -n 5 rm x",
            "sudo -u root rm x",
            "A=1 rm x",
            "timeout 5 rm x",
        ] {
            match e.rule_decision("bash", cmd) {
                Some(Decision::Deny(_)) => {}
                d => panic!("ожидали Deny для `{cmd}`, получили {d:?}"),
            }
        }
        // обычные команды правило не цепляет: ни сырая строка, ни подкоманды,
        // ни слова-не-команды («rm» как аргумент grep/echo — не повод для deny)
        for cmd in ["ls -la", "git status", "grep rm log.txt", "echo rm", "cat x && ls"] {
            assert_eq!(e.rule_decision("bash", cmd), None, "cmd: {cmd}");
        }
        // не-bash инструменты — прежняя семантика: префикс только сырой строки
        assert_eq!(e.rule_decision("read_file", "rm x"), None);
    }

    /// Конфликт правил по разным подкомандам решается worst-of: deny одной
    /// подкоманды строже allow другой.
    #[test]
    fn rule_worst_of_across_subcommands() {
        let e = engine(Mode::Yolo).with_rules(vec![
            crate::config::PermissionRule {
                decision: "allow".into(),
                pattern: "Bash(git)".into(),
                reason: String::new(),
            },
            crate::config::PermissionRule {
                decision: "deny".into(),
                pattern: "Bash(rm)".into(),
                reason: String::new(),
            },
        ]);
        assert!(matches!(
            e.rule_decision("bash", "git status && rm x"),
            Some(Decision::Deny(_))
        ));
        assert!(matches!(
            e.rule_decision("bash", "git status && git log -1"),
            Some(Decision::Allow)
        ));
    }

    // --- THS-QA-01: MODE_DONTASK достижим из общего атомика ---

    /// Код MODE_DONTASK в оверрайд-атомике обязан распознаваться как
    /// Mode::DontAsk, а не проваливаться в базовый режим конструктора.
    #[test]
    fn mode_override_dontask_recognized() {
        let atomic = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(crate::permissions::MODE_DONTASK));
        let e = PermissionEngine::new(Mode::Ask, PermissionConfig::default(), Path::new("/tmp/ws"))
            .with_mode_override(atomic);
        assert_eq!(e.mode(), Mode::DontAsk);
        // и решения — по dontAsk: побочный bash запрещён без вопросов
        assert!(matches!(e.bash("make install"), Decision::Deny(_)));
    }
}
