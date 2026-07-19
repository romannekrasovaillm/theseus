//! Провайдеры веб-поиска с TTL-кэшем (v0.3.2).
//!
//! Самодостаточный модуль (std + serde_json + regex + anyhow + reqwest/blocking),
//! выносящий логику простого `tools::web_search` в полноценную подсистему:
//!
//! - [`SearchResult`] — единая запись результата (заголовок, URL, сниппет);
//! - [`SearchProvider`] — трейт источника поиска (трейт-объект, мокабельно);
//! - [`DuckDuckGoProvider`] — парсинг HTML-выдачи `html.duckduckgo.com` regex'ом;
//! - [`WikipediaProvider`] — JSON API `action=opensearch`;
//! - [`SearchRouter`] — fallback-цепочка провайдеров с агрегацией, дедупликацией
//!   по URL, ограничением `max_results` и TTL-кэшем в памяти
//!   (`BTreeMap<запрос, (Instant, результаты)>`). Часы инжектируются, чтобы
//!   TTL тестировался без реального ожидания.

use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};

/// Единый результат поиска независимо от провайдера.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResult {
    /// Заголовок найденной страницы.
    pub title: String,
    /// Канонический URL (после разворачивания редиректов DDG).
    pub url: String,
    /// Короткий фрагмент текста (может быть пустым).
    pub snippet: String,
}

impl SearchResult {
    /// Конструктор-удобство для тестов и мок-провайдеров.
    pub fn new(title: impl Into<String>, url: impl Into<String>, snippet: impl Into<String>) -> Self {
        Self { title: title.into(), url: url.into(), snippet: snippet.into() }
    }
}

/// Источник веб-поиска. Трейт-объект — роутер работает с `Box<dyn SearchProvider>`.
pub trait SearchProvider: Send + Sync {
    /// Короткое имя провайдера для логов и сообщений об ошибках.
    fn name(&self) -> &str;
    /// Выполнить поиск и вернуть до `limit` результатов.
    ///
    /// `limit == 0` — пустой запрос по объёму: провайдер вправе вернуть пустой
    /// вектор без сетевого обращения. Пустой `query` — ошибка.
    fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>>;
}

// ---------------- общие текстовые помощники ----------------

/// Обратный percent-decode для параметра `uddg` в ссылках-редиректах DDG.
/// `+` трактуем как литерал (в `uddg` пробелы кодируются `%20`), некорректные
/// `%`-последовательности оставляем как есть.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..=i + 2]).unwrap_or("");
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Минимальная разэскейпка HTML-сущностей, встречающихся в выдаче DDG.
/// `&amp;` разворачиваем последним, чтобы не задеть уже развёрнутые сущности.
fn html_unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

// ---------------- DuckDuckGo (HTML-выдача) ----------------

/// Разборщик HTML-выдачи DuckDuckGo. Regex'ы компилируются один раз при
/// создании провайдера, поэтому в «горячем» пути компиляции нет.
struct DdgParser {
    /// Любой якорь: атрибуты + внутренний HTML. Порядок атрибутов не важен —
    /// класс и href извлекаем из строки атрибутов отдельно.
    anchor_re: Regex,
    /// `href="..."` внутри строки атрибутов.
    href_re: Regex,
    /// Все HTML-теги (для снятия разметки с заголовков/сниппетов).
    tag_re: Regex,
    /// Схлопывание пробельных серий в один пробел.
    ws_re: Regex,
}

impl DdgParser {
    fn new() -> Result<Self> {
        Ok(Self {
            anchor_re: Regex::new(r"(?s)<a\b([^>]*)>(.*?)</a>").context("regex anchor")?,
            href_re: Regex::new(r#"href="([^"]*)""#).context("regex href")?,
            tag_re: Regex::new(r"(?s)<[^>]+>").context("regex tag")?,
            ws_re: Regex::new(r"\s+").context("regex ws")?,
        })
    }

    /// Снять теги, разэскейпить сущности и схлопнуть пробелы.
    fn clean_text(&self, raw: &str) -> String {
        let no_tags = self.tag_re.replace_all(raw, "");
        let unescaped = html_unescape(&no_tags);
        self.ws_re.replace_all(unescaped.trim(), " ").into_owned()
    }

    /// Привести href из выдачи DDG к каноническому URL:
    /// развернуть редирект `//duckduckgo.com/l/?uddg=...`, дописать схему
    /// протокол-относительным ссылкам.
    fn clean_url(&self, raw: &str) -> String {
        let href = html_unescape(raw);
        if let Some(pos) = href.find("uddg=") {
            let rest = &href[pos + "uddg=".len()..];
            let end = rest.find('&').unwrap_or(rest.len());
            let decoded = percent_decode(&rest[..end]);
            if !decoded.is_empty() {
                return decoded;
            }
        }
        if let Some(stripped) = href.strip_prefix("//") {
            return format!("https://{stripped}");
        }
        href
    }

    /// Разобрать HTML-страницу выдачи: заголовки — якоря `result__a`,
    /// сниппеты — якоря `result__snippet` (привязываются к последнему
    /// заголовку без сниппета, т.к. в разметке они идут следом).
    fn parse(&self, html: &str, limit: usize) -> Vec<SearchResult> {
        let mut out: Vec<SearchResult> = Vec::new();
        for cap in self.anchor_re.captures_iter(html) {
            let attrs = &cap[1];
            let inner = &cap[2];
            if attrs.contains("result__a") {
                let url = self
                    .href_re
                    .captures(attrs)
                    .map(|c| self.clean_url(&c[1]))
                    .unwrap_or_default();
                if url.is_empty() {
                    continue;
                }
                out.push(SearchResult { title: self.clean_text(inner), url, snippet: String::new() });
            } else if attrs.contains("result__snippet") {
                let snippet = self.clean_text(inner);
                if snippet.is_empty() {
                    continue;
                }
                // Приклеиваем сниппет к последнему результату, у которого его ещё нет.
                if let Some(last) = out.iter_mut().rev().find(|r| r.snippet.is_empty()) {
                    last.snippet = snippet;
                }
            }
        }
        out.truncate(limit);
        out
    }
}

/// Провайдер поиска по HTML-выдаче DuckDuckGo (`html.duckduckgo.com/html/`,
/// POST-форма). Не требует JS и API-ключа; терпим к не-200 ответам — они
/// превращаются в ошибку, которую роутер обходит fallback'ом.
pub struct DuckDuckGoProvider {
    client: reqwest::blocking::Client,
    endpoint: String,
    parser: DdgParser,
}

impl DuckDuckGoProvider {
    /// Провайдер с endpoint'ом по умолчанию и заданным HTTP-таймаутом.
    pub fn new(timeout: Duration) -> Result<Self> {
        Self::with_endpoint("https://html.duckduckgo.com/html/", timeout)
    }

    /// Провайдер с переопределённым endpoint'ом (полезно для тестов/прокси).
    pub fn with_endpoint(endpoint: impl Into<String>, timeout: Duration) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(timeout.max(Duration::from_secs(5)))
            .user_agent("theseus-websearch/0.2")
            .build()
            .context("сборка HTTP-клиента DDG")?;
        Ok(Self { client, endpoint: endpoint.into(), parser: DdgParser::new()? })
    }
}

impl SearchProvider for DuckDuckGoProvider {
    fn name(&self) -> &str {
        "duckduckgo"
    }

    fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        if query.trim().is_empty() {
            bail!("duckduckgo: пустой запрос");
        }
        if limit == 0 {
            return Ok(Vec::new());
        }
        let resp = self
            .client
            .post(&self.endpoint)
            .form(&[("q", query)])
            .send()
            .context("duckduckgo: запрос не отправлен")?;
        if !resp.status().is_success() {
            bail!("duckduckgo: HTTP {}", resp.status());
        }
        let body = resp.text().context("duckduckgo: чтение тела ответа")?;
        Ok(self.parser.parse(&body, limit))
    }
}

// ---------------- Wikipedia (opensearch JSON) ----------------

/// Разбор ответа `action=opensearch`: массив
/// `[запрос, [заголовки], [описания], [url]]`. Массивы могут отличаться
/// по длине — идём по минимальной.
fn parse_opensearch(body: &str, limit: usize) -> Result<Vec<SearchResult>> {
    let v: serde_json::Value = serde_json::from_str(body).context("opensearch: невалидный JSON")?;
    let arr = v.as_array().ok_or_else(|| anyhow!("opensearch: корень не массив"))?;
    let titles = arr.get(1).and_then(serde_json::Value::as_array);
    let descs = arr.get(2).and_then(serde_json::Value::as_array);
    let urls = arr.get(3).and_then(serde_json::Value::as_array);
    let (Some(titles), Some(urls)) = (titles, urls) else {
        bail!("opensearch: нет массивов заголовков/URL");
    };
    let mut out = Vec::new();
    for (i, t) in titles.iter().enumerate().take(limit) {
        let title = t.as_str().unwrap_or_default().trim();
        let url = urls.get(i).and_then(|u| u.as_str()).unwrap_or_default().trim();
        if title.is_empty() || url.is_empty() {
            continue;
        }
        let snippet = descs
            .and_then(|d| d.get(i))
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        out.push(SearchResult { title: title.to_string(), url: url.to_string(), snippet });
    }
    Ok(out)
}

/// Провайдер поиска по Wikipedia через `w/api.php?action=opensearch`
/// (JSON `[query, titles, descriptions, urls]`).
pub struct WikipediaProvider {
    client: reqwest::blocking::Client,
    /// Языковой раздел: `ru`, `en`, ...
    lang: String,
}

impl WikipediaProvider {
    /// Провайдер для языкового раздела `lang` с заданным HTTP-таймаутом.
    pub fn new(lang: impl Into<String>, timeout: Duration) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(timeout.max(Duration::from_secs(5)))
            .user_agent("theseus-websearch/0.2")
            .build()
            .context("сборка HTTP-клиента Wikipedia")?;
        Ok(Self { client, lang: lang.into() })
    }

    /// URL API для текущего языкового раздела.
    fn api_url(&self) -> String {
        format!("https://{}.wikipedia.org/w/api.php", self.lang)
    }
}

impl SearchProvider for WikipediaProvider {
    fn name(&self) -> &str {
        "wikipedia"
    }

    fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        if query.trim().is_empty() {
            bail!("wikipedia: пустой запрос");
        }
        if limit == 0 {
            return Ok(Vec::new());
        }
        let limit_s = limit.to_string();
        let resp = self
            .client
            .get(self.api_url())
            .query(&[
                ("action", "opensearch"),
                ("format", "json"),
                ("limit", limit_s.as_str()),
                ("search", query),
            ])
            .send()
            .context("wikipedia: запрос не отправлен")?;
        if !resp.status().is_success() {
            bail!("wikipedia: HTTP {}", resp.status());
        }
        let body = resp.text().context("wikipedia: чтение тела ответа")?;
        parse_opensearch(&body, limit)
    }
}

// ---------------- SearchRouter: fallback + агрегация + TTL-кэш ----------------

/// Маршрутизатор поиска по цепочке провайдеров.
///
/// Семантика `search`:
/// 1. запрос нормализуется (trim + lowercase) и служит ключом кэша;
/// 2. свежая (`age < ttl`) запись кэша возвращается без сетевых вызовов;
/// 3. иначе провайдеры опрашиваются по порядку, результаты агрегируются
///    с дедупликацией по URL, пока не наберётся `max_results`; упавший
///    провайдер пропускается (fallback к следующему);
/// 4. если не ответил ни один провайдер — объединённая ошибка;
/// 5. успешная выдача кладётся в кэш целиком (до `max_results`), читатели
///    обрезают её до своего `limit`.
pub struct SearchRouter {
    providers: Vec<Box<dyn SearchProvider>>,
    ttl: Duration,
    max_results: usize,
    cache: BTreeMap<String, (Instant, Vec<SearchResult>)>,
    clock: Box<dyn Fn() -> Instant + Send + Sync>,
}

impl SearchRouter {
    /// Роутер с системными часами (`Instant::now`).
    pub fn new(providers: Vec<Box<dyn SearchProvider>>, ttl: Duration, max_results: usize) -> Self {
        Self::with_clock(providers, ttl, max_results, Box::new(Instant::now))
    }

    /// Роутер с инжектируемыми часами — для детерминированных тестов TTL.
    pub fn with_clock(
        providers: Vec<Box<dyn SearchProvider>>,
        ttl: Duration,
        max_results: usize,
        clock: Box<dyn Fn() -> Instant + Send + Sync>,
    ) -> Self {
        Self { providers, ttl, max_results: max_results.max(1), cache: BTreeMap::new(), clock }
    }

    /// Имена провайдеров в порядке опроса (для диагностики/логов).
    pub fn provider_names(&self) -> Vec<&str> {
        self.providers.iter().map(|p| p.name()).collect()
    }

    /// Число записей в кэше (включая, возможно, протухшие — они вычищаются
    /// лениво при следующей записи).
    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }

    /// Полностью очистить кэш.
    pub fn clear_cache(&mut self) {
        self.cache.clear();
    }

    /// Нормализованный ключ кэша: регистр и краевые пробелы не значимы.
    fn cache_key(query: &str) -> String {
        query.trim().to_lowercase()
    }

    /// Выполнить поиск через цепочку провайдеров (см. документацию структуры).
    pub fn search(&mut self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let key = Self::cache_key(query);
        if key.is_empty() {
            bail!("пустой поисковый запрос");
        }
        if limit == 0 {
            return Ok(Vec::new());
        }
        let now = (self.clock)();
        let limit = limit.min(self.max_results);

        // 1) Свежая запись кэша.
        if let Some((ts, cached)) = self.cache.get(&key) {
            if now.saturating_duration_since(*ts) < self.ttl {
                let mut out = cached.clone();
                out.truncate(limit);
                return Ok(out);
            }
        }

        // 2) Опрос провайдеров с fallback и дедупликацией по URL.
        if self.providers.is_empty() {
            bail!("SearchRouter без провайдеров");
        }
        let mut out: Vec<SearchResult> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut errors: Vec<String> = Vec::new();
        for provider in &self.providers {
            match provider.search(query, self.max_results) {
                Ok(results) => {
                    for r in results {
                        if r.url.is_empty() || !seen.insert(r.url.clone()) {
                            continue;
                        }
                        out.push(r);
                        if out.len() >= self.max_results {
                            break;
                        }
                    }
                }
                Err(e) => errors.push(format!("{}: {e}", provider.name())),
            }
            if out.len() >= self.max_results {
                break;
            }
        }
        if out.is_empty() && !errors.is_empty() {
            return Err(anyhow!("все провайдеры не ответили: {}", errors.join("; ")));
        }

        // 3) Запись в кэш + ленивая уборка протухших записей.
        let ttl = self.ttl;
        self.cache.retain(|_, (ts, _)| now.saturating_duration_since(*ts) < ttl);
        self.cache.insert(key, (now, out.clone()));
        out.truncate(limit);
        Ok(out)
    }
}

/// Фабрика цепочки по умолчанию: DDG (полнотекст) → Wikipedia (энциклопедия).
pub fn default_router(timeout: Duration, ttl: Duration, max_results: usize) -> Result<SearchRouter> {
    let providers: Vec<Box<dyn SearchProvider>> = vec![
        Box::new(DuckDuckGoProvider::new(timeout)?),
        Box::new(WikipediaProvider::new("ru", timeout)?),
    ];
    Ok(SearchRouter::new(providers, ttl, max_results))
}

// ---------------- тесты ----------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Мок-провайдер: отдаёт заранее заданные результаты или падает,
    /// считает число вызовов (для проверки кэша и fallback).
    struct MockProvider {
        label: String,
        results: Vec<SearchResult>,
        fail: bool,
        calls: Arc<AtomicUsize>,
    }

    impl MockProvider {
        fn ok(label: &str, results: Vec<SearchResult>, calls: Arc<AtomicUsize>) -> Box<dyn SearchProvider> {
            Box::new(Self { label: label.into(), results, fail: false, calls })
        }

        fn failing(label: &str, calls: Arc<AtomicUsize>) -> Box<dyn SearchProvider> {
            Box::new(Self { label: label.into(), results: Vec::new(), fail: true, calls })
        }
    }

    impl SearchProvider for MockProvider {
        fn name(&self) -> &str {
            &self.label
        }

        fn search(&self, _query: &str, limit: usize) -> Result<Vec<SearchResult>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(anyhow!("{}: сбой стенда", self.label));
            }
            let mut out = self.results.clone();
            out.truncate(limit);
            Ok(out)
        }
    }

    /// Инжектируемые часы: разделяемый `Instant`, который тест двигает вперёд.
    struct TestClock {
        now: Arc<Mutex<Instant>>,
    }

    impl TestClock {
        fn new() -> Self {
            Self { now: Arc::new(Mutex::new(Instant::now())) }
        }

        fn clock(&self) -> Box<dyn Fn() -> Instant + Send + Sync> {
            let shared = Arc::clone(&self.now);
            Box::new(move || *shared.lock().unwrap())
        }

        fn advance(&self, d: Duration) {
            let mut guard = self.now.lock().unwrap();
            *guard += d;
        }
    }

    fn res(title: &str, url: &str) -> SearchResult {
        SearchResult::new(title, url, format!("сниппет {title}"))
    }

    #[test]
    fn router_returns_results_from_first_provider() {
        let calls = Arc::new(AtomicUsize::new(0));
        let providers = vec![MockProvider::ok("m1", vec![res("A", "https://a/"), res("B", "https://b/")], Arc::clone(&calls))];
        let mut router = SearchRouter::new(providers, Duration::from_secs(60), 10);
        let out = router.search("rust", 10).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].url, "https://a/");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn router_falls_back_when_first_provider_errors() {
        let calls = Arc::new(AtomicUsize::new(0));
        let providers: Vec<Box<dyn SearchProvider>> = vec![
            MockProvider::failing("bad", Arc::clone(&calls)),
            MockProvider::ok("good", vec![res("W", "https://w/")], Arc::clone(&calls)),
        ];
        let mut router = SearchRouter::new(providers, Duration::from_secs(60), 10);
        let out = router.search("rust", 5).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].url, "https://w/");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "оба провайдера должны быть опрошены");
    }

    #[test]
    fn router_aggregates_and_dedups_by_url() {
        let c1 = Arc::new(AtomicUsize::new(0));
        let c2 = Arc::new(AtomicUsize::new(0));
        let providers: Vec<Box<dyn SearchProvider>> = vec![
            MockProvider::ok("p1", vec![res("A", "https://x/"), res("B", "https://y/")], c1),
            MockProvider::ok("p2", vec![res("A2", "https://x/"), res("C", "https://z/")], c2),
        ];
        let mut router = SearchRouter::new(providers, Duration::from_secs(60), 10);
        let out = router.search("q", 10).unwrap();
        let urls: Vec<&str> = out.iter().map(|r| r.url.as_str()).collect();
        assert_eq!(urls, ["https://x/", "https://y/", "https://z/"], "дубль по URL выкинут, порядок провайдеров сохранён");
    }

    #[test]
    fn router_errors_when_all_providers_fail() {
        let calls = Arc::new(AtomicUsize::new(0));
        let providers: Vec<Box<dyn SearchProvider>> = vec![
            MockProvider::failing("m1", Arc::clone(&calls)),
            MockProvider::failing("m2", Arc::clone(&calls)),
        ];
        let mut router = SearchRouter::new(providers, Duration::from_secs(60), 10);
        let err = router.search("rust", 5).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("m1") && msg.contains("m2"), "ошибка агрегирует имена провайдеров: {msg}");
    }

    #[test]
    fn router_cache_hit_skips_providers() {
        let calls = Arc::new(AtomicUsize::new(0));
        let providers = vec![MockProvider::ok("m", vec![res("A", "https://a/")], Arc::clone(&calls))];
        let mut router = SearchRouter::new(providers, Duration::from_secs(60), 10);
        let _ = router.search("rust", 10).unwrap();
        let out = router.search("rust", 10).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "второй вызов — из кэша");
        assert_eq!(router.cache_len(), 1);
    }

    #[test]
    fn router_cache_expires_after_ttl() {
        let calls = Arc::new(AtomicUsize::new(0));
        let providers = vec![MockProvider::ok("m", vec![res("A", "https://a/")], Arc::clone(&calls))];
        let clock = TestClock::new();
        let mut router = SearchRouter::with_clock(providers, Duration::from_secs(30), 10, clock.clock());
        let _ = router.search("rust", 10).unwrap();
        clock.advance(Duration::from_secs(10));
        let _ = router.search("rust", 10).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "в пределах TTL — кэш");
        clock.advance(Duration::from_secs(25));
        let _ = router.search("rust", 10).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2, "после TTL — повторный поиск");
    }

    #[test]
    fn router_cache_key_is_normalized() {
        let calls = Arc::new(AtomicUsize::new(0));
        let providers = vec![MockProvider::ok("m", vec![res("A", "https://a/")], Arc::clone(&calls))];
        let mut router = SearchRouter::new(providers, Duration::from_secs(60), 10);
        let _ = router.search("  Rust Lang ", 10).unwrap();
        let _ = router.search("rust lang", 10).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "trim + lowercase дают один ключ");
    }

    #[test]
    fn router_respects_max_results_and_limit() {
        let calls = Arc::new(AtomicUsize::new(0));
        let many: Vec<SearchResult> = (0..8).map(|i| res(&format!("T{i}"), &format!("https://e/{i}"))).collect();
        let providers = vec![MockProvider::ok("m", many, Arc::clone(&calls))];
        let mut router = SearchRouter::new(providers, Duration::from_secs(60), 3);
        let out = router.search("q", 10).unwrap();
        assert_eq!(out.len(), 3, "max_results=3 обрезает выдачу");
        let out2 = router.search("q", 2).unwrap();
        assert_eq!(out2.len(), 2, "читатель обрезает кэш до своего limit");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn router_zero_limit_returns_empty_without_calls() {
        let calls = Arc::new(AtomicUsize::new(0));
        let providers = vec![MockProvider::ok("m", vec![res("A", "https://a/")], Arc::clone(&calls))];
        let mut router = SearchRouter::new(providers, Duration::from_secs(60), 10);
        let out = router.search("q", 0).unwrap();
        assert!(out.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn router_rejects_empty_query() {
        let providers: Vec<Box<dyn SearchProvider>> = vec![MockProvider::ok("m", vec![], Arc::new(AtomicUsize::new(0)))];
        let mut router = SearchRouter::new(providers, Duration::from_secs(60), 10);
        assert!(router.search("   ", 5).is_err());
    }

    #[test]
    fn router_without_providers_errors() {
        let mut router = SearchRouter::new(Vec::new(), Duration::from_secs(60), 10);
        let err = router.search("q", 5).unwrap_err();
        assert!(format!("{err}").contains("без провайдеров"));
    }

    // --- парсинг DDG-HTML ---

    /// Фикстура: фрагмент реальной разметки html.duckduckgo.com —
    /// первый результат через редирект `uddg` с сущностями, второй — прямая
    /// ссылка, плюс посторонний якорь без result-классов.
    const DDG_FIXTURE: &str = r##"
<html><body>
<div class="result results_links results_links_deep web-result">
  <div class="links_main links_deep result__body">
    <h2 class="result__title">
      <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust-lang.org%2F&amp;rut=deadbeef">Rust Programming Language &amp; Tools</a>
    </h2>
    <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust-lang.org%2F">A language empowering everyone
to build reliable and <b>efficient</b> software.</a>
  </div>
</div>
<div class="result results_links results_links_deep web-result">
  <div class="links_main links_deep result__body">
    <h2 class="result__title">
      <a rel="nofollow" class="result__a" href="https://example.com/page">Example &lt;Page&gt;</a>
    </h2>
    <a class="result__snippet" href="https://example.com/page">Second snippet &quot;quoted&quot;.</a>
  </div>
</div>
<a href="https://ads.example/">sponsored</a>
</body></html>"##;

    #[test]
    fn ddg_parses_fixture_results() {
        let parser = DdgParser::new().unwrap();
        let out = parser.parse(DDG_FIXTURE, 10);
        assert_eq!(out.len(), 2, "рекламный якорь без result__a игнорируется");

        assert_eq!(out[0].title, "Rust Programming Language & Tools");
        assert_eq!(out[0].url, "https://www.rust-lang.org/", "uddg-редирект развёрнут");
        assert_eq!(
            out[0].snippet,
            "A language empowering everyone to build reliable and efficient software.",
            "теги сняты, переносы схлопнуты"
        );

        assert_eq!(out[1].title, "Example <Page>");
        assert_eq!(out[1].url, "https://example.com/page");
        assert_eq!(out[1].snippet, "Second snippet \"quoted\".");
    }

    #[test]
    fn ddg_respects_limit() {
        let parser = DdgParser::new().unwrap();
        let out = parser.parse(DDG_FIXTURE, 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].url, "https://www.rust-lang.org/");
    }

    #[test]
    fn ddg_empty_html_yields_empty() {
        let parser = DdgParser::new().unwrap();
        assert!(parser.parse("<html><body>no results</body></html>", 10).is_empty());
        assert!(parser.parse("", 10).is_empty());
    }

    #[test]
    fn ddg_anchor_without_href_is_skipped() {
        let parser = DdgParser::new().unwrap();
        let html = r#"<a class="result__a">No href</a><a class="result__a" href="https://ok/">Ok</a>"#;
        let out = parser.parse(html, 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].url, "https://ok/");
    }

    #[test]
    fn ddg_orphan_snippet_is_ignored() {
        let parser = DdgParser::new().unwrap();
        let html = r#"<a class="result__snippet" href="https://x/">lonely</a>"#;
        assert!(parser.parse(html, 10).is_empty(), "сниппет без заголовка результата не создаёт");
    }

    // --- парсинг opensearch JSON ---

    #[test]
    fn opensearch_parses_valid_response() {
        let body = r#"["rust",["Rust (programming language)","Rust Belt"],["A systems language","A region"],["https://en.wikipedia.org/wiki/Rust_(programming_language)","https://en.wikipedia.org/wiki/Rust_Belt"]]"#;
        let out = parse_opensearch(body, 10).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].title, "Rust (programming language)");
        assert_eq!(out[0].snippet, "A systems language");
        assert!(out[0].url.ends_with("Rust_(programming_language)"));
    }

    #[test]
    fn opensearch_truncates_to_limit() {
        let body = r#"["q",["A","B","C"],["","",""],["https://a/","https://b/","https://c/"]]"#;
        let out = parse_opensearch(body, 2).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].url, "https://b/");
    }

    #[test]
    fn opensearch_tolerates_missing_descriptions() {
        // Пустой массив описаний — законный ответ opensearch, сниппет пуст.
        let body = r#"["q",["A"],[],["https://a/"]]"#;
        let out = parse_opensearch(body, 10).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].snippet, "");
    }

    #[test]
    fn opensearch_malformed_json_errors() {
        assert!(parse_opensearch("not json", 10).is_err());
        assert!(parse_opensearch(r#"{"a":1}"#, 10).is_err(), "объект вместо массива");
        assert!(parse_opensearch(r#"["q"]"#, 10).is_err(), "нет массивов результатов");
    }

    #[test]
    fn opensearch_skips_entries_with_empty_url() {
        let body = r#"["q",["A","B"],["",""],["","https://b/"]]"#;
        let out = parse_opensearch(body, 10).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "B");
    }

    // --- текстовые помощники ---

    #[test]
    fn percent_decode_roundtrip() {
        assert_eq!(percent_decode("https%3A%2F%2Fexample.com%2Fa%20b"), "https://example.com/a b");
        assert_eq!(percent_decode("plain+text"), "plain+text", "+ не трогаем");
        assert_eq!(percent_decode("100%"), "100%", "висячий % не паникует");
        assert_eq!(percent_decode("%zz"), "%zz", "невалидный hex оставлен как есть");
    }

    #[test]
    fn html_unescape_order_is_safe() {
        assert_eq!(html_unescape("&amp;lt;"), "&lt;", "двойная эскейпка разворачивается один раз");
        assert_eq!(html_unescape("&lt;b&gt;&amp;&quot;&#39;&nbsp;"), "<b>&\"' ");
    }

    #[test]
    fn default_router_builds_chain() {
        let router = default_router(Duration::from_secs(10), Duration::from_secs(300), 5).unwrap();
        assert_eq!(router.provider_names(), ["duckduckgo", "wikipedia"]);
    }
}
