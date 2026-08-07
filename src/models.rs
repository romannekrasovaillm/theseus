//! Реестр провайдеров и моделей LLM (по образцу `codex-rs/model-provider-info`).
//!
//! Встроенные описания провайдеров (DeepSeek, Kimi, Moonshot и произвольный
//! OpenAI-совместимый эндпоинт) и их моделей: лимиты контекста, поддержка
//! thinking/tools, ориентировочные цены. Возможности:
//!
//! - поиск модели по идентификатору — [`find_model`];
//! - разрешение модели в креды вызова API из env-переменной — [`resolve`],
//!   [`resolve_with_env`];
//! - оценка заполненности контекста — [`estimate_context_pct`];
//! - подсказка ближайших моделей при опечатке — [`nearest_models`]
//!   (собственная реализация расстояния Левенштейна, [`levenshtein`]).
//!
//! Модуль самодостаточен: только `std`, `serde` и `anyhow`.

use std::env;
use std::fmt;

use anyhow::{anyhow, ensure, Context, Result};
use serde::{Deserialize, Serialize};

/// Тип «проводного» API провайдера: каким эндпоинтом с ним разговаривать.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WireApi {
    /// OpenAI-совместимый `POST /chat/completions` (все встроенные провайдеры).
    Chat,
    /// OpenAI Responses API (`POST /responses`) — задел на будущих провайдеров.
    Responses,
}

impl WireApi {
    /// Строковое имя для логов и конфигов: `"chat"` | `"responses"`.
    pub fn as_str(self) -> &'static str {
        match self {
            WireApi::Chat => "chat",
            WireApi::Responses => "responses",
        }
    }
}

impl fmt::Display for WireApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Ориентировочная цена токенов в USD за 1 млн (июль 2026; уточняйте у провайдера).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CostHint {
    /// входные (prompt) токены
    pub input_usd_per_mtok: f64,
    /// выходные (completion) токены
    pub output_usd_per_mtok: f64,
}

/// Описание провайдера LLM: куда слать запросы и как аутентифицироваться.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderInfo {
    /// короткое имя: `"deepseek"` | `"kimi"` | `"moonshot"` | `"zhipu"` | `"openai-compatible"`
    pub name: String,
    /// базовый URL API (без завершающего слеша), напр. `https://api.deepseek.com/v1`
    pub base_url: String,
    /// имя env-переменной с API-ключом (`None` — ключ не нужен, локальный эндпоинт)
    pub env_key: Option<String>,
    /// запасные имена env-переменных ключа (пробуются, если `env_key` не задан):
    /// пользователи ставят ключ под разными именами (GLM_API_KEY, ZAI_API_KEY...)
    #[serde(default)]
    pub env_key_aliases: Vec<String>,
    /// проводной API: chat/completions или responses
    pub wire_api: WireApi,
    /// дополнительные HTTP-заголовки по умолчанию (подмешиваются в каждый запрос)
    pub default_headers: Vec<(String, String)>,
    /// env-переменная, переопределяющая `base_url` (локальные прокси, зеркала)
    pub base_url_env: Option<String>,
    /// предупреждение о сетевых рисках (DPI/SNI-фильтрация у провайдеров РФ и т.п.)
    pub risk_note: Option<String>,
}

impl ProviderInfo {
    /// Базовый URL с учётом env-переопределения (`base_url_env`).
    ///
    /// Пустая/пробельная env-переменная игнорируется; завершающий слеш срезается.
    pub fn effective_base_url(&self) -> String {
        let from_env = self
            .base_url_env
            .as_deref()
            .and_then(|var| env::var(var).ok())
            .map(|url| url.trim().trim_end_matches('/').to_string())
            .filter(|url| !url.is_empty());
        from_env.unwrap_or_else(|| self.base_url.clone())
    }

    /// Требует ли провайдер API-ключ.
    pub fn requires_key(&self) -> bool {
        self.env_key.is_some()
    }
}

/// Описание модели: идентификатор, лимиты, возможности, ценовая подсказка.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    /// идентификатор, передаваемый в API: `deepseek-v4-pro`, `kimi-k3`, ...
    pub id: String,
    /// имя провайдера из [`ProviderInfo::name`]
    pub provider: String,
    /// окно контекста в токенах
    pub context_limit: usize,
    /// максимум токенов на один ответ (`max_tokens`)
    pub max_output: usize,
    /// поддерживает ли thinking-режим (reasoning)
    pub supports_thinking: bool,
    /// поддерживает ли function/tool calling
    pub supports_tools: bool,
    /// ориентировочная цена (`None` — данных нет)
    pub cost_hint: Option<CostHint>,
    /// принудительная температура провайдера: `Some(t)` — API отвергает другие
    /// значения (Kimi K3: «invalid temperature: only 1 is allowed», 08.08);
    /// `None` — харнесс шлёт 0 как обычно
    #[serde(default)]
    pub temperature: Option<f64>,
}

impl ModelInfo {
    /// Билдер для реестра: установить принудительную температуру.
    pub fn with_temperature(mut self, t: f64) -> Self {
        self.temperature = Some(t);
        self
    }
}

impl ModelInfo {
    /// Остаток контекста после `used_tokens` (0 при переполнении).
    pub fn remaining_context(&self, used_tokens: usize) -> usize {
        self.context_limit.saturating_sub(used_tokens)
    }

    /// Оценка стоимости запроса в USD по `cost_hint`; `None`, если цены неизвестны.
    pub fn estimate_cost_usd(&self, input_tokens: u64, output_tokens: u64) -> Option<f64> {
        let hint = self.cost_hint?;
        Some(
            (input_tokens as f64 * hint.input_usd_per_mtok
                + output_tokens as f64 * hint.output_usd_per_mtok)
                / 1_000_000.0,
        )
    }
}

/// Разрешённые креды для вызова API: URL, ключ, модель.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    /// базовый URL провайдера (с учётом env-переопределений)
    pub url: String,
    /// API-ключ (значение env-переменной, обрезанное от пробелов)
    pub key: String,
    /// идентификатор модели
    pub model: String,
}

/// Встроенные провайдеры харнесса.
///
/// Свежий `Vec` на каждый вызов — реестр можно свободно расширять/фильтровать
/// на стороне вызывающего кода без синхронизации.
pub fn builtin_providers() -> Vec<ProviderInfo> {
    vec![
        ProviderInfo {
            name: "deepseek".into(),
            base_url: "https://api.deepseek.com/v1".into(),
            env_key: Some("DEEPSEEK_API_KEY".into()),
            env_key_aliases: vec!["THESEUS_API_KEY".into()],
            wire_api: WireApi::Chat,
            default_headers: Vec::new(),
            base_url_env: Some("DEEPSEEK_BASE_URL".into()),
            risk_note: None,
        },
        ProviderInfo {
            name: "kimi".into(),
            // Kimi Code API (официальная OpenAI-совместимая поверхность,
            // доки kimi.com/code 08.08): chat — /chat/completions; модели
            // k3 / k3-256k / kimi-for-coding[-highspeed]. Старая /v1 — 404.
            base_url: "https://api.kimi.com/coding/v1".into(),
            env_key: Some("KIMI_API_KEY".into()),
            env_key_aliases: vec![],
            wire_api: WireApi::Chat,
            default_headers: Vec::new(),
            base_url_env: Some("KIMI_BASE_URL".into()),
            risk_note: None,
        },
        ProviderInfo {
            name: "moonshot".into(),
            base_url: "https://api.moonshot.ai/v1".into(),
            env_key: Some("MOONSHOT_API_KEY".into()),
            env_key_aliases: vec![],
            wire_api: WireApi::Chat,
            default_headers: Vec::new(),
            base_url_env: Some("MOONSHOT_BASE_URL".into()),
            risk_note: Some(
                "DPI-риск: api.moonshot.ai задушен по SNI у части провайдеров РФ; \
                 при таймаутах используйте туннель/VPN либо зеркало api.kimi.com"
                    .into(),
            ),
        },
        ProviderInfo {
            name: "zhipu".into(),
            base_url: "https://api.z.ai/api/paas/v4".into(),
            env_key: Some("ZHIPU_API_KEY".into()),
            // GLM-ключ пользователи ставят под разными именами (кейс 03.08:
            // GLM_API_KEY/ZAI_API_KEY в env, а харнесс ждал только ZHIPU_API_KEY)
            env_key_aliases: vec!["GLM_API_KEY".into(), "ZAI_API_KEY".into()],
            wire_api: WireApi::Chat,
            default_headers: Vec::new(),
            base_url_env: Some("ZHIPU_BASE_URL".into()),
            risk_note: None,
        },
        ProviderInfo {
            name: "openai-compatible".into(),
            base_url: "http://localhost:8000/v1".into(),
            env_key: Some("OPENAI_API_KEY".into()),
            env_key_aliases: vec![],
            wire_api: WireApi::Chat,
            default_headers: Vec::new(),
            base_url_env: Some("OPENAI_BASE_URL".into()),
            risk_note: None,
        },
    ]
}

/// Встроенные модели всех провайдеров (12 шт.).
///
/// Модели `openai-compatible` в реестр не входят: их идентификаторы и лимиты
/// задаются конфигом пользователя под конкретный эндпоинт.
pub fn builtin_models() -> Vec<ModelInfo> {
    let deepseek = "deepseek";
    let kimi = "kimi";
    let moonshot = "moonshot";
    vec![
        // --- DeepSeek (api.deepseek.com) ---
        model(
            "deepseek-v4-pro",
            deepseek,
            131_072,
            32_768,
            true,
            true,
            Some(CostHint { input_usd_per_mtok: 0.60, output_usd_per_mtok: 1.80 }),
        ),
        model(
            "deepseek-v4-flash",
            deepseek,
            131_072,
            32_768,
            true,
            true,
            // ценовая подсказка уточняется — flash-позиционирование: быстрее
            // и дешевле v4-pro; точные цифры смотрите у провайдера
            None,
        ),
        model(
            "deepseek-chat",
            deepseek,
            131_072,
            8_192,
            false,
            true,
            Some(CostHint { input_usd_per_mtok: 0.28, output_usd_per_mtok: 0.42 }),
        ),
        model(
            "deepseek-reasoner",
            deepseek,
            131_072,
            65_536,
            true,
            true,
            Some(CostHint { input_usd_per_mtok: 0.55, output_usd_per_mtok: 2.19 }),
        ),
        // --- Kimi (api.kimi.com/coding — официальная coding-поверхность) ---
        model(
            "kimi-k2",
            kimi,
            131_072,
            16_384,
            false,
            true,
            Some(CostHint { input_usd_per_mtok: 0.60, output_usd_per_mtok: 2.50 }),
        ),
        model(
            "kimi-k3",
            kimi,
            262_144,
            32_768,
            true,
            true,
            Some(CostHint { input_usd_per_mtok: 1.20, output_usd_per_mtok: 6.00 }),
        ),
        // канонические id по докам Kimi Code (08.08): k3 и k3-256k;
        // K3 принимает только temperature=1 (400 «only 1 is allowed»)
        model(
            "k3",
            kimi,
            262_144,
            32_768,
            true,
            true,
            None, // подписка Kimi Code (Moderato+), не помегабайтная цена
        )
        .with_temperature(1.0),
        model(
            "k3-256k",
            kimi,
            262_144,
            32_768,
            true,
            true,
            None,
        )
        .with_temperature(1.0),
        // --- Moonshot (api.moonshot.ai, DPI-риск) ---
        model(
            "moonshot-v1-8k",
            moonshot,
            8_192,
            4_096,
            false,
            true,
            Some(CostHint { input_usd_per_mtok: 1.70, output_usd_per_mtok: 1.70 }),
        ),
        model(
            "moonshot-v1-32k",
            moonshot,
            32_768,
            8_192,
            false,
            true,
            Some(CostHint { input_usd_per_mtok: 3.40, output_usd_per_mtok: 3.40 }),
        ),
        model(
            "moonshot-v1-128k",
            moonshot,
            131_072,
            8_192,
            false,
            true,
            Some(CostHint { input_usd_per_mtok: 8.40, output_usd_per_mtok: 8.40 }),
        ),
        // --- Zhipu (api.z.ai, GLM) ---
        model(
            "glm-5.2",
            "zhipu",
            131_072,
            32_768,
            true,
            true,
            // ценовая подсказка уточняется — смотрите тарифы z.ai
            None,
        ),
    ]
}

/// Короткий конструктор [`ModelInfo`], чтобы реестр читался таблицей.
#[allow(clippy::too_many_arguments)]
fn model(
    id: &str,
    provider: &str,
    context_limit: usize,
    max_output: usize,
    supports_thinking: bool,
    supports_tools: bool,
    cost_hint: Option<CostHint>,
) -> ModelInfo {
    ModelInfo {
        id: id.into(),
        provider: provider.into(),
        context_limit,
        max_output,
        supports_thinking,
        supports_tools,
        cost_hint,
        temperature: None,
    }
}

/// Найти встроенного провайдера по имени.
pub fn find_provider(name: &str) -> Option<ProviderInfo> {
    builtin_providers().into_iter().find(|p| p.name == name)
}

/// Найти модель по идентификатору (точное совпадение, регистр важен).
pub fn find_model(id: &str) -> Option<ModelInfo> {
    builtin_models().into_iter().find(|m| m.id == id)
}

/// Разрешить модель в креды вызова API.
///
/// Ключ читается из env-переменной, записанной в [`ProviderInfo::env_key`]
/// провайдера модели (`DEEPSEEK_API_KEY`, `KIMI_API_KEY`, ...).
/// Ошибки: модель не найдена (с подсказкой ближайших) либо env-переменная
/// не задана/пуста (с именем переменной в тексте).
pub fn resolve(model_id: &str) -> Result<Credentials> {
    let model = find_model(model_id).ok_or_else(|| unknown_model_error(model_id))?;
    let provider = registry_provider(&model)?;
    let env_key = provider.env_key.as_deref().ok_or_else(|| {
        anyhow!(
            "провайдер «{name}» не объявляет env-переменную ключа; \
             используйте resolve_with_env(\"{model_id}\", <ENV>)",
            name = provider.name,
        )
    })?;
    resolve_parts(&model, &provider, env_key)
}

/// То же, что [`resolve`], но имя env-переменной с ключом задано явно.
///
/// Полезно для оверрайдов, ротации ключей и изолированных тестов.
pub fn resolve_with_env(model_id: &str, api_key_env: &str) -> Result<Credentials> {
    let model = find_model(model_id).ok_or_else(|| unknown_model_error(model_id))?;
    let provider = registry_provider(&model)?;
    resolve_parts(&model, &provider, api_key_env)
}

/// Имена env-переменных, из которых принимается API-ключ модели, в порядке
/// приоритета: `env_key` провайдера (KIMI_API_KEY для k3, ...), его алиасы
/// (GLM_API_KEY/ZAI_API_KEY для zhipu), затем общие исторические имена
/// харнесса (DEEPSEEK_API_KEY, THESEUS_API_KEY). Дубликатов нет.
pub fn api_key_env_names(model_id: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut push = |name: &str| {
        if !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    };
    if let Some(model) = find_model(model_id) {
        if let Some(provider) = find_provider(&model.provider) {
            if let Some(env_key) = &provider.env_key {
                push(env_key);
            }
            for alias in &provider.env_key_aliases {
                push(alias);
            }
        }
    }
    push("DEEPSEEK_API_KEY");
    push("THESEUS_API_KEY");
    names
}

/// Прочитать API-ключ модели из окружения, не требуя его наличия.
///
/// Пробует имена из [`api_key_env_names`] по порядку; пробельные/пустые
/// значения игнорируются, найденное обрезается от пробелов. В отличие от
/// [`resolve`] не падает при отсутствии ключа — это «мягкий» env-слой
/// загрузчика конфига (явный `api_key` в конфиге всегда приоритетнее).
pub fn api_key_from_env(model_id: &str) -> Option<String> {
    api_key_env_names(model_id).into_iter().find_map(|name| {
        env::var(&name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
    })
}

/// Провайдер модели из реестра; отсутствие — внутренняя несогласованность.
fn registry_provider(model: &ModelInfo) -> Result<ProviderInfo> {
    find_provider(&model.provider).with_context(|| {
        format!(
            "внутренняя ошибка реестра: нет провайдера «{prov}» для модели {id}",
            prov = model.provider,
            id = model.id,
        )
    })
}

/// Общее ядро `resolve*`: ключ из env + эффективный URL провайдера.
/// Если основная переменная не задана, пробуются алиасы провайдера
/// (`env_key_aliases` — GLM_API_KEY/ZAI_API_KEY для zhipu и т.п.); в тексте
/// ошибки — все принятые имена и подсказка про окружение процесса.
fn resolve_parts(model: &ModelInfo, provider: &ProviderInfo, api_key_env: &str) -> Result<Credentials> {
    let mut tried = vec![api_key_env.to_string()];
    let mut raw = env::var(api_key_env).ok();
    for alias in &provider.env_key_aliases {
        if raw.is_some() { break; }
        if alias == api_key_env { continue; }
        tried.push(alias.clone());
        raw = env::var(alias).ok();
    }
    let raw = raw.with_context(|| format!(
        "нет API-ключа: задайте env-переменную {}. Если переменная выставлена \
         ПОСЛЕ запуска Тесея — перезапустите его: окружение захватывается при \
         старте процесса и позже не пополняется",
        tried.join(" или ")))?;
    let key = raw.trim();
    ensure!(!key.is_empty(), "env-переменная {} задана, но пустая",
        tried.last().map(String::as_str).unwrap_or(api_key_env));
    Ok(Credentials {
        url: provider.effective_base_url(),
        key: key.to_string(),
        model: model.id.clone(),
    })
}

/// Заполненность контекста модели в процентах.
///
/// Диапазон 0.0..=100.0 при штатной работе; значение **больше** 100.0 —
/// честный сигнал переполнения (вызывающий код решает, жать ли компактификацию).
/// При нулевом лимите: 0.0, если ничего не использовано, иначе `f64::INFINITY`.
pub fn estimate_context_pct(used_tokens: usize, model: &ModelInfo) -> f64 {
    if model.context_limit == 0 {
        return if used_tokens == 0 { 0.0 } else { f64::INFINITY };
    }
    used_tokens as f64 * 100.0 / model.context_limit as f64
}

/// До `limit` ближайших к `id` идентификаторов моделей по Левенштейну.
///
/// Порог допуска — `len/2 + 1` редактирований: опечатки в 1–2 символа ловятся,
/// совсем чужие строки (напр. `"zzzz"`) возвращают пустой список.
/// Сортировка: по расстоянию, при равенстве — по имени (стабильный вывод).
pub fn nearest_models(id: &str, limit: usize) -> Vec<(String, usize)> {
    let threshold = id.chars().count() / 2 + 1;
    let mut scored: Vec<(String, usize)> = builtin_models()
        .into_iter()
        .map(|m| {
            let dist = levenshtein(id, &m.id);
            (m.id, dist)
        })
        .filter(|(_, dist)| *dist <= threshold)
        .collect();
    scored.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    scored.truncate(limit);
    scored
}

/// Ошибка «неизвестная модель» с подсказкой ближайших (или полным списком).
fn unknown_model_error(id: &str) -> anyhow::Error {
    let near = nearest_models(id, 3);
    let hint = if near.is_empty() {
        let all: Vec<String> = builtin_models().into_iter().map(|m| m.id).collect();
        format!("зарегистрированные модели: {}", all.join(", "))
    } else {
        let names: Vec<&str> = near.iter().map(|(name, _)| name.as_str()).collect();
        format!("похожие модели: {}", names.join(", "))
    };
    anyhow!("неизвестная модель «{id}»; {hint}")
}

/// Расстояние Левенштейна между строками (посимвольно, Unicode-aware).
///
/// Классический DP в две строки: память O(min не гарантирована) — O(len(b)),
/// время O(len(a) * len(b)). Для идентификаторов моделей (десятки символов)
/// этого более чем достаточно.
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    // prev[j] = расстояние префикса a[..i] до префикса b[..j]
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    // общий crate-wide замок env-тестов (гонки между модулями, flake 08.08)
    use crate::test_util::ENV_LOCK;
    use std::collections::HashSet;

    /// Выполнить `f` с временно выставленными env-переменными (None = удалить),
    /// затем вернуть прежние значения. Держит глобальную блокировку.
    fn with_env_vars<R>(vars: &[(&str, Option<&str>)], f: impl FnOnce() -> R) -> R {
        let _guard = ENV_LOCK.lock().unwrap();
        let saved: Vec<(&str, Option<String>)> =
            vars.iter().map(|(name, _)| (*name, env::var(name).ok())).collect();
        for &(name, value) in vars {
            match value {
                Some(v) => env::set_var(name, v),
                None => env::remove_var(name),
            }
        }
        let result = f();
        for (name, old) in saved {
            match old {
                Some(v) => env::set_var(name, v),
                None => env::remove_var(name),
            }
        }
        result
    }

    /// f64 сравниваем через эпсилон (clippy::float_cmp).
    fn assert_near(got: f64, want: f64) {
        assert!((got - want).abs() < 1e-9, "ожидалось {want}, получено {got}");
    }

    #[test]
    fn find_model_hits_and_misses() {
        let m = find_model("deepseek-v4-pro").unwrap();
        assert_eq!(m.provider, "deepseek");
        assert_eq!(m.context_limit, 131_072);
        assert_eq!(m.max_output, 32_768);
        assert!(m.supports_thinking && m.supports_tools);
        assert!(m.cost_hint.is_some());
        assert!(find_model("gpt-9000").is_none());
        assert!(find_model("").is_none());
    }

    #[test]
    fn registry_is_internally_consistent() {
        let providers = builtin_providers();
        let names: Vec<&str> = providers.iter().map(|p| p.name.as_str()).collect();
        for expected in ["deepseek", "kimi", "moonshot", "zhipu", "openai-compatible"] {
            assert!(names.contains(&expected), "нет провайдера {expected}");
        }
        for p in &providers {
            assert!(p.base_url.starts_with("http"), "{}: base_url", p.name);
            // версия API в хвосте пути: /v1 у большинства, /v4 у zhipu (z.ai)
            assert!(p.base_url.ends_with("/v1") || p.base_url.ends_with("/v4"),
                "{}: base_url без версии API в хвосте", p.name);
            assert!(p.requires_key(), "{}: ожидался env_key", p.name);
        }
        let models = builtin_models();
        assert_eq!(models.len(), 12);
        let mut seen = HashSet::new();
        for m in &models {
            assert!(seen.insert(m.id.as_str()), "дубликат id {}", m.id);
            assert!(
                names.contains(&m.provider.as_str()),
                "{}: нет провайдера {}",
                m.id,
                m.provider
            );
            assert!(m.context_limit > 0, "{}: нулевой контекст", m.id);
            assert!(
                m.max_output > 0 && m.max_output <= m.context_limit,
                "{}: max_output вне лимитов",
                m.id
            );
        }
    }

    #[test]
    fn moonshot_marked_with_dpi_risk() {
        let moon = find_provider("moonshot").unwrap();
        let note = moon.risk_note.as_deref().unwrap_or("");
        assert!(note.contains("DPI"), "note: {note}");
        assert!(find_provider("deepseek").unwrap().risk_note.is_none());
        assert!(find_provider("kimi").unwrap().risk_note.is_none());
        assert!(find_provider("нет-такого").is_none());
    }

    /// Flash и GLM-5.2 в реестре с правильными провайдерами и env-ключами
    /// (быстрый выбор /model: pro | flash | glm).
    #[test]
    fn flash_and_glm_registered_with_providers() {
        let flash = find_model("deepseek-v4-flash").expect("flash в реестре");
        assert_eq!(flash.provider, "deepseek");
        assert!(flash.supports_thinking && flash.supports_tools);
        let deepseek = find_provider("deepseek").unwrap();
        assert_eq!(deepseek.env_key.as_deref(), Some("DEEPSEEK_API_KEY"));

        let glm = find_model("glm-5.2").expect("glm-5.2 в реестре");
        assert_eq!(glm.provider, "zhipu");
        assert!(glm.supports_thinking && glm.supports_tools);
        let zhipu = find_provider("zhipu").expect("провайдер zhipu");
        assert_eq!(zhipu.env_key.as_deref(), Some("ZHIPU_API_KEY"));
        assert_eq!(zhipu.base_url, "https://api.z.ai/api/paas/v4");

        // Kimi K3 по докам Kimi Code (08.08): канонический id «k3»,
        // провайдер — coding-поверхность api.kimi.com/coding/v1
        let k3 = find_model("k3").expect("k3 в реестре");
        assert_eq!(k3.provider, "kimi");
        assert!(k3.supports_thinking && k3.supports_tools);
        // K3 принимает только temperature=1 (400 «only 1 is allowed»)
        assert_eq!(k3.temperature, Some(1.0));
        assert_eq!(find_model("k3-256k").unwrap().temperature, Some(1.0));
        // остальные модели — без принудительной температуры
        assert_eq!(find_model("deepseek-v4-pro").unwrap().temperature, None);
        let kimi = find_provider("kimi").expect("провайдер kimi");
        assert_eq!(kimi.base_url, "https://api.kimi.com/coding/v1");
        assert_eq!(kimi.env_key.as_deref(), Some("KIMI_API_KEY"));
    }

    #[test]
    fn wire_api_display_and_serde() {
        assert_eq!(WireApi::Chat.as_str(), "chat");
        assert_eq!(WireApi::Responses.to_string(), "responses");
        assert_eq!(serde_json::to_string(&WireApi::Chat).unwrap(), "\"chat\"");
        let parsed: WireApi = serde_json::from_str("\"responses\"").unwrap();
        assert_eq!(parsed, WireApi::Responses);
        assert!(builtin_providers().iter().all(|p| p.wire_api == WireApi::Chat));
    }

    #[test]
    fn resolve_ok_reads_key_from_env() {
        with_env_vars(
            &[("THESEUS_TEST_MODELS_KEY", Some("  sk-test-123  ")), ("DEEPSEEK_BASE_URL", None)],
            || {
                let creds = resolve_with_env("deepseek-chat", "THESEUS_TEST_MODELS_KEY").unwrap();
                assert_eq!(creds.url, "https://api.deepseek.com/v1");
                assert_eq!(creds.key, "sk-test-123"); // пробелы обрезаны
                assert_eq!(creds.model, "deepseek-chat");
            },
        );
    }

    #[test]
    fn resolve_errors_when_env_missing() {
        with_env_vars(&[("THESEUS_TEST_MODELS_MISSING", None)], || {
            let err =
                resolve_with_env("deepseek-chat", "THESEUS_TEST_MODELS_MISSING").unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("THESEUS_TEST_MODELS_MISSING"), "msg: {msg}");
        });
    }

    #[test]
    fn resolve_errors_when_key_empty() {
        with_env_vars(&[("THESEUS_TEST_MODELS_EMPTY", Some("   "))], || {
            let err = resolve_with_env("kimi-k2", "THESEUS_TEST_MODELS_EMPTY").unwrap_err();
            assert!(format!("{err:#}").contains("пустая"));
        });
    }

    /// Алиасы env-ключа (кейс 03.08): zhipu принимает ключ и из GLM_API_KEY /
    /// ZAI_API_KEY, а не только из ZHIPU_API_KEY; приоритет — у основного имени.
    #[test]
    fn resolve_falls_back_to_env_key_aliases() {
        with_env_vars(
            &[("ZHIPU_API_KEY", None), ("GLM_API_KEY", Some(" glm-key ")), ("ZAI_API_KEY", None)],
            || {
                let creds = resolve("glm-5.2").unwrap();
                assert_eq!(creds.key, "glm-key"); // пробелы обрезаны
                assert_eq!(creds.url, "https://api.z.ai/api/paas/v4");
            },
        );
        // основное имя сильнее алиаса
        with_env_vars(
            &[("ZHIPU_API_KEY", Some("main-key")), ("GLM_API_KEY", Some("alias-key"))],
            || {
                let creds = resolve("glm-5.2").unwrap();
                assert_eq!(creds.key, "main-key");
            },
        );
    }

    /// Ошибка без ключа перечисляет все принятые имена и подсказывает про
    /// окружение процесса (переменная, выставленная после запуска, не доезжает).
    #[test]
    fn resolve_error_lists_all_key_names_and_restart_hint() {
        with_env_vars(
            &[("ZHIPU_API_KEY", None), ("GLM_API_KEY", None), ("ZAI_API_KEY", None)],
            || {
                let err = resolve("glm-5.2").unwrap_err();
                let msg = format!("{err:#}");
                assert!(msg.contains("ZHIPU_API_KEY"), "msg: {msg}");
                assert!(msg.contains("GLM_API_KEY"), "msg: {msg}");
                assert!(msg.contains("ZAI_API_KEY"), "msg: {msg}");
                assert!(msg.contains("перезапустите"), "подсказка про перезапуск: {msg}");
            },
        );
    }

    #[test]
    fn resolve_unknown_model_suggests_nearest() {
        let err = resolve_with_env("deepseek-chatt", "THESEUS_TEST_MODELS_UNUSED").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("неизвестная модель"), "msg: {msg}");
        assert!(msg.contains("deepseek-chat"), "msg: {msg}");
    }

    #[test]
    fn resolve_unknown_model_far_off_lists_registry() {
        let err = resolve_with_env("zzzz", "THESEUS_TEST_MODELS_UNUSED").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("зарегистрированные модели"), "msg: {msg}");
        assert!(msg.contains("deepseek-v4-pro"), "msg: {msg}");
    }

    #[test]
    fn resolve_uses_provider_default_env() {
        with_env_vars(
            &[("DEEPSEEK_API_KEY", Some("sk-from-default-env")), ("DEEPSEEK_BASE_URL", None)],
            || {
                let creds = resolve("deepseek-v4-pro").unwrap();
                assert_eq!(creds.key, "sk-from-default-env");
                assert_eq!(creds.url, "https://api.deepseek.com/v1");
                assert_eq!(creds.model, "deepseek-v4-pro");
            },
        );
    }

    #[test]
    fn base_url_env_overrides_default() {
        let provider = find_provider("openai-compatible").unwrap();
        with_env_vars(&[("OPENAI_BASE_URL", None)], || {
            assert_eq!(provider.effective_base_url(), "http://localhost:8000/v1");
        });
        with_env_vars(&[("OPENAI_BASE_URL", Some("http://127.0.0.1:8765/v1/"))], || {
            // завершающий слеш срезается
            assert_eq!(provider.effective_base_url(), "http://127.0.0.1:8765/v1");
        });
        with_env_vars(&[("OPENAI_BASE_URL", Some("   "))], || {
            // пустое после trim значение игнорируется
            assert_eq!(provider.effective_base_url(), "http://localhost:8000/v1");
        });
    }

    #[test]
    fn levenshtein_distances() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", ""), 3);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("deepseek-chat", "deepseek-chatt"), 1);
        // посимвольно, не по байтам: кириллица считается символами
        assert_eq!(levenshtein("модель", "модели"), 1);
        assert_eq!(levenshtein("kimi-k2", "kimi-k3"), 1);
    }

    #[test]
    fn nearest_models_sorted_and_thresholded() {
        let near = nearest_models("kimi-k33", 3);
        assert!(!near.is_empty());
        assert_eq!(near[0].0, "kimi-k3");
        assert_eq!(near[0].1, 1);
        let mut sorted = near.clone();
        sorted.sort_by_key(|(_, dist)| *dist);
        assert_eq!(near, sorted, "расстояния должны идти по возрастанию");
        // совсем чужая строка — за порогом допуска
        assert!(nearest_models("zzzz", 3).is_empty());
    }

    #[test]
    fn context_pct_boundaries() {
        let m = find_model("deepseek-v4-pro").unwrap();
        assert_near(estimate_context_pct(0, &m), 0.0);
        assert_near(estimate_context_pct(65_536, &m), 50.0);
        assert_near(estimate_context_pct(131_072, &m), 100.0);
        // переполнение — честно больше 100
        assert_near(estimate_context_pct(262_144, &m), 200.0);
        assert_eq!(m.remaining_context(65_536), 65_536);
        assert_eq!(m.remaining_context(131_072), 0);
        assert_eq!(m.remaining_context(200_000), 0);
    }

    #[test]
    fn context_pct_zero_limit_guard() {
        let m = ModelInfo {
            id: "test-zero".into(),
            provider: "test".into(),
            context_limit: 0,
            max_output: 0,
            supports_thinking: false,
            supports_tools: false,
            cost_hint: None,
            temperature: None,
        };
        assert_near(estimate_context_pct(0, &m), 0.0);
        assert!(estimate_context_pct(5, &m).is_infinite());
    }

    #[test]
    fn cost_estimate_uses_hint() {
        let m = find_model("deepseek-chat").unwrap();
        let cost = m.estimate_cost_usd(1_000_000, 1_000_000).unwrap();
        assert_near(cost, 0.28 + 0.42);
        let bare = ModelInfo {
            id: "test-bare".into(),
            provider: "test".into(),
            context_limit: 1,
            max_output: 1,
            supports_thinking: false,
            supports_tools: false,
            cost_hint: None,
            temperature: None,
        };
        assert!(bare.estimate_cost_usd(10, 10).is_none());
    }

    /// Мягкий env-резолвер ключа (кейс 07.08): провайдерское имя приоритетнее
    /// общих; пробелы обрезаются; пустые значения не считаются ключом.
    #[test]
    fn api_key_from_env_prefers_provider_key() {
        with_env_vars(
            &[
                ("KIMI_API_KEY", Some(" sk-k3 ")),
                ("DEEPSEEK_API_KEY", Some("ds-key")),
                ("THESEUS_API_KEY", None),
            ],
            || {
                assert_eq!(api_key_from_env("k3").as_deref(), Some("sk-k3"));
            },
        );
    }

    #[test]
    fn api_key_from_env_alias_generic_fallback_and_empty() {
        // алиас провайдера (GLM_API_KEY) работает и через мягкий резолвер
        with_env_vars(
            &[("ZHIPU_API_KEY", None), ("GLM_API_KEY", Some("glm")), ("ZAI_API_KEY", None)],
            || assert_eq!(api_key_from_env("glm-5.2").as_deref(), Some("glm")),
        );
        // неизвестная модель — только общие исторические имена
        with_env_vars(&[("DEEPSEEK_API_KEY", Some("ds"))], || {
            assert_eq!(api_key_from_env("custom-local-model").as_deref(), Some("ds"));
        });
        // пустая/пробельная переменная не считается; ничего нет — None
        with_env_vars(
            &[
                ("KIMI_API_KEY", Some("   ")),
                ("DEEPSEEK_API_KEY", None),
                ("THESEUS_API_KEY", None),
            ],
            || assert_eq!(api_key_from_env("k3"), None),
        );
    }

    #[test]
    fn api_key_env_names_ordered_and_deduplicated() {
        let names = api_key_env_names("k3");
        assert_eq!(names[0], "KIMI_API_KEY");
        assert!(names.contains(&"DEEPSEEK_API_KEY".to_string()));
        let unique: HashSet<_> = names.iter().collect();
        assert_eq!(unique.len(), names.len(), "дубли: {names:?}");
        // у deepseek общие имена совпадают с провайдерскими — дублей быть не должно
        let ds = api_key_env_names("deepseek-v4-pro");
        assert_eq!(ds, vec!["DEEPSEEK_API_KEY".to_string(), "THESEUS_API_KEY".to_string()]);
        // неизвестная модель — только общие
        assert_eq!(
            api_key_env_names("no-such-model"),
            vec!["DEEPSEEK_API_KEY".to_string(), "THESEUS_API_KEY".to_string()]
        );
    }
}
