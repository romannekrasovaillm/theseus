//! Экранирование и разбор shell-строк (POSIX).
//!
//! Образец — подход codex-rs (`shell-escalation` / `shell-command`): команда,
//! пришедшая от модели, показывается человеку на одобрение в виде СТРОКИ, а
//! выполняется как argv. Между этими представлениями нужен надёжный мост:
//! сериализация argv в строку без потерь и разбор строки обратно в argv.
//! Именно это и делает модуль:
//!
//! * [`quote`] — экранирование одного аргумента одинарными кавычками POSIX
//!   с классическим приёмом `'\''` для кавычки внутри текста. Результат
//!   гарантированно разбирается любым POSIX-shell обратно в исходную строку
//!   (инъекция через `$`, backticks, `;`, `|`, переводы строк исключена).
//! * [`quote_if_needed`] и [`quote_join`] — «человеческая» сериализация:
//!   кавычки добавляются только там, где без них слово разваливается
//!   (см. [`needs_quoting`]), и склейка argv в одну командную строку.
//! * [`split_quoted`] — разбор командной строки в argv: одинарные и двойные
//!   кавычки, обратные слэши, конкатенация соседних слов (`a'b'c` → `abc`),
//!   продолжение строки `\<newline>`. Незакрытая кавычка или слэш в конце —
//!   [`EscapeError`] с байтовой позицией проблемы.
//! * [`unquote`] — обратная операция к [`quote`] для ОДНОГО слова: снять
//!   кавычки и экраны. Лишние слова после первого — ошибка.
//! * [`make_safe_assignment`] и [`extract_assignments`] — работа с
//!   присваиваниями окружения: сборка `VAR='значение'` и выделение ведущих
//!   `VAR=value`-пар из команды (`A=1 B=2 cmd ...` → пары + остаток команды).
//!
//! Все функции — чистые (только `std`), без паник и без обращений к
//! окружению: их безопасно звать из горячих путей approve-UI и исполнителя.
//!
//! Позиции в [`EscapeError`] — БАЙТОВЫЕ смещения от начала входной строки
//! (как индексы `str`), так что их можно использовать для подсветки места
//! ошибки прямо в исходной команде.

use std::fmt;

// ---------------------------------------------------------------------------
// Ошибки разбора
// ---------------------------------------------------------------------------

/// Ошибка разбора shell-строки.
///
/// Все позиции — байтовые смещения от начала входной строки.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EscapeError {
    /// Кавычка открыта, но не закрыта до конца строки.
    ///
    /// `quote` — какая именно кавычка (`'` или `"`), `pos` — её позиция.
    UnterminatedQuote {
        /// Вид незакрытой кавычки: `'` или `"`.
        quote: char,
        /// Байтовое смещение открывающей кавычки.
        pos: usize,
    },
    /// Обратный слэш — последний символ строки: экранировать нечего.
    ///
    /// `pos` — позиция этого слэша.
    DanglingEscape {
        /// Байтовое смещение висячего слэша.
        pos: usize,
    },
    /// После конца слова в [`unquote`] остались непробельные символы:
    /// вход — не одно shell-слово. `pos` — начало «хвоста».
    TrailingInput {
        /// Байтовое смещение первого лишнего символа.
        pos: usize,
    },
}

impl fmt::Display for EscapeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EscapeError::UnterminatedQuote { quote, pos } => {
                write!(f, "незакрытая кавычка {quote:?} (байт {pos})")
            }
            EscapeError::DanglingEscape { pos } => {
                write!(f, "обратный слэш в конце строки (байт {pos})")
            }
            EscapeError::TrailingInput { pos } => {
                write!(f, "неожиданные символы после конца слова (байт {pos})")
            }
        }
    }
}

impl std::error::Error for EscapeError {}

// ---------------------------------------------------------------------------
// Экранирование (сериализация argv → строка)
// ---------------------------------------------------------------------------

/// Экранирует один аргумент одинарными кавычками POSIX.
///
/// Внутри одинарных кавычек shell не делает НИКАКИХ подстановок, поэтому
/// приём «всё в кавычки» надёжен против инъекций. Единственный символ,
/// который нельзя поместить внутрь `'`...`'`, — сама кавычка; она
/// кодируется классической последовательностью `'\''` (закрыть кавычки,
/// дать экранированную кавычку, открыть заново):
///
/// ```text
/// don't  →  'don'\''t'
/// ```
///
/// Пустая строка превращается в `''` (валидный пустой аргумент).
pub fn quote(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('\'');
    for ch in arg.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// true, если `arg` без кавычек НЕ выживет в командной строке.
///
/// Безопасный набор — как в `shlex.quote` из Python: латиница, цифры и
/// `_@%+=:,./-`. Эти символы не являются метасимволами shell и не начинают
/// подстановок. Всё остальное (пробелы, кавычки, `$`, backticks, `;`, `&`,
/// `|`, `*`, `?`, `~`, `#`, не-ASCII и т.п.) требует кавычек, равно как и
/// пустая строка (без кавычек она просто исчезнет из argv).
pub fn needs_quoting(arg: &str) -> bool {
    arg.is_empty()
        || arg.bytes().any(|b| {
            !matches!(
                b,
                b'a'..=b'z'
                    | b'A'..=b'Z'
                    | b'0'..=b'9'
                    | b'_'
                    | b'@'
                    | b'%'
                    | b'+'
                    | b'='
                    | b':'
                    | b','
                    | b'.'
                    | b'/'
                    | b'-'
            )
        })
}

/// Как [`quote`], но без кавычек, когда слово безопасно ([`needs_quoting`]).
///
/// Даёт строки, читаемые человеком в approve-диалоге: `/tmp/x` остаётся
/// как есть, а `my dir` становится `'my dir'`.
pub fn quote_if_needed(arg: &str) -> String {
    if needs_quoting(arg) {
        quote(arg)
    } else {
        arg.to_string()
    }
}

/// Склеивает argv в одну командную строку через пробел.
///
/// Каждый аргумент проходит через [`quote`], поэтому результат разбирается
/// [`split_quoted`] (и любым POSIX-shell) ровно в исходный список — это
/// основа roundtrip-канала «модель → человек → исполнитель».
pub fn quote_join<S: AsRef<str>>(args: &[S]) -> String {
    args.iter()
        .map(|a| quote(a.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// Присваивания окружения (сериализация)
// ---------------------------------------------------------------------------

/// true, если `name` — корректное имя переменной окружения по POSIX:
/// латинская буква или `_`, далее буквы, цифры и `_` (`[A-Za-z_][A-Za-z0-9_]*`).
pub fn is_valid_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return false;
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Собирает присваивание `VAR=значение`, безопасное для вставки в команду.
///
/// Значение проходит через [`quote_if_needed`]: `make_safe_assignment("A", "1")`
/// даёт `A=1`, а `make_safe_assignment("A", "x y")` — `A='x y'`.
///
/// Имя переменной НЕ валидируется: если оно приходит из недоверенного
/// источника, проверяйте его [`is_valid_var_name`] заранее.
pub fn make_safe_assignment(var: &str, val: &str) -> String {
    format!("{var}={}", quote_if_needed(val))
}

// ---------------------------------------------------------------------------
// Токенизатор (разбор строки → argv)
// ---------------------------------------------------------------------------

/// Одно разобранное слово: текст после снятия кавычек/экранов и его
/// байтовый диапазон `[start, end)` в исходной строке (диапазон нужен
/// [`extract_assignments`], чтобы вернуть остаток команды без пересборки).
struct Token {
    /// Слово после снятия кавычек и обратных слэшей.
    decoded: String,
    /// Байтовое смещение начала слова в исходной строке.
    start: usize,
    /// Байтовое смещение конца слова (указывает на разделитель или конец).
    end: usize,
}

/// Посимвольный токенизатор POSIX-слова с байтовым курсором.
///
/// Правила разбора:
/// * разделитель слов — любой неэкранированный whitespace;
/// * `'`...`'` — литеральный текст (внутри нет экранов, слэш не особый);
/// * `"`...`"` — текст, где `\` экранирует только `"`, `\`, `$`, `` ` `` и
///   перевод строки; прочие `\x` остаются двумя символами;
/// * `\x` вне кавычек — литеральный `x`, а `\<newline>` — продолжение
///   строки (удаляется);
/// * соседние куски без разделителя конкатенируются: `a'b'c"d"` → `abcd`.
struct Tokenizer<'a> {
    /// Исходная строка.
    src: &'a str,
    /// Байтовый курсор (всегда на границе символа).
    pos: usize,
    /// Начало последнего прочитанного токена (включая неудачный — нужно
    /// для консервативного остатка в [`extract_assignments`]).
    token_start: usize,
}

impl<'a> Tokenizer<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src,
            pos: 0,
            token_start: 0,
        }
    }

    /// Текущий символ без продвижения курсора.
    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    /// Считывает символ и продвигает курсор.
    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }

    /// Пропускает разделители между словами.
    fn skip_ws(&mut self) {
        while let Some(ch) = self.peek() {
            if !ch.is_whitespace() {
                break;
            }
            self.pos += ch.len_utf8();
        }
    }

    /// Следующее слово, либо `None` в конце строки.
    fn next_token(&mut self) -> Result<Option<Token>, EscapeError> {
        self.skip_ws();
        if self.peek().is_none() {
            return Ok(None);
        }
        let start = self.pos;
        self.token_start = start;
        let mut decoded = String::new();
        while let Some(ch) = self.peek() {
            match ch {
                c if c.is_whitespace() => break,
                '\'' => {
                    let quote_pos = self.pos;
                    self.pos += 1; // '\'' — ASCII
                    self.read_single_quoted(quote_pos, &mut decoded)?;
                }
                '"' => {
                    let quote_pos = self.pos;
                    self.pos += 1; // '"' — ASCII
                    self.read_double_quoted(quote_pos, &mut decoded)?;
                }
                '\\' => {
                    let esc_pos = self.pos;
                    self.pos += 1; // '\\' — ASCII
                    match self.bump() {
                        None => return Err(EscapeError::DanglingEscape { pos: esc_pos }),
                        // Продолжение строки: слэш + перевод строки удаляются.
                        Some('\n') => {}
                        Some(c) => decoded.push(c),
                    }
                }
                c => {
                    decoded.push(c);
                    self.pos += c.len_utf8();
                }
            }
        }
        Ok(Some(Token {
            decoded,
            start,
            end: self.pos,
        }))
    }

    /// Тело `'`...`'`: всё литерально до ближайшей закрывающей кавычки.
    fn read_single_quoted(&mut self, quote_pos: usize, out: &mut String) -> Result<(), EscapeError> {
        while let Some(ch) = self.bump() {
            if ch == '\'' {
                return Ok(());
            }
            out.push(ch);
        }
        Err(EscapeError::UnterminatedQuote {
            quote: '\'',
            pos: quote_pos,
        })
    }

    /// Тело `"`...`"`: `\` экранирует только `"`, `\`, `$`, `` ` `` и
    /// перевод строки; остальные `\x` сохраняются как есть.
    fn read_double_quoted(&mut self, quote_pos: usize, out: &mut String) -> Result<(), EscapeError> {
        while let Some(ch) = self.bump() {
            match ch {
                '"' => return Ok(()),
                '\\' => match self.bump() {
                    // Слэш в конце незакрытой двойной кавычки — это всё ещё
                    // незакрытая кавычка, а не висячий экран.
                    None => {
                        return Err(EscapeError::UnterminatedQuote {
                            quote: '"',
                            pos: quote_pos,
                        })
                    }
                    Some('\n') => {}
                    Some(c @ ('"' | '\\' | '$' | '`')) => out.push(c),
                    Some(c) => {
                        out.push('\\');
                        out.push(c);
                    }
                },
                c => out.push(c),
            }
        }
        Err(EscapeError::UnterminatedQuote {
            quote: '"',
            pos: quote_pos,
        })
    }
}

// ---------------------------------------------------------------------------
// Разбор (строка → argv)
// ---------------------------------------------------------------------------

/// Разбирает командную строку в argv по правилам POSIX-слов.
///
/// Обратна к [`quote_join`]: `split_quoted(&quote_join(&argv)) == argv`.
/// Незакрытая кавычка и слэш в конце строки — ошибки с позицией
/// ([`EscapeError`]); пустая и чисто пробельная строка дают пустой argv.
pub fn split_quoted(cmd: &str) -> Result<Vec<String>, EscapeError> {
    let mut tokenizer = Tokenizer::new(cmd);
    let mut argv = Vec::new();
    while let Some(tok) = tokenizer.next_token()? {
        argv.push(tok.decoded);
    }
    Ok(argv)
}

/// Снимает кавычки/экраны с ОДНОГО shell-слова (обратная к [`quote`]).
///
/// Вход должен быть ровно одним словом: `unquote("'a b'")` → `a b`, но
/// `unquote("a b")` — [`EscapeError::TrailingInput`], потому что разбора
/// списка слов здесь не обещают (для этого есть [`split_quoted`]). Пустая
/// и чисто пробельная строка трактуются как пустое слово.
pub fn unquote(arg: &str) -> Result<String, EscapeError> {
    let mut tokenizer = Tokenizer::new(arg);
    let Some(tok) = tokenizer.next_token()? else {
        return Ok(String::new());
    };
    tokenizer.skip_ws();
    if tokenizer.peek().is_some() {
        return Err(EscapeError::TrailingInput { pos: tokenizer.pos });
    }
    Ok(tok.decoded)
}

// ---------------------------------------------------------------------------
// Присваивания окружения (разбор)
// ---------------------------------------------------------------------------

/// Если СЫРОЙ текст слова `raw` — присваивание `NAME=value`, вернуть пару
/// `(имя, значение)`. Значение берётся из разобранного текста `decoded`
/// (имя и `=` — ASCII, переживают разбор байт в байт). Экранированный `=`
/// (`A\=x`) и кавычки в имени (`'A'=x`) присваиванием не считаются — как
/// и в shell, где имя должно быть записано литерально.
fn split_assignment(raw: &str, decoded: &str) -> Option<(String, String)> {
    let eq = raw.find('=')?;
    let name = &raw[..eq];
    if !is_valid_var_name(name) {
        return None;
    }
    // Имя и '=' — ASCII и идут в начале decoded, срез на границе безопасен.
    Some((name.to_string(), decoded[name.len() + 1..].to_string()))
}

/// Выделяет ведущие присваивания окружения из командной строки.
///
/// Возвращает пару `(присваивания, остаток)`: ведущие слова вида
/// `NAME=value` (значения уже разэкранированы, `A='x y'` → `x y`) и
/// исходный текст остальной команды без обрамляющих пробелов — его можно
/// показывать/исполнять дальше без пересборки. Примеры:
///
/// ```text
/// "A=1 B='x y' ls -la"  →  ([("A","1"), ("B","x y")], "ls -la")
/// "A=1 B=2"             →  ([("A","1"), ("B","2")], "")
/// "ls -la"              →  ([], "ls -la")
/// ```
///
/// При синтаксической ошибке разбора (незакрытая кавычка) функция не
/// падает: уже найденные пары сохраняются, а остаток начинается с начала
/// битого слова — так вызывающий видит ровно то, что удалось разобрать.
pub fn extract_assignments(cmd: &str) -> (Vec<(String, String)>, String) {
    let mut tokenizer = Tokenizer::new(cmd);
    let mut pairs = Vec::new();
    let rest_start = loop {
        match tokenizer.next_token() {
            Ok(Some(tok)) => match split_assignment(&cmd[tok.start..tok.end], &tok.decoded) {
                Some(pair) => pairs.push(pair),
                None => break tok.start,
            },
            Ok(None) => break cmd.len(),
            // Битое слово в остаток целиком: доверять ему нельзя.
            Err(_) => break tokenizer.token_start,
        }
    };
    (pairs, cmd[rest_start..].trim_end().to_string())
}

// ---------------------------------------------------------------------------
// Тесты
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- quote ------------------------------------------------------------

    #[test]
    fn quote_empty_is_two_quotes() {
        // Пустой аргумент без кавычек исчез бы из argv — кодируем как ''.
        assert_eq!(quote(""), "''");
    }

    #[test]
    fn quote_simple_word_wraps_verbatim() {
        assert_eq!(quote("abc"), "'abc'");
        // Метасимволы внутри одинарных кавычек — просто текст.
        assert_eq!(quote("$HOME; rm -rf /"), "'$HOME; rm -rf /'");
    }

    #[test]
    fn quote_single_quote_uses_posix_idiom() {
        // Классический приём: закрыть, дать \' , открыть заново.
        assert_eq!(quote("don't"), "'don'\\''t'");
        assert_eq!(quote("'"), "''\\'''");
        assert_eq!(quote("''"), "''\\'''\\'''");
    }

    #[test]
    fn quote_join_glues_with_spaces() {
        assert_eq!(quote_join(&["a b", "c"]), "'a b' 'c'");
        // Обобщённость по AsRef<str>: работает и с Vec<String>.
        let owned = vec![String::from("x y"), String::from("z")];
        assert_eq!(quote_join(&owned), "'x y' 'z'");
        // Пустой список — пустая строка.
        let none: [&str; 0] = [];
        assert_eq!(quote_join(&none), "");
    }

    #[test]
    fn quote_if_needed_quotes_only_unsafe() {
        assert_eq!(quote_if_needed("/tmp/x"), "/tmp/x");
        assert_eq!(quote_if_needed("my dir"), "'my dir'");
        assert_eq!(quote_if_needed(""), "''");
    }

    // ---- needs_quoting ----------------------------------------------------

    #[test]
    fn needs_quoting_safe_set_stays_bare() {
        // Точность безопасного набора: ни один «тихий» символ не должен
        // заставлять брать слово в кавычки.
        for safe in [
            "abc", "A_B-9", "a/b/c.txt", "user@host", "a=b", "a:b,c+d%e", "-x", "/", ".", "..",
            "x86_64",
        ] {
            assert!(!needs_quoting(safe), "должно оставаться голым: {safe:?}");
        }
    }

    #[test]
    fn needs_quoting_metachars_and_unicode_require_quotes() {
        for unsafe_ in [
            "", "a b", "a'b", "a\"b", "$x", "`x`", "a;b", "a&b", "a|b", "a<b", "a>b", "a(b",
            "a)b", "a*b", "a?b", "a#b", "a!b", "a~b", "a\nb", "a\tb", "a\\b", "юникод", "a🚀b",
        ] {
            assert!(needs_quoting(unsafe_), "должно требовать кавычек: {unsafe_:?}");
        }
    }

    // ---- roundtrip quote → split_quoted -----------------------------------

    #[test]
    fn roundtrip_quote_split_on_20_cases() {
        // Пробелы, кавычки, $, backticks, переводы строк, юникод, пустые.
        let cases = [
            "",
            "simple",
            "with space",
            "trailing ",
            " leading",
            "single'quote",
            "don't",
            "'",
            "double\"quote",
            "quote'mix\"ed",
            "$HOME",
            "${PATH}",
            "`whoami`",
            "$(id -u)",
            "line1\nline2",
            "tab\there",
            "юникод/файл 🚀.txt",
            "back\\slash",
            "glob*?[x]",
            "semi;&|<>()#!",
        ];
        assert_eq!(cases.len(), 20);
        for case in cases {
            let got = split_quoted(&quote(case)).unwrap();
            assert_eq!(got, vec![case], "roundtrip не сошёлся для {case:?}");
        }
    }

    #[test]
    fn roundtrip_multi_arg_list() {
        let argv = ["ls", "-la", "/tmp/my dir", "", "a'b", "x=y"];
        let line = quote_join(&argv);
        assert_eq!(split_quoted(&line).unwrap(), argv);
    }

    #[test]
    fn roundtrip_through_real_sh() {
        // Контроль по «земной правде»: строку, собранную quote_join,
        // разбирает настоящий POSIX-shell, и argv совпадает с исходным.
        if !std::path::Path::new("/bin/sh").exists() {
            eprintln!("пропуск: /bin/sh недоступен");
            return;
        }
        let cases = [
            "",
            "a b",
            "don't",
            "$HOME",
            "`id`",
            "line1\nline2",
            "юникод 🚀",
            "back\\slash",
        ];
        let script = format!(
            "set -- {}; for a in \"$@\"; do printf '%s\\0' \"$a\"; done",
            quote_join(&cases)
        );
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            .output()
            .unwrap();
        assert!(out.status.success(), "sh вернул ошибку: {out:?}");
        // stdout — значения через NUL; последний кусок после финального NUL пуст.
        let mut pieces: Vec<&[u8]> = out.stdout.split(|b| *b == 0).collect();
        pieces.pop();
        let got: Vec<&[u8]> = pieces;
        let want: Vec<&[u8]> = cases.iter().map(|s| s.as_bytes()).collect();
        assert_eq!(got, want);
    }

    // ---- split_quoted -----------------------------------------------------

    #[test]
    fn split_quoted_basic_whitespace_handling() {
        assert_eq!(split_quoted("ls -la /tmp").unwrap(), ["ls", "-la", "/tmp"]);
        // Табуляции и переводы строк — такие же разделители.
        assert_eq!(split_quoted("  a\tb\nc  ").unwrap(), ["a", "b", "c"]);
        assert_eq!(split_quoted("").unwrap(), Vec::<String>::new());
        assert_eq!(split_quoted("   \n\t ").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn split_quoted_single_quotes_are_literal() {
        // Внутри одинарных кавычек нет подстановок и экранов.
        assert_eq!(
            split_quoted("echo '$HOME `x` \\'").unwrap(),
            ["echo", "$HOME `x` \\"]
        );
        // Пустые кавычки — пустой, но СУЩЕСТВУЮЩИЙ аргумент.
        assert_eq!(split_quoted("''").unwrap(), [""]);
    }

    #[test]
    fn split_quoted_double_quotes_and_escapes() {
        assert_eq!(split_quoted("say \"it's fine\"").unwrap(), ["say", "it's fine"]);
        // Внутри двойных кавычек слэш экранирует ", \, $ и `.
        assert_eq!(split_quoted("\"a\\\"b\\\\c\\$d\\`e\"").unwrap(), ["a\"b\\c$d`e"]);
        // Прочий слэш в двойных кавычках остаётся литеральным.
        assert_eq!(split_quoted("\"a\\nb\"").unwrap(), ["a\\nb"]);
    }

    #[test]
    fn split_quoted_backslash_outside_quotes() {
        assert_eq!(split_quoted("a\\ b").unwrap(), ["a b"]);
        assert_eq!(split_quoted("\\\\").unwrap(), ["\\"]);
        assert_eq!(split_quoted("\\$HOME").unwrap(), ["$HOME"]);
        // Слэш + перевод строки — продолжение строки, удаляется.
        assert_eq!(split_quoted("a\\\nb").unwrap(), ["ab"]);
    }

    #[test]
    fn split_quoted_concatenates_adjacent_parts() {
        // Соседние куски без разделителя склеиваются в одно слово.
        assert_eq!(split_quoted("a'b'c\"d\"e").unwrap(), ["abcde"]);
        assert_eq!(split_quoted("''\"\"").unwrap(), [""]);
        assert_eq!(split_quoted("--flag='v 1'").unwrap(), ["--flag=v 1"]);
    }

    #[test]
    fn split_quoted_unterminated_quotes_report_position() {
        // Позиция — байтовое смещение ОТКРЫВАЮЩЕЙ кавычки.
        let err = split_quoted("echo 'abc").unwrap_err();
        assert_eq!(err, EscapeError::UnterminatedQuote { quote: '\'', pos: 5 });
        let err = split_quoted("x \"ab").unwrap_err();
        assert_eq!(err, EscapeError::UnterminatedQuote { quote: '"', pos: 2 });
        // Слэш в конце внутри двойных кавычек — та же незакрытая кавычка.
        let err = split_quoted("\"ab\\").unwrap_err();
        assert_eq!(err, EscapeError::UnterminatedQuote { quote: '"', pos: 0 });
    }

    #[test]
    fn split_quoted_dangling_escape_reports_position() {
        let err = split_quoted("abc\\").unwrap_err();
        assert_eq!(err, EscapeError::DanglingEscape { pos: 3 });
    }

    // ---- unquote ----------------------------------------------------------

    #[test]
    fn unquote_strips_quotes_and_escapes() {
        assert_eq!(unquote("abc").unwrap(), "abc");
        assert_eq!(unquote("'a b'").unwrap(), "a b");
        assert_eq!(unquote("\"x\\\"y\"").unwrap(), "x\"y");
        assert_eq!(unquote("'a'\"b\"").unwrap(), "ab");
        assert_eq!(unquote("\\$").unwrap(), "$");
        // Пустая строка — легальное пустое слово.
        assert_eq!(unquote("").unwrap(), "");
        assert_eq!(unquote("   ").unwrap(), "");
    }

    #[test]
    fn unquote_rejects_trailing_words_with_position() {
        assert_eq!(unquote("a b").unwrap_err(), EscapeError::TrailingInput { pos: 2 });
        assert_eq!(unquote("'a'  'b'").unwrap_err(), EscapeError::TrailingInput { pos: 5 });
    }

    #[test]
    fn unquote_propagates_unterminated_quote() {
        assert_eq!(
            unquote("'abc").unwrap_err(),
            EscapeError::UnterminatedQuote { quote: '\'', pos: 0 }
        );
    }

    // ---- ошибки: Display / Error -------------------------------------------

    #[test]
    fn escape_error_display_is_russian_and_error_impl() {
        let e = EscapeError::UnterminatedQuote { quote: '\'', pos: 5 };
        let text = e.to_string();
        assert!(text.contains("незакрытая кавычка"), "текст: {text}");
        assert!(text.contains('5'), "текст: {text}");
        // Тип реализует std::error::Error — можно воткнуть в anyhow.
        let dyn_err: &dyn std::error::Error = &e;
        assert!(dyn_err.source().is_none());
        let e = EscapeError::DanglingEscape { pos: 3 };
        assert!(e.to_string().contains("слэш"));
        let e = EscapeError::TrailingInput { pos: 2 };
        assert!(e.to_string().contains("после конца слова"));
    }

    // ---- имена переменных и сборка присваиваний -----------------------------

    #[test]
    fn is_valid_var_name_posix_rules() {
        for ok in ["A", "_X", "A1_", "_", "PATH", "x"] {
            assert!(is_valid_var_name(ok), "валидно: {ok}");
        }
        for bad in ["", "1A", "A-B", "A B", "A.B", "9", "A'B"] {
            assert!(!is_valid_var_name(bad), "невалидно: {bad}");
        }
    }

    #[test]
    fn make_safe_assignment_quotes_value_only_when_needed() {
        assert_eq!(make_safe_assignment("A", "1"), "A=1");
        assert_eq!(make_safe_assignment("PATH", "/a:/b"), "PATH=/a:/b");
        assert_eq!(make_safe_assignment("A", "x y"), "A='x y'");
        assert_eq!(make_safe_assignment("V", "don't"), "V='don'\\''t'");
        assert_eq!(make_safe_assignment("A", ""), "A=''");
    }

    // ---- extract_assignments ------------------------------------------------

    #[test]
    fn extract_assignments_basic_prefix() {
        let (pairs, rest) = extract_assignments("A=1 B=two echo hi");
        assert_eq!(pairs, [("A".to_string(), "1".to_string()), ("B".to_string(), "two".to_string())]);
        assert_eq!(rest, "echo hi");
    }

    #[test]
    fn extract_assignments_decodes_quoted_values() {
        let (pairs, rest) = extract_assignments("A='x y' B=\"$v\" run");
        assert_eq!(pairs, [("A".to_string(), "x y".to_string()), ("B".to_string(), "$v".to_string())]);
        assert_eq!(rest, "run");
    }

    #[test]
    fn extract_assignments_no_prefix_and_trim() {
        let (pairs, rest) = extract_assignments("  ls -la  ");
        assert!(pairs.is_empty());
        assert_eq!(rest, "ls -la");
        // export — это команда, а не присваивание.
        let (pairs, rest) = extract_assignments("export A=1");
        assert!(pairs.is_empty());
        assert_eq!(rest, "export A=1");
    }

    #[test]
    fn extract_assignments_all_consumed_leaves_empty_rest() {
        let (pairs, rest) = extract_assignments("A=1 B=2");
        assert_eq!(pairs.len(), 2);
        assert_eq!(rest, "");
    }

    #[test]
    fn extract_assignments_rejects_invalid_names() {
        // Слово с = , но не NAME=value — просто часть команды.
        let (pairs, rest) = extract_assignments("1A=x cmd");
        assert!(pairs.is_empty());
        assert_eq!(rest, "1A=x cmd");
        let (pairs, rest) = extract_assignments("=x cmd");
        assert!(pairs.is_empty());
        assert_eq!(rest, "=x cmd");
        // Экранированный = не заводит присваивание.
        let (pairs, rest) = extract_assignments("A\\=1 cmd");
        assert!(pairs.is_empty());
        assert_eq!(rest, "A\\=1 cmd");
    }

    #[test]
    fn extract_assignments_empty_and_equals_values() {
        let (pairs, rest) = extract_assignments("A= B==x c");
        assert_eq!(pairs, [("A".to_string(), String::new()), ("B".to_string(), "=x".to_string())]);
        assert_eq!(rest, "c");
    }

    #[test]
    fn extract_assignments_unterminated_quote_falls_back_conservatively() {
        // Уже разобранные пары сохраняются, битый хвост уходит в остаток.
        let (pairs, rest) = extract_assignments("A=1 B='oops");
        assert_eq!(pairs, [("A".to_string(), "1".to_string())]);
        assert_eq!(rest, "B='oops");
        // Ошибка в первом же слове — присваиваний не гарантируем.
        let (pairs, rest) = extract_assignments("A='oops");
        assert!(pairs.is_empty());
        assert_eq!(rest, "A='oops");
    }
}
