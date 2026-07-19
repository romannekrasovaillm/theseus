//! Политика исполнения shell-команд: каноникализация, классификация, движок правил.
//!
//! По урокам обзора Codex:
//! - `core/src/command_canonicalization.rs` — составную команду разбираем на
//!   подкоманды ДО решения (иначе `ls && rm -rf /` выглядит как `ls`);
//! - `execpolicy` (`decision.rs`, `policy.rs`) — явные правила Allow/Ask/Deny
//!   важнее эвристик; при конфликте правил и по составной команде побеждает
//!   худшее решение.
//!
//! В отличие от codex (полный парсер tree-sitter-bash) здесь ручной сканер
//! кавычек/экранирования: дерево не строим, но операторы внутри кавычек и
//! экранированные разделителями не считаем.
//!
//! Принятые ограничения (fail-safe в сторону Unknown/Ask): подстановки `$(...)`
//! и бэктики не раскрываются (команда → Unknown, как codex отклоняет
//! не-word-only парс); редирект `>` детектируется грубо и поднимает Readonly
//! до Write; арифметика `$((...))`, here-doc и if/case не разбираются.
//!
//! Модуль самодостаточен: `Mode`/`Decision` зеркалят по смыслу
//! `permissions::{Mode, Decision}`, но не импортируются оттуда.

use regex::Regex;

/// Режим работы харнесса при не-автоматических решениях.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Спрашивать пользователя о командах строже read-only.
    Ask,
    /// Не спрашивать никогда (CI/headless без yolo): всё не-readonly запрещено.
    DontAsk,
    /// Разрешать всё, что не запрещено явными правилами Deny.
    Yolo,
}

/// Решение по команде. Порядок вариантов задаёт суровость `Allow < Ask < Deny`,
/// поэтому derived `Ord` агрегирует «худшее» решение по подкомандам и правилам.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Decision {
    /// Запустить без вопросов.
    Allow,
    /// Спросить пользователя (в режиме DontAsk класс-эвристика даёт Deny сразу).
    Ask,
    /// Запретить безусловно.
    Deny,
}

impl Decision {
    /// Имя варианта для текстов причин.
    pub fn as_str(&self) -> &'static str {
        match self {
            Decision::Allow => "Allow",
            Decision::Ask => "Ask",
            Decision::Deny => "Deny",
        }
    }
}

/// Класс подкоманды по степени опасности (эвристика по программе и флагам).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmdClass {
    /// Только чтение/печать: `ls`, `cat`, `grep`, `git status`, `cargo check`, ...
    Readonly,
    /// Пишет файлы/индекс: `touch`, `mkdir`, `cp`, `sed -i`, `git add`, ...
    Write,
    /// Сетевой доступ: `curl`, `wget`, `ssh`, `ping`, `git push/fetch`, ...
    Network,
    /// Разрушение данных/состояния: `rm`, `dd`, `mkfs`, `kill`, `git reset --hard`, ...
    Destructive,
    /// Не распознано: интерпретаторы, запускатели произвольных команд и т.п.
    Unknown,
}

impl CmdClass {
    /// Русская метка для причин и попапов согласования.
    pub fn ru_label(&self) -> &'static str {
        match self {
            CmdClass::Readonly => "только чтение",
            CmdClass::Write => "запись файлов",
            CmdClass::Network => "сетевой доступ",
            CmdClass::Destructive => "разрушительная",
            CmdClass::Unknown => "неизвестная",
        }
    }
}

/// Состояние кавычек при посимвольном сканировании.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Quote {
    None,
    Single,
    Double,
}

/// Разобрать составную команду на подкоманды по операторам `&&`, `||`, `|`, `;`.
///
/// Дополнительные разделители: перевод строки, одиночный `&` (фон) и bash-вариант
/// `|&`. Операторы внутри `'...'`/`"..."` и экранированные слэшем — не разделители.
/// Пустые сегменты (`ls;;;ls`) отбрасываются; подкоманды возвращаются обрезанными
/// по пробелам, кавычки и экраны в тексте сохраняются.
pub fn canonicalize_command(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::with_capacity(cmd.len());
    let mut quote = Quote::None;
    let mut chars = cmd.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Quote::Single, '\'') => { cur.push(c); quote = Quote::None; }
            (Quote::Single, _) => cur.push(c),
            (Quote::Double, '"') => { cur.push(c); quote = Quote::None; }
            // В "..." слэш экранирует лишь $ ` " \ и \n, но для разбора
            // разделителей важно только не потерять следующий символ.
            (Quote::Double, '\\') | (Quote::None, '\\') => {
                cur.push(c);
                if let Some(next) = chars.next() { cur.push(next); }
            }
            (Quote::Double, _) => cur.push(c),
            (Quote::None, '\'') => { cur.push(c); quote = Quote::Single; }
            (Quote::None, '"') => { cur.push(c); quote = Quote::Double; }
            // `&&` и одиночный `&` (фон) — оба разделители.
            (Quote::None, '&') => {
                if chars.peek() == Some(&'&') { chars.next(); }
                push_sub(&mut out, &mut cur);
            }
            // `||`, `|` и `|&` — разделители.
            (Quote::None, '|') => {
                if matches!(chars.peek(), Some('|') | Some('&')) { chars.next(); }
                push_sub(&mut out, &mut cur);
            }
            (Quote::None, ';') | (Quote::None, '\n') => push_sub(&mut out, &mut cur),
            (Quote::None, _) => cur.push(c),
        }
    }
    push_sub(&mut out, &mut cur);
    out
}

/// Добавить накопленный сегмент в список, если после обрезки он не пуст.
fn push_sub(out: &mut Vec<String>, cur: &mut String) {
    let trimmed = cur.trim();
    if !trimmed.is_empty() { out.push(trimmed.to_string()); }
    cur.clear();
}

/// Разбить подкоманду на слова (shlex-подобно): пробелы вне кавычек — разделители,
/// кавычки снимаются, `\x` превращается в `x`. Пустые кавычки (`''`) дают пустое
/// слово. Нужна классификатору: имя программы и флаги смотрим уже без кавычек.
pub fn split_words(cmd: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    // Есть ли уже содержимое текущего слова (пустые кавычки — тоже слово).
    let mut has = false;
    let mut quote = Quote::None;
    let mut chars = cmd.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Quote::Single, '\'') => quote = Quote::None,
            (Quote::Single, _) => cur.push(c),
            (Quote::Double, '"') => quote = Quote::None,
            (Quote::Double, '\\') => match chars.peek() {
                Some(&next) if matches!(next, '"' | '\\' | '$' | '`') => {
                    cur.push(next);
                    chars.next();
                }
                _ => cur.push(c),
            },
            (Quote::Double, _) => cur.push(c),
            (Quote::None, ch) if ch.is_whitespace() => {
                if has {
                    words.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            (Quote::None, '\'') => { quote = Quote::Single; has = true; }
            (Quote::None, '"') => { quote = Quote::Double; has = true; }
            (Quote::None, '\\') => {
                if let Some(next) = chars.next() { cur.push(next); }
                has = true;
            }
            (Quote::None, _) => { cur.push(c); has = true; }
        }
    }
    if has { words.push(cur); }
    words
}

/// Признаки, найденные грубым сканером по одной подкоманде.
struct ScanFlags {
    /// Неэкранированный `>` вне кавычек (кроме fd-дубля `>&`).
    redirect: bool,
    /// `$(` или обратная кавычка вне одиночных кавычек.
    subst: bool,
}

/// Сканировать подкоманду на редиректы вывода и подстановки команд. Подстановку
/// не раскрываем (это работа полноценного парсера), но сам факт её наличия делает
/// классификацию по первому слову небезопасной: за `echo $(rm -rf x)` что угодно.
fn scan_special(cmd: &str) -> ScanFlags {
    let mut flags = ScanFlags { redirect: false, subst: false };
    let mut quote = Quote::None;
    let mut chars = cmd.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Quote::Single, '\'') => quote = Quote::None,
            (Quote::Single, _) => {}
            (Quote::Double, '"') => quote = Quote::None,
            (Quote::Double, '\\') | (Quote::None, '\\') => { chars.next(); }
            (Quote::None, '\'') => quote = Quote::Single,
            (Quote::None, '"') => quote = Quote::Double,
            (Quote::None, '>') => {
                // `2>&1` — дубль дескриптора, а не запись в файл.
                if chars.peek() == Some(&'&') { chars.next(); } else { flags.redirect = true; }
            }
            (Quote::Double, '`') | (Quote::None, '`') => flags.subst = true,
            (Quote::Double, '$') | (Quote::None, '$') if chars.peek() == Some(&'(') => flags.subst = true,
            _ => {}
        }
    }
    flags
}

/// Классифицировать ОДНУ подкоманду (уже выделенную `canonicalize_command`).
///
/// Сначала разворачиваются обёртки (`VAR=1 cmd`, `env`, `sudo`, `timeout`,
/// `nice`, `command`, `builtin`, `nohup`, `stdbuf`), затем решение принимается
/// по имени программы, её подкоманде (git/cargo/pip/npm/systemctl/crontab)
/// и опасным флагам. Команда с подстановкой — всегда Unknown; редирект
/// вывода поднимает Readonly до Write.
pub fn classify(cmd: &str) -> CmdClass {
    let flags = scan_special(cmd);
    if flags.subst { return CmdClass::Unknown; }
    let class = classify_by_words(cmd);
    if flags.redirect && class == CmdClass::Readonly { CmdClass::Write } else { class }
}

/// Класс по словам подкоманды (без учёта редиректов/подстановок).
fn classify_by_words(cmd: &str) -> CmdClass {
    let words = split_words(cmd);
    let mut i = 0;
    // Разворачиваем обёртки и префиксные VAR=value, пока не доберёмся до программы.
    loop {
        let Some(w) = words.get(i) else { return CmdClass::Unknown };
        if is_env_assignment(w) {
            i += 1;
            continue;
        }
        match basename(w) {
            "command" | "builtin" | "nohup" | "exec" => i += 1,
            "env" => i = skip_wrapper_args(&words, i + 1, &["-u", "-C", "-S", "--unset", "--chdir", "--split-string"]),
            "sudo" => {
                let vf = ["-u", "-g", "-h", "-p", "-C", "-T", "--user", "--group", "--host", "--prompt"];
                i = skip_wrapper_args(&words, i + 1, &vf);
            }
            "nice" => i = skip_wrapper_args(&words, i + 1, &["-n", "--adjustment"]),
            "stdbuf" => i = skip_wrapper_args(&words, i + 1, &["-i", "-o", "-e", "--input", "--output", "--error"]),
            // После флагов timeout идёт ДЛИТЕЛЬНОСТЬ, и только потом команда.
            "timeout" => i = skip_wrapper_args(&words, i + 1, &["-k", "-s", "--kill-after", "--signal"]) + 1,
            _ => break,
        }
    }
    let Some(prog) = words.get(i) else { return CmdClass::Unknown };
    classify_prog(basename(prog), &words[i + 1..])
}

/// Класс по имени программы и её аргументам.
fn classify_prog(prog: &str, args: &[String]) -> CmdClass {
    use CmdClass::*;
    match prog {
        // --- только чтение ---
        "ls" | "cat" | "grep" | "egrep" | "fgrep" | "rg" | "echo" | "printf" | "pwd"
        | "head" | "tail" | "wc" | "sort" | "uniq" | "date" | "uname" | "df" | "du" | "free"
        | "stat" | "file" | "which" | "whereis" | "whoami" | "id" | "hostname" | "true" | "false"
        | "printenv" | "diff" | "cmp" | "comm" | "tr" | "cut" | "paste" | "join" | "nl" | "od"
        | "readlink" | "realpath" | "basename" | "dirname" | "cal" | "uptime" | "w" | "who"
        | "ps" | "lsof" | "ss" | "netstat" | "lsblk" | "lsmod" | "dmesg" | "man" | "less"
        | "more" | "tree" | "jq" | "column" | "seq" | "journalctl" => Readonly,

        // --- запись в пределах ФС ---
        "touch" | "mkdir" | "cp" | "mv" | "ln" | "tee" | "chmod" | "chown" | "chgrp"
        | "truncate" | "install" | "patch" | "rmdir" | "mktemp" | "split" | "cpio"
        | "zip" | "unzip" | "gzip" | "gunzip" | "bzip2" | "bunzip2" | "xz" | "unxz"
        | "ssh-keygen" | "rustc" => Write,

        // --- сеть ---
        "curl" | "wget" | "ssh" | "scp" | "sftp" | "ftp" | "telnet" | "ping" | "ping6"
        | "traceroute" | "tracepath" | "dig" | "host" | "nslookup" | "whois" | "nc" | "ncat"
        | "netcat" | "socat" | "nmap" | "rsync" | "rclone" | "mosh" | "aria2c" | "rustup" => Network,

        // --- разрушение данных/состояния системы ---
        "rm" | "dd" | "shred" | "wipe" | "fdisk" | "sfdisk" | "parted" | "wipefs" | "mkswap"
        | "mount" | "umount" | "swapon" | "swapoff" | "kill" | "killall" | "pkill"
        | "shutdown" | "reboot" | "halt" | "poweroff" | "init" | "passwd" | "useradd"
        | "userdel" | "usermod" | "groupadd" | "groupdel" | "sysctl" | "modprobe" | "insmod"
        | "rmmod" | "apt" | "apt-get" | "dpkg" | "dnf" | "yum" | "pacman" | "zypper"
        | "snap" | "flatpak" | "brew" | "visudo" => Destructive,

        // `mkfs` и все `mkfs.*`.
        _ if prog.starts_with("mkfs") => Destructive,

        // --- специальные разборы по подкомандам/флагам ---
        "git" => classify_git(args),
        "cargo" => classify_cargo(args),
        "pip" | "pip3" | "pipx" => classify_pip(args),
        "npm" | "pnpm" | "yarn" | "bun" => classify_npm(args),
        "sed" => classify_sed(args),
        "find" => classify_find(args),
        "tar" => classify_tar(args),
        "crontab" => classify_crontab(args),
        "systemctl" => classify_systemctl(args),

        // Интерпретаторы (sh, python, node, awk, ...) и запускатели (xargs,
        // parallel, watch) не берёмся судить — это всегда Unknown.
        _ => Unknown,
    }
}

/// Класс git-подкоманды: учитываем глобальные флаги git и опасные флаги.
fn classify_git(args: &[String]) -> CmdClass {
    use CmdClass::*;
    // Пропускаем глобальные флаги git: `git -C /repo -c k=v status`.
    let mut i = 0;
    while let Some(a) = args.get(i) {
        match a.as_str() {
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" => i += 2,
            s if s.starts_with("--git-dir=") || s.starts_with("--work-tree=") || s.starts_with("--namespace=") => i += 1,
            // Неизвестный глобальный флаг — считаем унарным (без значения).
            s if s.starts_with('-') => i += 1,
            _ => break,
        }
    }
    let Some(sub) = args.get(i) else { return Readonly }; // голый `git`, `git --version`, `git help`
    let rest = &args[i + 1..];
    match sub.as_str() {
        "status" | "log" | "diff" | "show" | "rev-parse" | "rev-list" | "ls-files" | "ls-tree"
        | "grep" | "blame" | "annotate" | "shortlog" | "describe" | "count-objects" | "var"
        | "whatchanged" | "verify-commit" | "verify-tag" | "cat-file" | "name-rev" | "show-ref" => Readonly,
        "branch" => {
            if has_short_flag(rest, 'D') {
                Destructive // принудительное удаление ветки
            } else if "dmMcC".chars().any(|c| has_short_flag(rest, c))
                || has_long_flag(rest, "--delete") || has_long_flag(rest, "--move") || has_long_flag(rest, "--copy")
                || has_positional(rest)
            {
                Write // удаление/переименование/создание ветки
            } else {
                Readonly // `git branch`, `git branch -a`
            }
        }
        // -d/--delete или позиционный аргумент — изменение; иначе листинг.
        "tag" => if has_short_flag(rest, 'd') || has_long_flag(rest, "--delete") || has_positional(rest) {
            Write
        } else {
            Readonly
        },
        "remote" => match first_non_flag(rest) {
            None => Readonly, // `git remote`, `git remote -v`
            Some("show") | Some("prune") | Some("update") => Network, // ходит на remote
            Some(_) => Write,  // add/remove/set-url/rename
        },
        "stash" => match first_non_flag(rest) {
            Some("drop") | Some("clear") => Destructive,
            Some("list") | Some("show") | None => Readonly,
            Some(_) => Write, // push/pop/apply/save
        },
        "reflog" => if matches!(first_non_flag(rest), Some("expire") | Some("delete")) { Destructive } else { Readonly },
        "add" | "commit" | "checkout" | "switch" | "restore" | "merge" | "rebase" | "cherry-pick"
        | "revert" | "mv" | "am" | "apply" | "format-patch" | "gc" | "worktree" | "bisect"
        | "notes" | "update-index" | "read-tree" | "write-tree" | "commit-tree" | "prune" => Write,
        // --soft/--mixed/--keep пишут индекс; --hard ещё и сносит рабочее дерево.
        "reset" => if has_long_flag(rest, "--hard") { Destructive } else { Write },
        // `git clean` по дизайну требует -f; -n (dry-run) — редкость, считаем строго.
        "clean" => Destructive,
        // `git rm` удаляет файлы и из рабочего дерева.
        "rm" => Destructive,
        "fetch" | "pull" | "clone" | "submodule" | "ls-remote" | "archive" => Network,
        // --force/-f — слепая перезапись remote; --force-with-lease мягче — Network.
        "push" => if has_long_flag(rest, "--force") || has_short_flag(rest, 'f') { Destructive } else { Network },
        "config" => classify_git_config(rest),
        _ => Unknown,
    }
}

/// `git config`: чтение значений — Readonly, установка — Write.
fn classify_git_config(rest: &[String]) -> CmdClass {
    const READONLY_FLAGS: &[&str] = &["--get", "--get-all", "--get-regexp", "-l", "--list"];
    if rest.iter().any(|a| READONLY_FLAGS.contains(&a.as_str())) { return CmdClass::Readonly; }
    // `git config user.name` (один позиционный, без значения) — чтение.
    let positional = rest.iter().filter(|a| !a.starts_with('-')).count();
    if positional <= 1 { CmdClass::Readonly } else { CmdClass::Write }
}

/// Класс cargo-подкоманды (учитываем `+toolchain` и глобальные флаги).
fn classify_cargo(args: &[String]) -> CmdClass {
    use CmdClass::*;
    let mut i = 0;
    while let Some(a) = args.get(i) {
        if a.starts_with('+') || a.starts_with('-') { i += 1; } else { break; }
    }
    let Some(sub) = args.get(i) else { return Readonly }; // `cargo --version`
    match sub.as_str() {
        // По заданию check/clippy/test — read-only уровень доверия (как SAFE_BASH_PREFIXES
        // в permissions.rs), хотя build-скрипты формально выполняются.
        "check" | "clippy" | "test" | "bench" | "metadata" | "tree" | "verify-project" | "pkgid"
        | "locate-project" | "version" | "help" => Readonly,
        "build" | "run" | "doc" | "fix" | "fmt" | "add" | "remove" | "new" | "init" | "uninstall" => Write,
        "install" | "update" | "search" | "publish" | "login" | "logout" | "owner" | "yank" => Network,
        "clean" => Destructive,
        _ => Unknown,
    }
}

/// Класс pip-подкоманды.
fn classify_pip(args: &[String]) -> CmdClass {
    match first_non_flag(args) {
        Some("list") | Some("show") | Some("freeze") | Some("check") | None => CmdClass::Readonly,
        Some("install") | Some("download") | Some("index") | Some("search") => CmdClass::Network,
        Some("uninstall") => CmdClass::Destructive,
        Some(_) => CmdClass::Unknown,
    }
}

/// Класс npm-подобных пакетных менеджеров (npm/pnpm/yarn/bun).
fn classify_npm(args: &[String]) -> CmdClass {
    match first_non_flag(args) {
        Some("ls") | Some("list") | Some("outdated") | Some("view") | Some("info") | None => CmdClass::Readonly,
        Some("install") | Some("add") | Some("update") | Some("upgrade") | Some("publish") | Some("link") => {
            CmdClass::Network
        }
        Some("remove") | Some("uninstall") | Some("prune") | Some("dedupe") => CmdClass::Write,
        // run/test/exec/dlx и всё прочее выполняет произвольные скрипты.
        Some(_) => CmdClass::Unknown,
    }
}

/// `sed -i` перезаписывает файл на месте; без `-i` — фильтр в stdout.
fn classify_sed(args: &[String]) -> CmdClass {
    let long_in_place = args.iter().any(|a| a.as_str() == "--in-place" || a.starts_with("--in-place="));
    if long_in_place || has_short_flag(args, 'i') { CmdClass::Write } else { CmdClass::Readonly }
}

/// `find -delete` удаляет найденное; `-exec`/`-ok` выполняет произвольную команду.
fn classify_find(args: &[String]) -> CmdClass {
    if args.iter().any(|a| a.as_str() == "-delete") { return CmdClass::Destructive; }
    if args.iter().any(|a| matches!(a.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir")) {
        return CmdClass::Unknown;
    }
    CmdClass::Readonly
}

/// `tar t` только листает архив; создание/распаковка пишут файлы.
/// Длинные формы (`--list`/`--extract`/`--create`) ловятся по буквам t/x/c.
fn classify_tar(args: &[String]) -> CmdClass {
    match args.first() {
        Some(mode) => {
            let m = mode.trim_start_matches('-');
            if m.contains('t') && !m.contains('x') && !m.contains('c') { CmdClass::Readonly } else { CmdClass::Write }
        }
        None => CmdClass::Unknown,
    }
}

/// `crontab -l` читает, `-e` пишет, `-r` сносит всю таблицу.
fn classify_crontab(args: &[String]) -> CmdClass {
    if has_short_flag(args, 'r') { CmdClass::Destructive }
    else if has_short_flag(args, 'e') { CmdClass::Write }
    else { CmdClass::Readonly }
}

/// systemctl: запросы статуса — Readonly, управление юнитами — Destructive.
fn classify_systemctl(args: &[String]) -> CmdClass {
    match first_non_flag(args) {
        // Голый `systemctl` — это list-units.
        None => CmdClass::Readonly,
        Some(
            "status" | "show" | "list-units" | "list-services" | "list-timers" | "is-active"
            | "is-enabled" | "is-failed" | "cat" | "help",
        ) => CmdClass::Readonly,
        Some(
            "start" | "stop" | "restart" | "reload" | "reload-or-restart" | "enable" | "disable"
            | "mask" | "unmask" | "kill" | "isolate" | "daemon-reexec",
        ) => CmdClass::Destructive,
        Some(_) => CmdClass::Unknown,
    }
}

/// Имя программы без пути: `/usr/bin/ls` → `ls`.
fn basename(path: &str) -> &str { path.rsplit('/').next().unwrap_or(path) }

/// Похоже ли слово на префиксное присваивание `VAR=value`.
fn is_env_assignment(w: &str) -> bool {
    let Some(eq) = w.find('=') else { return false };
    let key = &w[..eq];
    !key.is_empty() && !key.starts_with(|c: char| c.is_ascii_digit()) && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Пропустить флаги обёртки; флаги из `value_flags` забирают и следующее слово.
/// Присваивания `VAR=value` тоже пропускаются (для `env A=1 cmd`).
fn skip_wrapper_args(words: &[String], mut i: usize, value_flags: &[&str]) -> usize {
    while let Some(w) = words.get(i) {
        if value_flags.contains(&w.as_str()) { i += 2; }
        else if w.starts_with('-') || is_env_assignment(w) { i += 1; }
        else { break; }
    }
    i
}

/// Есть ли среди аргументов короткий флаг с символом `c` (учитывает слипшиеся `-rf`).
fn has_short_flag(args: &[String], c: char) -> bool {
    args.iter().any(|a| a.starts_with('-') && !a.starts_with("--") && a[1..].contains(c))
}

/// Есть ли точный длинный флаг `--name`.
fn has_long_flag(args: &[String], name: &str) -> bool { args.iter().any(|a| a.as_str() == name) }

/// Есть ли позиционный (не начинающийся с `-`) аргумент.
fn has_positional(args: &[String]) -> bool { args.iter().any(|a| !a.starts_with('-')) }

/// Первый позиционный аргумент (не флаг).
fn first_non_flag(args: &[String]) -> Option<&str> { args.iter().find(|a| !a.starts_with('-')).map(String::as_str) }

/// Правило политики: regex по ТЕКСТУ команды/подкоманды → решение.
///
/// Правила проверяются ДО эвристики класса и имеют приоритет над ней (урок codex
/// execpolicy: явная политика важнее эвристик). Если матчится несколько правил —
/// побеждает худшее решение (`Deny > Ask > Allow`).
#[derive(Debug, Clone)]
pub struct Rule {
    /// Паттерн; матчится поиском (не anchor-match) на тексте после trim.
    pub pattern: Regex,
    /// Решение при матче.
    pub decision: Decision,
    /// Человекочитаемая причина; пустая — в причинах используется текст паттерна.
    pub reason: String,
}

impl Rule {
    /// Скомпилировать правило из строки-паттерна.
    pub fn compile(pattern: &str, decision: Decision, reason: impl Into<String>) -> Result<Self, regex::Error> {
        Ok(Self { pattern: Regex::new(pattern)?, decision, reason: reason.into() })
    }
}

/// Движок политики: набор правил + эвристика классов + режим.
///
/// Порядок решения:
/// 1. правила проверяются и по ПОЛНОМУ тексту команды — часть угроз (fork-бомба)
///    состоит из shell-операторов и невидима на уровне отдельных подкоманд;
/// 2. по каждой подкоманде: если матчится правило — решение правил (худшее из
///    матчнувшихся), иначе решение по классу (`classify`) и режиму.
///
/// Итог по составной команде — худшее из всех этих решений.
#[derive(Debug, Clone)]
pub struct PolicyEngine {
    rules: Vec<Rule>,
}

impl PolicyEngine {
    /// Движок с явным набором правил (пустой вектор — только эвристика классов).
    pub fn new(rules: Vec<Rule>) -> Self { Self { rules } }

    /// Базовый набор hard-deny правил (урок permissions.rs: катастрофическое
    /// запрещено даже в yolo). Константные паттерны проверены тестами; на всякий
    /// случай нескомпилировавшиеся молча пропускаются, без `.unwrap()` вне тестов
    /// (как RegexSet в permissions.rs).
    pub fn default_rules() -> Vec<Rule> {
        let raw: &[(&str, &str)] = &[
            (r"\brm\s+(-[\w-]*\s+)*(--no-preserve-root\s+)?(/|/\*|~|\$HOME)\s*$", "rm корня/домашней директории"),
            (r"\bmkfs(\.\w+)?\b", "форматирование устройства"),
            (r"\bdd\b.*\bof=/dev/", "dd на блочное устройство"),
            (r">\s*/dev/sd[a-z]", "запись редиректом в блочное устройство"),
            (r"\b(shutdown|reboot|halt|poweroff)\b", "выключение/перезагрузка машины"),
            (r"\bgit\s+push\b.*(--force|\s-f)(\s|$).*\b(main|master)\b", "force-push в main/master"),
            (r":\(\)\s*\{[^}]*\}\s*;\s*:", "fork-бомба"),
            (r"\bchmod\s+(-[\w-]*\s+)*777\s+/\s*$", "chmod 777 на корень"),
        ];
        raw.iter()
            .filter_map(|&(pat, why)| Rule::compile(pat, Decision::Deny, why).ok())
            .collect()
    }

    /// Решение по (возможно составной) команде + причины.
    ///
    /// Возвращает худшее решение среди правил по полному тексту и решений
    /// подкоманд. В `reasons` попадают объяснения только строже Allow:
    /// для полностью разрешённой команды список пуст.
    pub fn decide(&self, cmd: &str, mode: Mode) -> (Decision, Vec<String>) {
        let subs = canonicalize_command(cmd);
        let mut decision = Decision::Allow;
        let mut reasons = Vec::new();
        // Голос правил по полному тексту (ловим конструкции из операторов).
        if let Some((d, mut rs)) = self.rule_decision(cmd) {
            if d > Decision::Allow { reasons.append(&mut rs); }
            decision = decision.max(d);
        }
        for sub in &subs {
            let (d, mut rs) = self.decide_sub(sub, mode);
            if d > Decision::Allow { reasons.append(&mut rs); }
            decision = decision.max(d);
        }
        // Дедупликация: целая команда из одной подкоманды даёт те же строки.
        let mut seen = std::collections::HashSet::new();
        reasons.retain(|r| seen.insert(r.clone()));
        (decision, reasons)
    }

    /// Решение по одной подкоманде: сначала правила, потом класс + режим.
    fn decide_sub(&self, sub: &str, mode: Mode) -> (Decision, Vec<String>) {
        if let Some(rd) = self.rule_decision(sub) { return rd; }
        let class = classify(sub);
        let decision = decide_by_class(class, mode);
        let reason = format!("«{sub}»: класс «{}» → {}", class.ru_label(), decision.as_str());
        (decision, vec![reason])
    }

    /// Худшее решение среди правил, матчнущихся на `text` (None — правил нет).
    fn rule_decision(&self, text: &str) -> Option<(Decision, Vec<String>)> {
        let mut by_rule: Option<Decision> = None;
        let mut reasons = Vec::new();
        for rule in &self.rules {
            if rule.pattern.is_match(text) {
                let why = if rule.reason.is_empty() {
                    format!("«{text}»: правило /{}/ → {}", rule.pattern.as_str(), rule.decision.as_str())
                } else {
                    format!("«{text}»: {} → {}", rule.reason, rule.decision.as_str())
                };
                reasons.push(why);
                by_rule = Some(by_rule.map_or(rule.decision, |d| d.max(rule.decision)));
            }
        }
        by_rule.map(|d| (d, reasons))
    }
}

/// Таблица «режим × класс → решение»: readonly разрешено всегда,
/// остальное — по строгости режима.
fn decide_by_class(class: CmdClass, mode: Mode) -> Decision {
    match (mode, class) {
        (_, CmdClass::Readonly) => Decision::Allow,
        (Mode::Ask, _) => Decision::Ask,
        (Mode::DontAsk, _) => Decision::Deny,
        (Mode::Yolo, _) => Decision::Allow,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subs(cmd: &str) -> Vec<String> { canonicalize_command(cmd) }

    fn rule(pat: &str, d: Decision) -> Rule { Rule::compile(pat, d, "").unwrap() }

    fn deny(pat: &str, why: &str) -> Rule { Rule::compile(pat, Decision::Deny, why).unwrap() }

    // --- каноникализация ---

    #[test]
    fn split_basic_operators_and_empty() {
        assert_eq!(subs("ls -la && cat x"), vec!["ls -la", "cat x"]);
        assert_eq!(subs("a || b | c ; d"), vec!["a", "b", "c", "d"]);
        assert_eq!(subs("ls & top"), vec!["ls", "top"]); // одиночный & — фон
        assert_eq!(subs("first\nsecond"), vec!["first", "second"]);
        assert_eq!(subs("a |& b"), vec!["a", "b"]); // bash: пайп с stderr
        // Пустые сегменты отбрасываются.
        assert_eq!(subs("ls;;;ls"), vec!["ls", "ls"]);
        assert_eq!(subs("  &&  && ls"), vec!["ls"]);
        assert!(subs("   ").is_empty());
        assert!(subs("").is_empty());
    }

    #[test]
    fn split_respects_quotes() {
        assert_eq!(subs("echo 'a;b' && ls"), vec!["echo 'a;b'", "ls"]);
        assert_eq!(subs("echo \"x|y\" | cat"), vec!["echo \"x|y\"", "cat"]);
        assert_eq!(subs("echo 'a && b'"), vec!["echo 'a && b'"]);
        // Незакрытая кавычка — просто часть текста, разделителей нет.
        assert_eq!(subs("echo 'oops && ls"), vec!["echo 'oops && ls"]);
    }

    #[test]
    fn split_respects_escapes() {
        assert_eq!(subs("echo a\\;b ; ls"), vec!["echo a\\;b", "ls"]);
        assert_eq!(subs("echo a\\&\\&b && ls"), vec!["echo a\\&\\&b", "ls"]);
        // Экранированная кавычка не открывает секцию кавычек.
        assert_eq!(subs("echo \\\" ; ls"), vec!["echo \\\"", "ls"]);
    }

    #[test]
    fn split_words_quotes_and_escapes() {
        assert_eq!(split_words("echo 'a b' \"c\" d\\ e"), vec!["echo", "a b", "c", "d e"]);
        // Пустые кавычки — отдельное пустое слово.
        assert_eq!(split_words("echo '' x"), vec!["echo", "", "x"]);
        assert_eq!(
            split_words("git commit -m \"msg: fix; deploy\""),
            vec!["git", "commit", "-m", "msg: fix; deploy"]
        );
    }

    // --- классификатор ---

    #[test]
    fn classify_readonly_basics() {
        for cmd in [
            "ls -la", "cat file.txt", "grep -r TODO src", "rg pattern", "pwd", "head -n 5 x",
            "tail -f log", "wc -l f", "sort -u f", "uniq -c f", "echo hello", "find . -name '*.rs'",
            "cargo check", "cargo clippy -- -D warnings", "cargo test -p foo", "sed 's/a/b/' f",
            "git status", "git log --oneline -5", "git diff HEAD~1", "git show abc123", "git branch",
        ] {
            assert_eq!(classify(cmd), CmdClass::Readonly, "cmd: {cmd}");
        }
    }

    #[test]
    fn classify_write_basics() {
        for cmd in [
            "touch f", "mkdir -p a/b", "cp a b", "mv a b", "ln -s a b", "tee out.log",
            "sed -i 's/a/b/' f", "chmod +x s.sh", "cargo build", "git add .", "git commit -m x",
            "git checkout -b feat", "git reset --soft HEAD~1",
        ] {
            assert_eq!(classify(cmd), CmdClass::Write, "cmd: {cmd}");
        }
    }

    #[test]
    fn classify_destructive_basics() {
        for cmd in [
            "rm -rf build", "dd if=/dev/zero of=/dev/sda", "mkfs.ext4 /dev/sda1", "shred secret",
            "kill -9 1234", "shutdown now", "apt install htop",
        ] {
            assert_eq!(classify(cmd), CmdClass::Destructive, "cmd: {cmd}");
        }
    }

    #[test]
    fn classify_network_basics() {
        for cmd in [
            "curl https://x", "wget -q url", "ssh host ls", "scp f host:", "ping 1.1.1.1",
            "git fetch", "git pull", "git push origin main", "git clone url", "cargo install ripgrep",
            "pip install requests", "npm install",
        ] {
            assert_eq!(classify(cmd), CmdClass::Network, "cmd: {cmd}");
        }
    }

    #[test]
    fn classify_git_dangerous_flags() {
        assert_eq!(classify("git reset --hard HEAD"), CmdClass::Destructive);
        assert_eq!(classify("git reset --mixed"), CmdClass::Write);
        assert_eq!(classify("git push --force origin main"), CmdClass::Destructive);
        assert_eq!(classify("git push -f"), CmdClass::Destructive);
        // lease мягче: перезапись remote с проверкой — уровень сети.
        assert_eq!(classify("git push --force-with-lease"), CmdClass::Network);
        assert_eq!(classify("git clean -fdx"), CmdClass::Destructive);
        assert_eq!(classify("git branch -D old"), CmdClass::Destructive);
        assert_eq!(classify("git branch -d old"), CmdClass::Write);
        assert_eq!(classify("git stash drop"), CmdClass::Destructive);
        assert_eq!(classify("git rm cached.txt"), CmdClass::Destructive);
    }

    #[test]
    fn classify_git_global_flags_and_reads() {
        assert_eq!(classify("git -C /repo status"), CmdClass::Readonly);
        assert_eq!(classify("git --git-dir=/r/.git log"), CmdClass::Readonly);
        assert_eq!(classify("git -c color.ui=false diff"), CmdClass::Readonly);
        assert_eq!(classify("git remote -v"), CmdClass::Readonly);
        assert_eq!(classify("git remote add origin url"), CmdClass::Write);
        assert_eq!(classify("git config --get user.name"), CmdClass::Readonly);
        assert_eq!(classify("git config user.email a@b"), CmdClass::Write);
        assert_eq!(classify("git stash list"), CmdClass::Readonly);
    }

    #[test]
    fn classify_redirect_bumps_readonly_to_write() {
        assert_eq!(classify("cat a > b"), CmdClass::Write);
        assert_eq!(classify("sort x >> y"), CmdClass::Write);
        assert_eq!(classify("git diff > /tmp/patch"), CmdClass::Write);
        // Дубль дескриптора — не запись в файл.
        assert_eq!(classify("cargo test 2>&1"), CmdClass::Readonly);
        // `>` внутри кавычек — литерал.
        assert_eq!(classify("echo 'a > b'"), CmdClass::Readonly);
    }

    #[test]
    fn classify_substitution_is_unknown() {
        assert_eq!(classify("echo $(rm -rf x)"), CmdClass::Unknown);
        assert_eq!(classify("echo `id`"), CmdClass::Unknown);
        assert_eq!(classify("echo \"$(date)\""), CmdClass::Unknown);
        // В одиночных кавычках подстановка — литерал.
        assert_eq!(classify("echo '$(date)'"), CmdClass::Readonly);
        // Интерпретаторы и запускатели — не берёмся судить.
        assert_eq!(classify("bash -c 'ls'"), CmdClass::Unknown);
        assert_eq!(classify("python3 script.py"), CmdClass::Unknown);
        assert_eq!(classify("xargs rm"), CmdClass::Unknown);
        assert_eq!(classify("find . -exec rm {} ;"), CmdClass::Unknown);
        assert_eq!(classify("find . -delete"), CmdClass::Destructive);
    }

    #[test]
    fn classify_wrappers() {
        assert_eq!(classify("FOO=bar ls -la"), CmdClass::Readonly);
        assert_eq!(classify("sudo rm -rf /tmp/x"), CmdClass::Destructive);
        assert_eq!(classify("sudo -u root cat /etc/shadow"), CmdClass::Readonly);
        assert_eq!(classify("env A=1 curl https://x"), CmdClass::Network);
        assert_eq!(classify("timeout 10 ls"), CmdClass::Readonly);
        assert_eq!(classify("nice -n 5 rm f"), CmdClass::Destructive);
        assert_eq!(classify("command cat f"), CmdClass::Readonly);
        assert_eq!(classify("/usr/bin/git status"), CmdClass::Readonly);
    }

    // --- движок политики ---

    #[test]
    fn decide_by_class_matrix() {
        let e = PolicyEngine::new(vec![]);
        // Readonly разрешён во всех режимах.
        for m in [Mode::Ask, Mode::DontAsk, Mode::Yolo] {
            assert_eq!(e.decide("ls -la", m).0, Decision::Allow);
        }
        assert_eq!(e.decide("touch f", Mode::Ask).0, Decision::Ask);
        assert_eq!(e.decide("touch f", Mode::Yolo).0, Decision::Allow);
        assert_eq!(e.decide("touch f", Mode::DontAsk).0, Decision::Deny);
        assert_eq!(e.decide("rm -rf x", Mode::Ask).0, Decision::Ask);
        // Unknown — как Write по режиму.
        assert_eq!(e.decide("make install", Mode::Ask).0, Decision::Ask);
        assert_eq!(e.decide("make install", Mode::Yolo).0, Decision::Allow);
        assert_eq!(e.decide("make install", Mode::DontAsk).0, Decision::Deny);
        // Пустая команда — нечего исполнять.
        assert_eq!(e.decide("", Mode::DontAsk), (Decision::Allow, vec![]));
        assert_eq!(e.decide("   ", Mode::Ask).0, Decision::Allow);
    }

    #[test]
    fn compound_takes_worst_decision() {
        let e = PolicyEngine::new(vec![]);
        assert_eq!(e.decide("ls && touch f", Mode::Ask).0, Decision::Ask);
        assert_eq!(e.decide("touch f && rm -rf x", Mode::Ask).0, Decision::Ask);
        // Deny от класса в DontAsk перекрывает Allow readonly-подкоманд.
        assert_eq!(e.decide("ls && curl https://x", Mode::DontAsk).0, Decision::Deny);
        assert_eq!(e.decide("ls | grep x && pwd", Mode::Yolo).0, Decision::Allow);
    }

    #[test]
    fn rule_allow_overrides_class() {
        let e = PolicyEngine::new(vec![rule(r"^rm -rf /tmp/build(/.+)?$", Decision::Allow)]);
        assert_eq!(e.decide("rm -rf /tmp/build", Mode::Ask).0, Decision::Allow);
        // Вне паттерна — обычная эвристика.
        assert_eq!(e.decide("rm -rf /tmp/other", Mode::Ask).0, Decision::Ask);
    }

    #[test]
    fn deny_beats_allow_and_ask() {
        let e = PolicyEngine::new(vec![
            rule(r"^git\b", Decision::Allow),
            rule(r"push", Decision::Ask),
            deny(r"git\s+push\b.*--force", "форс-пуш запрещён политикой"),
        ]);
        // Allow + Ask по одной команде → Ask.
        assert_eq!(e.decide("git push origin main", Mode::Ask).0, Decision::Ask);
        // Deny бьёт Allow/Ask даже в yolo.
        assert_eq!(e.decide("git push --force origin main", Mode::Yolo).0, Decision::Deny);
        let (d, reasons) = e.decide("git push --force origin main", Mode::Yolo);
        assert_eq!(d, Decision::Deny);
        assert!(reasons.iter().any(|r| r.contains("форс-пуш")), "reasons: {reasons:?}");
    }

    #[test]
    fn deny_rule_in_compound_wins() {
        let e = PolicyEngine::new(vec![deny(r"\bmkfs\b", "форматирование"), rule(r".*", Decision::Allow)]);
        assert_eq!(e.decide("ls && mkfs.ext4 /dev/sda1", Mode::Yolo).0, Decision::Deny);
        // Allow-правило на всё — и ничего строже нет.
        assert_eq!(e.decide("ls && pwd", Mode::Ask).0, Decision::Allow);
    }

    #[test]
    fn default_rules_block_catastrophic_even_in_yolo() {
        let e = PolicyEngine::new(PolicyEngine::default_rules());
        for cmd in [
            "rm -rf /", "rm -rf /*", "rm -rf ~", "dd if=/dev/zero of=/dev/sda", "mkfs /dev/sda",
            "shutdown -h now", "git push --force origin main", ":(){ :|:& };:",
        ] {
            assert_eq!(e.decide(cmd, Mode::Yolo).0, Decision::Deny, "cmd: {cmd}");
        }
        // Обычная работа не страдает от false positive.
        assert_eq!(e.decide("cargo check && git status", Mode::DontAsk).0, Decision::Allow);
        assert_eq!(e.decide("touch f", Mode::Yolo).0, Decision::Allow);
    }

    #[test]
    fn reasons_explain_non_allow() {
        let e = PolicyEngine::new(vec![]);
        let (d, reasons) = e.decide("ls && touch f", Mode::Ask);
        assert_eq!(d, Decision::Ask);
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("запись файлов"), "reasons: {reasons:?}");
        assert!(reasons[0].contains("touch f"));
        // Полностью разрешённая команда — причин нет.
        let (_, reasons) = e.decide("ls && git status", Mode::Ask);
        assert!(reasons.is_empty());
    }
}
