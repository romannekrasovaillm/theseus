//! Трёхуровневая компактификация контекста (OpenDev ACC + триггеры Grok).

use super::events::AgentEvent;
use super::{est_tokens, fingerprint, Agent};
use crate::api::Message;
use crate::hooks_ext::HookEvent as ExtHookEvent;
use crate::trace::SpanId;
use anyhow::Result;

impl Agent {
    /// Трёхуровневая компактификация (OpenDev ACC + триггеры Grok):
    /// L1 маскирование (70%) → L2 прунинг+дедуп (80%) → L3 LLM-саммари (95%).
    /// `parent` — спан текущего хода, к нему родительствуются compact-спаны.
    /// PreCompact/PostCompact (hooks_ext) обрамляют весь эпизод компактификации
    /// ровно один раз (порог L1 пройден раньше остальных — он и есть «вход»).
    pub(crate) fn maybe_compact(&mut self, messages: &mut Vec<Message>, parent: Option<SpanId>) -> Result<()> {
        // стадия 0: переразмерные tool-сообщения маскируются ВСЕГДА, до порогов —
        // один гигант в истории (баг 06.08) раздувает est и валит запрос 413
        let oversized = mask_oversized_tool_outputs(messages);
        if oversized > 0 {
            self.emit(AgentEvent::HookNote(format!(
                "⤓ маскирование переразмера: {oversized} tool-сообщений (>32 КБ)")));
        }
        let limit = self.context_limit;
        let est = est_tokens(messages).max(self.last_prompt);
        let engaged = est >= limit * self.compact_mask_pct / 100;
        if engaged {
            self.fire_ext(ExtHookEvent::PreCompact,
                serde_json::json!({"est_tokens": est, "context_limit": limit}));
        }

        // L1 (70%): маскирование старых tool-выводов — дёшево и без потерь пар
        if engaged {
            let masked = mask_old_tool_outputs(messages, 6, 300);
            if masked > 0 {
                // спан compact: from/to — оценка токенов до/после уровня
                let sp = self.trace.open_span("compact", parent);
                self.trace.attr(sp, "level", "L1");
                self.trace.attr(sp, "from", &est.to_string());
                self.trace.attr(sp, "to", &est_tokens(messages).max(self.last_prompt).to_string());
                self.trace.close_span(sp);
                self.emit(AgentEvent::HookNote(format!(
                    "⤓ L1 маскирование: {masked} tool-выводов ({}% окна)", self.compact_mask_pct)));
            }
        }

        // L2 (80%): дедуп повторных чтений (точный + семантический) + прунинг старых
        // tool-результатов (пары не рвём)
        let est2 = est_tokens(messages).max(self.last_prompt);
        if est2 >= limit * self.compact_prune_pct / 100 {
            let dups = dedupe_tool_results(messages);
            let sem_dups = dedupe_tool_results_semantic(messages);
            let pruned = prune_tool_results(messages, 6);
            if dups + sem_dups + pruned > 0 {
                let sp = self.trace.open_span("compact", parent);
                self.trace.attr(sp, "level", "L2");
                self.trace.attr(sp, "from", &est2.to_string());
                self.trace.attr(sp, "to", &est_tokens(messages).max(self.last_prompt).to_string());
                self.trace.close_span(sp);
                self.emit(AgentEvent::HookNote(format!(
                    "⤓ L2 прунинг: {pruned} результатов, дедупов: {dups}, сем-дедупов: {sem_dups} ({}% окна)", self.compact_prune_pct)));
            }
        }

        // L3 (95%): полная LLM-суммаризация (Claude 9 секций + перенос provider-overhead)
        let est3 = est_tokens(messages).max(self.last_prompt);
        let l3_threshold = limit * self.compact_summary_pct / 100;
        // QA-STRESS-01: при лимите меньше базового контекста (~2500 ток. — системный
        // промпт плюс удерживаемый хвост) L3 не способна опустить est ниже порога
        // и зацикливается по API-вызову на каждый ход. Если предыдущая L3 не опустила
        // est ниже порога, помечаем её бесполезной и пропускаем на следующих ходах.
        let l3 = (est3 >= l3_threshold && !self.l3_futile).then(|| {
            let res = self.llm_compact(messages, parent);
            if res.is_ok() && est_tokens(messages).max(self.last_prompt) >= l3_threshold {
                self.l3_futile = true;
            }
            res
        });
        if engaged {
            // PostCompact — в любом исходе, включая ошибку L3 (симметрия с PreCompact)
            self.fire_ext(ExtHookEvent::PostCompact,
                serde_json::json!({"est_tokens": est_tokens(messages).max(self.last_prompt),
                    "l3": l3.is_some(), "ok": l3.as_ref().is_none_or(Result::is_ok)}));
        }
        if let Some(res) = l3 {
            res?;
        }
        Ok(())
    }

    /// L3: LLM-суммаризация старого в одно сообщение (границы tool-пар не нарушаются).
    /// Спан compact (level=L3): from/to — число сообщений до/после (как в AgentEvent::Compact);
    /// при ошибке API спан закрывается с атрибутом error (ручной scopeguard).
    pub(crate) fn llm_compact(&mut self, messages: &mut Vec<Message>, parent: Option<SpanId>) -> Result<()> {
        let keep_tail = 6.min(messages.len().saturating_sub(2));
        let mut cut = messages.len().saturating_sub(keep_tail);
        // нельзя резать между assistant(tool_calls) и его tool-результатами
        while cut < messages.len() && messages[cut].role == "tool" { cut += 1; }
        if cut <= 1 || cut >= messages.len() { return Ok(()); }
        let old: Vec<Message> = messages[1..cut].to_vec();
        let from = messages.len();
        let sp = self.trace.open_span("compact", parent);
        self.trace.attr(sp, "level", "L3");
        self.trace.attr(sp, "from", &from.to_string());
        let sum_prompt = vec![
            Message::system("Summarize the conversation so far for another LLM that continues the task. \
                Structure: 1. Primary request and intent; 2. Key technical concepts; 3. Files and code sections \
                (paths); 4. Errors and fixes; 5. Problem solving; 6. All user messages (verbatim); \
                7. Pending tasks; 8. Current work; 9. Optional next step (quote the exact last exchange). \
                NO tool calls. Under 500 words."),
            Message::user(serde_json::to_string_pretty(&old)?),
        ];
        // стрим-режим, как везде в харнесс (run_turn): endpoint тот же, а моки
        // (mock_sse) говорят только SSE; дельты саммари в UI не выводим
        let resp = match self.api.chat_stream(&sum_prompt, &serde_json::Value::Null, &mut |_| {}, &|| false) {
            Ok(r) => r,
            Err(e) => {
                self.trace.attr(sp, "error", &format!("{e:#}"));
                self.trace.close_span(sp);
                return Err(e);
            }
        };
        // цепочку рассуждений саммари тоже прикрепляем: DeepSeek thinking+tools
        // требует reasoning_content на КАЖДОМ assistant-сообщении истории
        // (доки thinking_mode#tool-calls), голая вставка саммари роняла
        // следующий запрос с 400 «reasoning_content must be passed back»
        // (живой баг 25.08, сессия 1787226001: L3 318→8 → 400 без ретрая)
        let summary_reasoning = resp.reasoning.clone();
        let summary = resp.content.unwrap_or_else(|| "(пустая суммаризация)".into());
        let mut rebuilt = vec![messages[0].clone()];
        rebuilt.push(Message::assistant(Some(format!("CONTEXT COMPACTED ({from} сообщений → саммари): {summary}")), None)
            .with_reasoning(summary_reasoning));
        rebuilt.extend_from_slice(&messages[cut..]);
        *messages = rebuilt;
        // перенос provider-overhead (урок Grok): калибруем счётчик заново, чтобы не зациклиться
        self.last_prompt = est_tokens(messages);
        self.trace.attr(sp, "to", &messages.len().to_string());
        self.trace.close_span(sp);
        self.emit(AgentEvent::Compact { from_msgs: from, to_msgs: messages.len() });
        Ok(())
    }


}

/// Стадия 1 прогрессивной компакции (OpenDev ACC): маскирование старых tool-выводов
fn mask_old_tool_outputs(messages: &mut [Message], keep_last: usize, max_chars: usize) -> usize {
    let mut masked = 0;
    let cutoff = messages.len().saturating_sub(keep_last);
    for m in messages.iter_mut().take(cutoff) {
        if m.role != "tool" { continue; }
        let Some(c) = m.content.as_mut() else { continue; };
        if c.starts_with("[masked]") || c.chars().count() <= max_chars { continue; }
        let head: String = c.chars().take(max_chars).collect();
        *c = format!("[masked] {head} …(урезано харнессом)");
        masked += 1;
    }
    masked
}

/// Потолок одного tool-сообщения в истории (байт): превышение маскируется
/// независимо от «свежести» — один гигантский tool-результат (37,9 МБ из
/// grep по всему $HOME, баг 06.08) раздувал est до 9,69M и валил запрос 413.
const TOOL_MSG_HARD_CAP_BYTES: usize = 32 * 1024;

/// Маскирование ПЕРЕРАЗМЕРНЫХ tool-сообщений (любой позиции): L1 режет по
/// «старости» и не видит гиганта в свежем хвосте, а 413 от него — именно
/// там. Голова сообщения сохраняется, хвост заменяется честным маркером.
/// Возвращает число замаскированных.
pub(crate) fn mask_oversized_tool_outputs(messages: &mut [Message]) -> usize {
    let mut masked = 0;
    for m in messages.iter_mut() {
        if m.role != "tool" { continue; }
        let Some(c) = m.content.as_mut() else { continue; };
        if c.len() <= TOOL_MSG_HARD_CAP_BYTES || c.starts_with("[masked]") { continue; }
        let mut head_end = TOOL_MSG_HARD_CAP_BYTES.min(c.len());
        while !c.is_char_boundary(head_end) { head_end -= 1; }
        let skipped = c.len() - head_end;
        let head = &c[..head_end];
        *c = format!("[masked] {head}\n…(урезано харнессом: {skipped} байт переразмера)");
        masked += 1;
    }
    masked
}

/// L2: дедуп идентичных tool-результатов (agentnye-harnessy 6.3: дедупликация повторных чтений)
fn dedupe_tool_results(messages: &mut [Message]) -> usize {
    let mut seen = std::collections::HashMap::new();
    let mut dups = 0;
    for m in messages.iter_mut() {
        if m.role != "tool" { continue; }
        let Some(c) = m.content.as_mut() else { continue; };
        if c.len() < 200 || c.starts_with("[dedup]") || c.starts_with("[pruned]") { continue; }
        let fp = fingerprint("tool_result", &serde_json::json!(c));
        if let std::collections::hash_map::Entry::Vacant(e) = seen.entry(fp) {
            e.insert(());
        } else {
            *c = format!("[dedup] идентичный результат уже был выше ({} байт)", c.len());
            dups += 1;
        }
    }
    dups
}

/// L2b: семантический дедуп ПОХОЖИХ tool-результатов (compact_v2::simhash64) —
/// повторное чтение слегка изменённого файла не ловится точным fingerprint'ом.
/// Порог — DEFAULT_HAMMING_THRESHOLD бит; дорогая стадия (simhash ~1 мс/10 КБ),
/// поэтому только после точного дедупа и только для результатов ≥500 байт.
fn dedupe_tool_results_semantic(messages: &mut [Message]) -> usize {
    let mut seen: Vec<(u64, usize)> = Vec::new(); // (simhash, индекс сообщения)
    let mut dups = 0;
    for (i, m) in messages.iter_mut().enumerate() {
        if m.role != "tool" { continue; }
        let Some(c) = m.content.as_mut() else { continue; };
        if c.len() < 500 || c.starts_with('[') { continue; } // заглушки L1/L2 пропускаем
        let h = crate::compact_v2::simhash64(c);
        if let Some(&(_, j)) = seen.iter()
            .find(|(ph, _)| crate::compact_v2::hamming(*ph, h) <= crate::compact_v2::DEFAULT_HAMMING_THRESHOLD)
        {
            *c = format!("[dedup~] похожий результат уже был выше (сообщение #{j}, {} байт)", c.len());
            dups += 1;
        } else {
            seen.push((h, i));
        }
    }
    dups
}

/// L2: прунинг старых tool-результатов с сохранением пар (замена на заглушку, не удаление)
fn prune_tool_results(messages: &mut [Message], keep_last: usize) -> usize {
    let mut pruned = 0;
    let cutoff = messages.len().saturating_sub(keep_last);
    for m in messages.iter_mut().take(cutoff) {
        if m.role != "tool" { continue; }
        let Some(c) = m.content.as_mut() else { continue; };
        if c.starts_with("[pruned]") { continue; }
        if c.len() < 150 { continue; }
        *c = format!("[pruned] tool result dropped ({} bytes)", c.len());
        pruned += 1;
    }
    pruned
}

#[cfg(test)]
mod compact_tests {
    use super::*;
    use crate::api::Message;

    fn tool_msg(s: &str) -> Message { Message::tool("id1", s.to_string().repeat(30)) }

    /// Регрессия (живой баг 25.08, сессия 1787226001): после компактификации
    /// (L1+L2+L3) КАЖДОЕ assistant-сообщение истории обязано нести непустой
    /// reasoning_content — контракт DeepSeek thinking+tools, иначе следующий
    /// запрос падает с HTTP 400 «reasoning_content must be passed back» без
    /// ретрая. Хвост L3 клонируется с цепочками, вставка саммари получает
    /// цепочку ответа-суммаризатора.
    #[test]
    fn compaction_preserves_reasoning_passback_contract() {
        use crate::api::ToolCall;
        use crate::config::{Config, PermissionConfig};
        use crate::mock_sse::{MockLlm, Scenario};
        use crate::permissions::{Mode, PermissionEngine};

        // суммаризатор отвечает с цепочкой рассуждений, как thinking-модель
        let handle = MockLlm::with_scenarios(vec![
            Scenario::new().reply_reasoning(&["обдумываю саммари"]).reply_text("краткое саммари"),
        ]).serve_on_ephemeral().expect("мок поднялся");
        let ws = std::env::temp_dir().join(format!(
            "theseus_reason_passback_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
        ));
        std::fs::create_dir_all(&ws).expect("временный workspace");
        let cfg = Config {
            model: "mock-model".into(),
            base_url: Some(handle.base_url.clone()),
            api_key: Some("test-key".into()),
            context_limit_tokens: 2_000,
            max_output_tokens: 4_096,
            api_timeout_secs: 30,
            extra_body: serde_json::json!({}),
            permission: PermissionConfig::default(),
            mcp_servers: vec![],
            permission_rules: vec![],
            hooks: vec![],
            skill_dirs: vec![],
            web_allowed_domains: vec![],
            sandbox: false,
            compact_mask_pct: 70,
            compact_prune_pct: 80,
            compact_summary_pct: 95,
            reasoning_effort: "high".into(),
        };
        let perms = PermissionEngine::new(Mode::Yolo, cfg.permission.clone(), &ws);
        let mut agent = Agent::new(cfg, perms, &ws, 4, None).expect("агент создаётся");

        // история с интерливинговым ризонингом: assistant(tool_calls+reasoning)
        // + tool-результаты, объёма хватает на срабатывание всех трёх уровней
        let big = "x".repeat(600);
        let mut msgs = vec![Message::system("s")];
        for i in 0..12 {
            msgs.push(Message::assistant(Some(big.clone()), Some(vec![ToolCall {
                id: format!("call_{i}"),
                kind: "function".into(),
                function: crate::api::ToolFunction { name: "read_file".into(), arguments: "{}".into() },
            }])).with_reasoning(Some(format!("рассуждение {i}"))));
            msgs.push(Message::tool(format!("call_{i}"), big.clone()));
        }
        agent.maybe_compact(&mut msgs, None).expect("компактификация");
        assert!(msgs.iter().any(|m| m.role == "assistant"
            && m.content.as_deref().is_some_and(|c| c.starts_with("CONTEXT COMPACTED"))),
            "L3 реально отработала");
        for (i, m) in msgs.iter().enumerate() {
            if m.role == "assistant" {
                assert!(m.reasoning_content.as_deref().is_some_and(|r| !r.is_empty()),
                    "assistant #{i} (tools={}) без reasoning_content после компактификации",
                    m.tool_calls.is_some());
            }
        }
        // вставка саммари несёт цепочку суммаризатора, хвост — исходные цепочки
        assert_eq!(msgs[1].reasoning_content.as_deref(), Some("обдумываю саммари"));
        assert!(msgs.iter().any(|m| m.reasoning_content.as_deref() == Some("рассуждение 11")),
            "хвост сохранил свежие цепочки");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn dedupe_replaces_second_identical() {
        let mut msgs = vec![
            Message::user("a".repeat(50).as_str()),
            tool_msg("same-content "),
            Message::assistant(Some("x".into()), None),
            tool_msg("same-content "),
        ];
        let n = dedupe_tool_results(&mut msgs);
        assert_eq!(n, 1);
        assert!(msgs[3].content.as_ref().unwrap().starts_with("[dedup]"));
        assert!(!msgs[1].content.as_ref().unwrap().starts_with("[dedup]"));
    }

    #[test]
    fn prune_keeps_pairs_and_tail() {
        let mut msgs = vec![
            Message::system("s"),
            Message::assistant(Some("call".into()), None),
            tool_msg("big-result "),
            tool_msg("big-result2 "),
            Message::user("u"),
            Message::assistant(Some("a".into()), None),
            tool_msg("tail "),
        ];
        let n = prune_tool_results(&mut msgs, 1);
        assert_eq!(n, 2);
        assert!(msgs[2].content.as_ref().unwrap().starts_with("[pruned]"));
        assert!(!msgs[6].content.as_ref().unwrap().starts_with("[pruned]"));
        // роли не удалены — пары не нарушены
        assert_eq!(msgs[2].role, "tool");
    }

    /// Стадия 0 (баг 06.08): переразмерное tool-сообщение маскируется в ЛЮБОЙ
    /// позиции (свежий хвост не спасает), голова сохраняется с честным
    /// маркером; мелкие и уже замаскированные не трогаем.
    #[test]
    fn mask_oversized_tool_outputs_caps_giants_anywhere() {
        let giant = "x".repeat(40 * 1024);
        let mut msgs = vec![
            Message::system("s"),
            Message::assistant(Some("call".into()), None),
            Message::tool("c1", "маленький результат"),
            Message::tool("c2", giant),
            Message::user("u"),
            Message::tool("c3", "[masked] уже урезан ".to_string() + &"y".repeat(40 * 1024)),
        ];
        let n = mask_oversized_tool_outputs(&mut msgs);
        assert_eq!(n, 1, "замаскирован только гигант: {n}");
        let c2 = msgs[3].content.as_ref().unwrap();
        assert!(c2.starts_with("[masked] "), "{c2:.40}");
        assert!(c2.contains("урезано харнессом"), "маркер: {c2:.60}");
        assert!(c2.len() < 34 * 1024, "размер после урезки: {}", c2.len());
        assert_eq!(msgs[2].content.as_deref(), Some("маленький результат"));
        assert!(msgs[5].content.as_ref().unwrap().starts_with("[masked] уже урезан"));
    }

    /// L2b: похожие (не идентичные) повторные чтения ловятся simhash-дедупом.
    #[test]
    fn semantic_dedupe_catches_near_duplicate_reads() {
        // два прочтения одного файла до/после правки одной строки — exact match нет
        let v1: String = (1..80).map(|i| format!("строка {i}: содержимое конфигурации системы\n")).collect();
        let v2 = v1.replace("строка 40", "строка 40 ИЗМЕНЕНА");
        let mut msgs = vec![
            Message::system("s"),
            Message::tool("id1", v1),
            Message::tool("id2", v2),
        ];
        let n = dedupe_tool_results_semantic(&mut msgs);
        assert_eq!(n, 1);
        assert!(msgs[2].content.as_ref().unwrap().starts_with("[dedup~]"));
        assert!(!msgs[1].content.as_ref().unwrap().starts_with("[dedup~]"));
    }

    /// L2b: разные по смыслу результаты не задеваются; мелкие — пропускаются.
    #[test]
    fn semantic_dedupe_ignores_dissimilar_and_small() {
        let a: String = (1..80).map(|i| format!("строка {i}: совершенно иной текст про погоду\n")).collect();
        let b: String = (1..80).map(|i| format!("record {i}: database migration log output here\n")).collect();
        let mut msgs = vec![
            Message::tool("id1", a),
            Message::tool("id2", b),
            Message::tool("id3", "короткий".repeat(10)),
        ];
        let n = dedupe_tool_results_semantic(&mut msgs);
        assert_eq!(n, 0);
        assert!(!msgs[0].content.as_ref().unwrap().starts_with("[dedup~]"));
        assert!(!msgs[1].content.as_ref().unwrap().starts_with("[dedup~]"));
    }

    /// QA-STRESS-01: при лимите меньше базового контекста L3 обязана сработать
    /// один раз, признаться бесполезной (est не опускается ниже порога) и дальше
    /// пропускаться — иначе она жжёт по API-вызову на каждый ход бесконечно.
    /// Проверка — по счётчику API-вызовов и журналу запросов мока.
    #[test]
    fn l3_futile_stops_repeat_compaction_below_floor() {
        use crate::config::{Config, PermissionConfig};
        use crate::mock_sse::{MockResponse, MockServer};
        use crate::permissions::{Mode, PermissionEngine};

        // lenient-мок: один сценарий саммари; повторный вызов L3 получил бы
        // запасной ответ и накрутил счётчик — тест это и ловит.
        let server = MockServer::start(vec![MockResponse::text("краткое саммари контекста")])
            .expect("мок поднялся");
        let ws = std::env::temp_dir().join(format!(
            "theseus_l3_futile_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
        ));
        std::fs::create_dir_all(&ws).expect("временный workspace");
        let cfg = Config {
            model: "mock-model".into(),
            base_url: Some(format!("http://127.0.0.1:{}", server.port())),
            api_key: Some("test-key".into()),
            context_limit_tokens: 2_000,
            max_output_tokens: 4_096,
            api_timeout_secs: 30,
            extra_body: serde_json::json!({}),
            permission: PermissionConfig::default(),
            mcp_servers: vec![],
            permission_rules: vec![],
            hooks: vec![],
            skill_dirs: vec![],
            web_allowed_domains: vec![],
            sandbox: false,
            compact_mask_pct: 70,
            compact_prune_pct: 80,
            compact_summary_pct: 95,
            reasoning_effort: "high".into(),
        };
        let perms = PermissionEngine::new(Mode::Yolo, cfg.permission.clone(), &ws);
        let mut agent = Agent::new(cfg, perms, &ws, 4, None).expect("агент создаётся");

        // Базовый контекст выше порога L3 (95% от 2000 = 1900): 10 сообщений
        // по 1300 байт, хвост из 6 (≈1950 ток.) удерживается llm_compact и сам
        // по себе выше порога — опустить est ниже 1900 невозможно.
        // Сообщений-role «tool» нет — стадии L1/L2 остаются инертными.
        let big = "d".repeat(1300);
        let mut msgs = vec![Message::system("s")];
        for i in 0..10 {
            if i % 2 == 0 {
                msgs.push(Message::user(big.clone()));
            } else {
                msgs.push(Message::assistant(Some(big.clone()), None));
            }
        }
        assert!(est_tokens(&msgs) >= 2_000 * 95 / 100, "стартовый est выше порога L3");

        agent.maybe_compact(&mut msgs, None).expect("первая компактификация");
        assert_eq!(agent.api.accounting.calls, 1, "L3 сработала ровно один раз");
        assert!(agent.l3_futile, "неэффективная L3 помечается бесполезной");
        assert!(est_tokens(&msgs).max(agent.last_prompt) >= 2_000 * 95 / 100,
            "est остался выше порога — повод для цикла есть, пропуск именно по флагу");

        // следующие ходы: L3 больше не вызывается, несмотря на est выше порога
        agent.maybe_compact(&mut msgs, None).expect("вторая компактификация");
        agent.maybe_compact(&mut msgs, None).expect("третья компактификация");
        assert_eq!(agent.api.accounting.calls, 1, "повторных вызовов L3 нет");
        assert_eq!(server.requests().len(), 1, "на мок ушёл ровно один HTTP-запрос");
        std::fs::remove_dir_all(&ws).ok();
    }
}

