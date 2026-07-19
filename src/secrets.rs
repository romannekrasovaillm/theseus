//! Маскировка секретов в выводе инструментов и транскриптах агента.
//!
//! По образцу `codex-rs/secrets` (sanitizer): best-effort замена известных
//! классов секретов на маркеры вида `…[REDACTED <имя-правила>]`. Применяется
//! перед записью вывода инструментов в транскрипт/аудит, чтобы ключи, токены
//! и пароли не оседали в логах и истории сессий.
//!
//! Модель:
//!
//! - [`SecretRule`] — одно правило: имя, скомпилированный regex и
//!   `keep_prefix` (сколько символов от начала совпадения оставить открытыми
//!   для диагностики);
//! - [`builtin_rules`] — встроенный набор: PEM-блоки приватных ключей,
//!   `DEEPSEEK_API_KEY=...`, пароли в URL (`://user:pass@`), Bearer-токены,
//!   OpenAI `sk-...`, AWS `AKIA...`, hex-токены 32+ символов;
//! - [`Redactor`] — применяет правила к тексту: [`Redactor::redact`]
//!   (маскировка), [`Redactor::scan`] (координаты находок без самого секрета),
//!   [`Redactor::is_clean`] (быстрая проверка), [`Redactor::from_env_keys`]
//!   (точное совпадение значений перечисленных env-переменных).
//!
//! Правила применяются последовательно в порядке списка: более специфичные
//! (и потенциально пересекающиеся с общими) идут первыми — PEM раньше hex,
//! `DEEPSEEK_API_KEY=sk-...` раньше общего правила OpenAI. Уже вставленные
//! маркеры не содержат символов, которые могли бы повторно сматчиться
//! последующими правилами, поэтому [`Redactor::redact`] идемпотентен.
//!
//! Известные ограничения (best effort, как и в эталоне):
//!
//! - правило hex-токенов грубое: длинные git-SHA (40 hex) и хэши тоже
//!   маскируются — сознательная цена за перехват настоящих токенов;
//! - пароль в URL маскируется вместе с именем пользователя (фиксированный
//!   `keep_prefix` не может сохранить префикс переменной длины);
//! - встроенные шаблоны компилируются при первом обращении и паникуют при
//!   опечатке в шаблоне — это ошибка программиста, а не входных данных;
//!   покрыто тестом `builtin_rules_compile`.

#![forbid(unsafe_code)]

use std::cmp::Reverse;
use std::ops::Range;

use regex::Captures;
use regex::Regex;

/// Минимальная длина значения env-переменной (в символах), чтобы оно
/// считалось секретом и попадало в правила [`Redactor::from_env_keys`].
/// Более короткие значения слишком часто встречаются в обычном тексте
/// (`true`, `0`, `/tmp`), маскировать их — сплошные ложные срабатывания.
const MIN_ENV_VALUE_LEN: usize = 8;

/// Правило маскировки одного класса секретов.
///
/// Совпадение шаблона заменяется на `<первые keep_prefix символов
/// совпадения>…[REDACTED <name>]`. Открытый префикс нужен для диагностики
/// (видно тип ключа и его начало), но не должен нести энтропии секрета.
#[derive(Clone, Debug)]
pub struct SecretRule {
    /// Имя правила — попадает в маркер замены и в [`SecretHit`].
    pub name: String,
    /// Скомпилированный шаблон совпадения.
    pub regex: Regex,
    /// Сколько символов (не байт) от начала совпадения оставить открытыми.
    /// Если больше длины совпадения — совпадение сохраняется целиком,
    /// маркер всё равно добавляется.
    pub keep_prefix: usize,
}

impl SecretRule {
    /// Создаёт правило из regex-шаблона.
    ///
    /// Ошибка возможна только при невалидном шаблоне — для пользовательских
    /// правил из конфига; встроенные шаблоны компилирует [`builtin_rules`].
    pub fn new(
        name: impl Into<String>,
        pattern: &str,
        keep_prefix: usize,
    ) -> Result<Self, regex::Error> {
        Ok(Self {
            name: name.into(),
            regex: Regex::new(pattern)?,
            keep_prefix,
        })
    }

    /// Строка-замена для одного совпадения: открытый префикс + маркер.
    ///
    /// Префикс считается в символах через `chars().take()`, поэтому срез
    /// всегда на границе UTF-8 (многобайтовые значения env не роняют поток).
    fn replacement(&self, matched: &str) -> String {
        let prefix: String = matched.chars().take(self.keep_prefix).collect();
        let name = &self.name;
        format!("{prefix}…[REDACTED {name}]")
    }
}

/// Найденный секрет: какое правило сработало и где.
///
/// Сам текст секрета сюда сознательно НЕ попадает — только имя правила и
/// координаты; попадание можно безопасно писать в аудит и телеметрию.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretHit {
    /// Имя сработавшего правила (см. [`SecretRule::name`]).
    pub rule: String,
    /// Байтовый диапазон совпадения в исходном тексте.
    pub span: Range<usize>,
}

/// Встроенный набор правил маскировки.
///
/// Порядок в возвращаемом векторе — порядок применения в
/// [`Redactor::redact`]; он подобран так, чтобы специфичные шаблоны
/// срабатывали раньше общих:
///
/// 1. `pem-private-key` — блоки `-----BEGIN ... PRIVATE KEY-----...`;
/// 2. `deepseek-api-key` — присваивания `DEEPSEEK_API_KEY=...`;
/// 3. `url-password` — `://user:pass@` (маскируется весь userinfo);
/// 4. `bearer-token` — `Bearer <токен>` (регистронезависимо);
/// 5. `openai-api-key` — `sk-...` (20+ символов ключа);
/// 6. `aws-access-key-id` — `AKIA...` (16 символов после префикса);
/// 7. `hex-token` — hex-строки 32+ символов.
pub fn builtin_rules() -> Vec<SecretRule> {
    vec![
        // Многострочный блок целиком; `(?s)` — точка захватывает переводы строк.
        compile_builtin(
            "pem-private-key",
            r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----",
            0,
        ),
        // Значение после `DEEPSEEK_API_KEY=`, опционально в кавычках.
        // keep_prefix = длина строки "DEEPSEEK_API_KEY=" (17 символов).
        compile_builtin(
            "deepseek-api-key",
            r#"\bDEEPSEEK_API_KEY=["']?[A-Za-z0-9._\-]{8,}"#,
            17,
        ),
        // userinfo в URL: схема остаётся, user:pass маскируется целиком
        // (пароль — от 4 символов, чтобы не цеплять экзотику вида `://a:b@`).
        compile_builtin("url-password", r"://[^/\s:@]{1,64}:[^@\s/]{4,}@", 3),
        // keep_prefix = "Bearer" + один пробельный символ — токен не протекает
        // даже при вариациях регистра и количества пробелов.
        compile_builtin("bearer-token", r"(?i)\bBearer\s+[A-Za-z0-9._\-]{16,}", 7),
        // keep_prefix = "sk-" — постоянный маркер формата, энтропии не несёт.
        compile_builtin("openai-api-key", r"\bsk-[A-Za-z0-9_-]{20,}", 3),
        // keep_prefix = "AKIA" — постоянный префикс формата AWS.
        compile_builtin("aws-access-key-id", r"\bAKIA[0-9A-Z]{16}\b", 4),
        // keep_prefix = 8 hex-символов — как короткий git-хэш.
        compile_builtin("hex-token", r"\b[0-9a-fA-F]{32,}\b", 8),
    ]
}

/// Компилирует статический шаблон встроенного правила.
///
/// Паника при невалидном шаблоне — это ошибка программиста в исходнике,
/// а не входных данных; шаблоны фиксированы, тест `builtin_rules_compile`
/// гарантирует, что паника недостижима.
fn compile_builtin(name: &str, pattern: &str, keep_prefix: usize) -> SecretRule {
    match SecretRule::new(name, pattern, keep_prefix) {
        Ok(rule) => rule,
        Err(err) => panic!("невалидный встроенный шаблон `{name}`: {err}"),
    }
}

/// Применяет набор [`SecretRule`] к тексту: маскировка и поиск секретов.
///
/// Дешёвый в клонировании (regex делится внутренним кэшем), потокобезопасен
/// (`Regex: Send + Sync`) — один `Redactor` можно разделять между потоками
/// инструментов.
#[derive(Clone, Debug)]
pub struct Redactor {
    /// Правила в порядке применения.
    rules: Vec<SecretRule>,
}

impl Redactor {
    /// Создаёт редактор из явного списка правил.
    ///
    /// Порядок правил — порядок применения: при пересечениях выигрывает
    /// более раннее правило.
    pub fn new(rules: Vec<SecretRule>) -> Self {
        Self { rules }
    }

    /// Редактор со встроенным набором [`builtin_rules`].
    pub fn with_builtin_rules() -> Self {
        Self::new(builtin_rules())
    }

    /// Текущий набор правил (в порядке применения).
    pub fn rules(&self) -> &[SecretRule] {
        &self.rules
    }

    /// Редактор со встроенными правилами плюс точное совпадение значений
    /// перечисленных env-переменных.
    ///
    /// Для каждого имени из `keys` читается значение переменной окружения;
    /// если оно существует и не короче 8 символов (`MIN_ENV_VALUE_LEN`),
    /// добавляется правило `env:<ИМЯ>`, маскирующее каждое литеральное
    /// вхождение значения где бы ни встретилось (спецсимволы regex
    /// экранируются через `regex::escape`, совпадение — точная подстрока).
    /// Отсутствующие и короткие значения молча пропускаются. Правила env
    /// сортируются по убыванию длины значения: при вложенности значений
    /// более длинное маскируется первым. `keep_prefix` у env-правил нулевой:
    /// любой открытый кусок произвольного значения — уже утечка.
    pub fn from_env_keys(keys: &[&str]) -> Self {
        let mut valued: Vec<(String, String)> = Vec::new();
        for &key in keys {
            let Ok(value) = std::env::var(key) else {
                continue;
            };
            if value.chars().count() >= MIN_ENV_VALUE_LEN {
                valued.push((key.to_string(), value));
            }
        }
        valued.sort_by_key(|(_, value)| Reverse(value.len()));

        let mut rules = builtin_rules();
        for (key, value) in valued {
            // Из экранированного литерала невалидный regex получиться не может
            // (теоретически — только упираясь в лимит размера шаблона);
            // такое правило пропускаем, а не роняем процесс.
            if let Ok(rule) = SecretRule::new(format!("env:{key}"), &regex::escape(&value), 0) {
                rules.push(rule);
            }
        }
        Self::new(rules)
    }

    /// Маскирует все секреты в `text`, возвращая новую строку.
    ///
    /// Правила применяются последовательно; операция идемпотентна
    /// (`redact(redact(x)) == redact(x)`), т.к. маркеры замены не матчатся
    /// ни одним из встроенных шаблонов.
    pub fn redact(&self, text: &str) -> String {
        let mut out = text.to_string();
        for rule in &self.rules {
            out = rule
                .regex
                .replace_all(&out, |caps: &Captures<'_>| rule.replacement(&caps[0]))
                .into_owned();
        }
        out
    }

    /// Ищет все секреты в `text` и возвращает их координаты.
    ///
    /// Каждое правило прогоняется по исходному тексту независимо, поэтому
    /// пересекающиеся находки разных правил возвращаются обе. Список
    /// отсортирован по позиции в тексте. Сами значения секретов в
    /// результат не включаются — см. [`SecretHit`].
    pub fn scan(&self, text: &str) -> Vec<SecretHit> {
        let mut hits = Vec::new();
        for rule in &self.rules {
            hits.extend(rule.regex.find_iter(text).map(|m| SecretHit {
                rule: rule.name.clone(),
                span: m.range(),
            }));
        }
        hits.sort_by_key(|hit| (hit.span.start, hit.span.end));
        hits
    }

    /// `true`, если ни одно правило не сработало на `text`.
    ///
    /// Дешевле полного [`Redactor::scan`]: останавливается на первом
    /// совпадении любого правила.
    pub fn is_clean(&self, text: &str) -> bool {
        !self.rules.iter().any(|rule| rule.regex.is_match(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use std::time::Instant;

    /// OpenAI-подобный ключ: `sk-` + 41 символ тела.
    const OPENAI_KEY: &str = "sk-proj_abcd1234EFGH5678ijkl9012MNOP3456qrst";

    /// Канонический пример AWS access key id из документации AWS.
    const AWS_KEY: &str = "AKIAIOSFODNN7EXAMPLE";

    /// 64-символьный hex-токен (как SHA-256).
    const HEX_TOKEN: &str = "9f8e7d6c5b4a39281706f5e4d3c2b1a09f8e7d6c5b4a39281706f5e4d3c2b1a0";

    /// JWT-подобный Bearer-токен с точками и подчёркиваниями.
    const BEARER_TOKEN: &str =
        "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";

    /// Страж env-переменной: восстанавливает прежнее состояние при выходе
    /// из области (тесты ходят параллельно, чужие переменные не трогаем —
    /// только свои THESEUS_TEST_SECRETS_*).
    struct EnvGuard {
        name: &'static str,
        old: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(name: &'static str, value: &str) -> Self {
            let old = std::env::var_os(name);
            std::env::set_var(name, value);
            Self { name, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.old {
                Some(value) => std::env::set_var(self.name, value),
                None => std::env::remove_var(self.name),
            }
        }
    }

    #[test]
    fn builtin_rules_compile() {
        // Сама сборка набора прогоняет все шаблоны через компилятор regex:
        // опечатка в шаблоне упала бы паникой здесь, а не в проде.
        let rules = builtin_rules();
        assert_eq!(rules.len(), 7);
        let names: Vec<&str> = rules.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "pem-private-key",
                "deepseek-api-key",
                "url-password",
                "bearer-token",
                "openai-api-key",
                "aws-access-key-id",
                "hex-token",
            ]
        );
    }

    #[test]
    fn redacts_openai_key_keeping_marker_prefix() {
        let redactor = Redactor::with_builtin_rules();
        let text = format!("ключ: {OPENAI_KEY}, дальше обычный текст");
        let out = redactor.redact(&text);
        // Открыт только постоянный префикс "sk-" (keep_prefix = 3).
        assert!(out.contains("sk-…[REDACTED openai-api-key]"), "вывод: {out}");
        assert!(!out.contains("proj_abcd1234"), "тело ключа протекло: {out}");
        assert!(redactor.is_clean(&out));
    }

    #[test]
    fn redacts_bearer_token_case_insensitively() {
        let redactor = Redactor::with_builtin_rules();
        let text = format!("Authorization: Bearer {BEARER_TOKEN}");
        let out = redactor.redact(&text);
        assert!(
            out.contains("Bearer …[REDACTED bearer-token]"),
            "вывод: {out}"
        );
        assert!(!out.contains("eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9"), "токен протек: {out}");

        // Регистр слова не важен, префикс сохраняется как в оригинале.
        let lower = redactor.redact(&format!("bearer {BEARER_TOKEN}"));
        assert!(lower.starts_with("bearer …[REDACTED bearer-token]"), "вывод: {lower}");
    }

    #[test]
    fn redacts_deepseek_api_key_assignment() {
        let redactor = Redactor::with_builtin_rules();
        let text = "export DEEPSEEK_API_KEY=dk-9f8e7d6c5b4a3210fedc && env";
        let out = redactor.redact(text);
        // keep_prefix = 17: открыто ровно "DEEPSEEK_API_KEY=".
        assert!(out.contains("DEEPSEEK_API_KEY=…[REDACTED deepseek-api-key]"), "вывод: {out}");
        assert!(!out.contains("9f8e7d6c5b4a3210"), "значение протекло: {out}");

        // Вариант в кавычках: значение тоже маскируется.
        let quoted = redactor.redact("DEEPSEEK_API_KEY=\"dk-12345678abcd\"");
        assert!(!quoted.contains("dk-12345678abcd"), "значение протекло: {quoted}");
    }

    #[test]
    fn redacts_url_password_with_userinfo() {
        let redactor = Redactor::with_builtin_rules();
        let text = "postgres://admin:Sup3rSecretPass@db.internal:5432/app";
        let out = redactor.redact(text);
        // keep_prefix = 3: открыто "://"; userinfo уходит целиком.
        assert_eq!(
            out,
            "postgres://…[REDACTED url-password]db.internal:5432/app"
        );

        // Без userinfo — не секрет.
        assert!(redactor.is_clean("https://example.com/path?q=1"));
        // Пароль короче 4 символов — правило не цепляет.
        assert!(redactor.is_clean("proto://u:a@host/"));
    }

    #[test]
    fn redacts_aws_access_key_id() {
        let redactor = Redactor::with_builtin_rules();
        let text = format!("aws_access_key_id = {AWS_KEY}");
        let out = redactor.redact(&text);
        assert!(out.contains("AKIA…[REDACTED aws-access-key-id]"), "вывод: {out}");
        assert!(!out.contains("IOSFODNN7EXAMPLE"), "тело ключа протекло: {out}");
    }

    #[test]
    fn redacts_pem_private_key_block() {
        let redactor = Redactor::with_builtin_rules();
        let pem = "-----BEGIN OPENSSH PRIVATE KEY-----\n\
             b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\n\
             QyNTUxOQAAACB3emVpY2VudGVuY2UAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n\
             -----END OPENSSH PRIVATE KEY-----";
        let text = format!("до\n{pem}\nпосле");
        let out = redactor.redact(&text);
        assert_eq!(out, "до\n…[REDACTED pem-private-key]\nпосле");
        assert!(!out.contains("b3BlbnNzaC1rZXktdjE"), "тело ключа протекло: {out}");

        // RSA-вариант заголовка тоже покрыт.
        let rsa = "-----BEGIN RSA PRIVATE KEY-----\nMIIabc\n-----END RSA PRIVATE KEY-----";
        assert!(!redactor.is_clean(rsa));
    }

    #[test]
    fn redacts_hex_token_keeping_short_hash_prefix() {
        let redactor = Redactor::with_builtin_rules();
        let text = format!("token={HEX_TOKEN}");
        let out = redactor.redact(&text);
        // keep_prefix = 8: как короткий git-хэш.
        assert_eq!(out, "token=9f8e7d6c…[REDACTED hex-token]");

        // 31 hex — уже не секрет.
        assert!(redactor.is_clean(&"a".repeat(31)));
    }

    #[test]
    fn keep_prefix_counts_chars_and_may_exceed_match() {
        // keep_prefix внутри совпадения.
        let rule = SecretRule::new("demo", r"tok-[0-9]{6}", 4).unwrap();
        let redactor = Redactor::new(vec![rule]);
        assert_eq!(redactor.redact("x tok-123456 y"), "x tok-…[REDACTED demo] y");

        // keep_prefix больше длины совпадения: совпадение сохраняется
        // целиком, маркер добавляется, паники на срезе нет.
        let wide = SecretRule::new("wide", r"tok-[0-9]{6}", 100).unwrap();
        let redactor = Redactor::new(vec![wide]);
        assert_eq!(
            redactor.redact("tok-123456"),
            "tok-123456…[REDACTED wide]"
        );
    }

    #[test]
    fn scan_reports_sorted_spans_without_secret_text() {
        let redactor = Redactor::with_builtin_rules();
        // hex-токен стоит в тексте раньше ключа OpenAI, хотя его правило в
        // списке позже — scan обязан отсортировать по позиции.
        let text = format!("{HEX_TOKEN} и {OPENAI_KEY}");
        let hits = redactor.scan(&text);
        assert_eq!(hits.len(), 2, "находки: {hits:?}");
        assert_eq!(hits[0].rule, "hex-token");
        assert_eq!(hits[1].rule, "openai-api-key");
        // Диапазоны указывают ровно на секреты в исходном тексте...
        assert_eq!(&text[hits[0].span.clone()], HEX_TOKEN);
        assert_eq!(&text[hits[1].span.clone()], OPENAI_KEY);
        // ...но сам SecretHit текст секрета не содержит.
        for hit in &hits {
            let debug = format!("{hit:?}");
            assert!(!debug.contains(HEX_TOKEN), "SecretHit протёк: {debug}");
            assert!(!debug.contains(OPENAI_KEY), "SecretHit протёк: {debug}");
        }
    }

    #[test]
    fn is_clean_matches_scan_emptiness() {
        let redactor = Redactor::with_builtin_rules();
        let dirty = format!("вот ключ {OPENAI_KEY} тут");
        assert!(!redactor.is_clean(&dirty));
        assert!(!redactor.scan(&dirty).is_empty());
        let clean = "совершенно обычная строка лога без ключей";
        assert!(redactor.is_clean(clean));
        assert!(redactor.scan(clean).is_empty());
    }

    #[test]
    fn env_values_are_masked_wherever_they_occur() {
        let _guard = EnvGuard::set(
            "THESEUS_TEST_SECRETS_ALPHA",
            "theseus-alpha-token-9f8e7d6c5b4a",
        );
        let redactor = Redactor::from_env_keys(&["THESEUS_TEST_SECRETS_ALPHA"]);
        // Вхождение отдельно, вхождение склеенное с соседним текстом,
        // вхождение повторное — маскируются все.
        let text = "a=theseus-alpha-token-9f8e7d6c5b4a; \
                    склейка:prefix-theseus-alpha-token-9f8e7d6c5b4a-suffix; \
                    повтор theseus-alpha-token-9f8e7d6c5b4a";
        let out = redactor.redact(text);
        assert!(!out.contains("theseus-alpha-token-9f8e7d6c5b4a"), "значение протекло: {out}");
        assert_eq!(out.matches("[REDACTED env:THESEUS_TEST_SECRETS_ALPHA]").count(), 3);
        // Имя переменной само по себе — не секрет и остаётся видимым.
        assert!(redactor.is_clean("THESEUS_TEST_SECRETS_ALPHA"));
        // Скан находит env-правило по имени.
        let hits = redactor.scan("x theseus-alpha-token-9f8e7d6c5b4a y");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].rule, "env:THESEUS_TEST_SECRETS_ALPHA");
    }

    #[test]
    fn env_values_shorter_than_min_are_ignored() {
        let _guard = EnvGuard::set("THESEUS_TEST_SECRETS_SHORT", "abc1234"); // 7 < 8
        let redactor = Redactor::from_env_keys(&["THESEUS_TEST_SECRETS_SHORT"]);
        assert!(redactor.is_clean("abc1234"));
        assert!(
            !redactor.rules().iter().any(|r| r.name.contains("SHORT")),
            "короткое значение не должно порождать правило"
        );

        // Отсутствующая переменная — тоже без правила и без паники.
        let redactor = Redactor::from_env_keys(&["THESEUS_TEST_SECRETS_MISSING"]);
        assert!(redactor.is_clean("что угодно"));
    }

    #[test]
    fn env_value_with_regex_metacharacters_is_literal() {
        // Значение с regex-спецсимволами должно матчиться как литерал.
        let _guard = EnvGuard::set("THESEUS_TEST_SECRETS_META", "a.b+c*d(e)f[g]h^i$j\\k|l?m");
        let redactor = Redactor::from_env_keys(&["THESEUS_TEST_SECRETS_META"]);
        let out = redactor.redact("утечка: a.b+c*d(e)f[g]h^i$j\\k|l?m конец");
        assert!(!out.contains("a.b+c*d(e)f[g]h^i$j\\k|l?m"), "значение протекло: {out}");
        // Похожая, но не точная строка не маскируется.
        assert!(redactor.is_clean("aXb+c*d(e)f[g]h^i$j\\k|l?m"));
    }

    #[test]
    fn short_benign_strings_do_not_false_positive() {
        let redactor = Redactor::with_builtin_rules();
        let benign = [
            "sk-123",                                    // OpenAI: тело короче 20
            "sk-",                                       // пустое тело
            "Bearer abc",                                // токен короче 16
            "AKIA1234",                                  // короче 16 после AKIA
            "akiaiosfodnn7example",                      // строчные — не AWS-формат
            "deadbeefcafe",                              // hex короче 32
            "color: #ff00aa; token=42",                  // css-хэкс и числа
            "DEEPSEEK_API_KEY=",                         // пустое значение
            "DEEPSEEK_API_KEY=short",                    // значение короче 8
            "password",                                  // само слово — не секрет
            "https://example.com/path",                  // URL без userinfo
            "user@example.com",                          // почта, не URL
            "-----BEGIN PUBLIC KEY-----",                // публичный ключ — ок
            "обычный русский текст с цифрами 123456789", // проза
        ];
        for text in benign {
            assert!(redactor.is_clean(text), "ложное срабатывание на: {text}");
        }
    }

    #[test]
    fn empty_and_unicode_text_are_safe() {
        let redactor = Redactor::with_builtin_rules();
        assert_eq!(redactor.redact(""), "");
        assert!(redactor.is_clean(""));
        assert!(redactor.scan("").is_empty());

        // Многобайтовый секрет из env: срез префикса по символам, без паники.
        let _guard = EnvGuard::set("THESEUS_TEST_SECRETS_UNICODE", "секретный-токен-123");
        let redactor = Redactor::from_env_keys(&["THESEUS_TEST_SECRETS_UNICODE"]);
        let out = redactor.redact("утечка секретный-токен-123 в логе");
        assert!(!out.contains("секретный-токен-123"), "значение протекло: {out}");

        // keep_prefix по символам на кириллице.
        let rule = SecretRule::new("uni", r"секрет[0-9]+", 7).unwrap();
        let redactor = Redactor::new(vec![rule]);
        assert_eq!(redactor.redact("секрет42"), "секрет4…[REDACTED uni]");
    }

    #[test]
    fn redact_is_idempotent() {
        let redactor = Redactor::with_builtin_rules();
        let text = format!(
            "ключи: {OPENAI_KEY}, {AWS_KEY}, hex {HEX_TOKEN}, bearer {BEARER_TOKEN}"
        );
        let once = redactor.redact(&text);
        let twice = redactor.redact(&once);
        assert_eq!(once, twice, "повторная маскировка изменила текст: {twice}");
    }

    #[test]
    fn redact_one_megabyte_is_fast() {
        let redactor = Redactor::with_builtin_rules();
        // ~1 МиБ преимущественно чистого текста с редкими вкраплениями секретов.
        let line = "строка лога агента: файл прочитан, diff применён, тесты зелёные\n";
        let mut text = String::with_capacity(1 << 21);
        while text.len() < (1 << 20) {
            text.push_str(line);
            if text.len() % 65536 < line.len() {
                text.push_str("служебно: sk-proj_abcd1234EFGH5678ijkl9012MNOP3456qrst\n");
            }
        }
        assert!(text.len() >= (1 << 20), "собрали {} байт", text.len());

        let start = Instant::now();
        let out = redactor.redact(&text);
        let elapsed = start.elapsed();

        assert!(!out.contains("sk-proj_abcd1234"), "секрет уцелел в 1 МиБ");
        assert!(redactor.is_clean(&out));
        // В debug-сборке замерено ~1 с; порог с запасом ловит только
        // патологические регрессии (катастрофический бэктрекинг и т.п.).
        assert!(elapsed < Duration::from_secs(5), "слишком медленно: {elapsed:?}");
    }
}
