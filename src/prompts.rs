//! Сборщик системного промпта (по образцу `codex-rs/prompts` + `core`).
//!
//! Системный промпт — плоский markdown-документ, собранный из секций
//! в ЖЁСТКО заданном порядке (приоритет сверху вниз; самое важное для
//! текущего состояния — ближе к концу, эффект recency у LLM):
//!
//! 1. `Base instructions` — роль агента, правила tool-use, стиль ответов.
//! 2. `Environment` — ОС, shell, cwd, дата, git-ветка (аналог `environment_context` из codex core).
//! 3. `AGENTS.md` — склейка слоёв (глобальный + workspace) с подзаголовками.
//! 4. `Available skills` — дайджест скиллов (имя + краткое описание).
//! 5. `Plan mode` — ограничения read-only фазы (только когда режим включён).
//! 6. `Goal` — цель сессии (последней: максимальная заметность для модели).
//!
//! Правила сборки:
//! - пустые (после trim) секции пропускаются целиком;
//! - секции склеиваются одной пустой строкой, каждая начинается с заголовка `##`;
//! - каждый слой AGENTS.md обрезается до лимита СИМВОЛОВ с явной пометкой;
//! - вся обрезка посимвольная (не побайтовая) — безопасна для кириллицы.

use std::path::Path;

/// Базовые инструкции агента по умолчанию: роль + правила tool-use + стиль ответов.
/// Собраны по мотивам `gpt_5_codex_prompt.md` (codex-rs/core) и действующего
/// `SYSTEM_PROMPT` theseus (`agent/mod.rs`).
pub const DEFAULT_BASE_INSTRUCTIONS: &str = "## Base instructions

You are Theseus, an autonomous coding agent running inside a TUI harness on the user's machine.

### Tool use rules

- Work only inside the workspace unless the user explicitly directs otherwise.
- Read with `read_file` / `list_files` / `grep`; edit with `write_file` / `edit_file`; run commands with `bash`. Do NOT use bash `echo`/`cat` to read or write files when a dedicated tool exists.
- Before non-trivial work, keep a plan via `todo_write` (exactly one item `in_progress`). Call `finish(summary)` only when the task is actually done.
- Prefer `rg` / `rg --files` over `grep` / `find` when searching through bash.
- You may be in a dirty git worktree: never revert or overwrite changes you did not make.
- Never run destructive or hard-to-reverse commands (`rm -rf`, `git reset --hard`, force-push) unless the user explicitly approved them.

### Response style

- Be concise and factual; lead with the outcome, then the details.
- Reference code as `path/to/file.rs:42`; cite paths instead of dumping whole files.
- Light Markdown only: short paragraphs, `-` bullets, backticks for commands/paths/code.
- Mirror the user's language in prose; keep code and identifiers in their original form.";

/// Лимит длины одного слоя AGENTS.md по умолчанию — в СИМВОЛАХ (не байтах).
pub const DEFAULT_AGENTS_MD_LIMIT: usize = 8_000;

/// Максимум скиллов, перечисляемых в дайджесте системного промпта.
pub const MAX_SKILLS_IN_DIGEST: usize = 50;

/// Максимум символов описания скилла в дайджесте (дальше — усечение с `...`).
pub const SKILL_DESC_MAX_CHARS: usize = 80;

/// Сколько уровней вверх от стартового каталога ищем `.git/HEAD` при определении ветки.
const GIT_SEARCH_DEPTH: usize = 8;

/// Секция plan-режима: добавляется в промпт только когда режим включён.
const PLAN_MODE_SECTION: &str = "## Plan mode

Plan mode is ON — this is a read-only phase:

- Explore, read and analyze, but do NOT modify files, run mutating commands or change external state.
- Work out an explicit step-by-step plan (`todo_write`) and present it before any execution.
- Leave plan mode only after the user approves the plan.";

/// Контекст окружения для секции `## Environment` (аналог `environment_context` из codex core).
///
/// Все поля — plain data; пустые (после trim) поля в рендер не попадают.
/// Дата намеренно строкой: формат выбирает вызывающая сторона (конвенция theseus — `YYYY-MM-DD`).
#[derive(Debug, Clone, Default)]
pub struct EnvContext {
    /// ОС (например, `linux`).
    pub os: String,
    /// Шелл пользователя (например, `/bin/bash`).
    pub shell: String,
    /// Текущий рабочий каталог (workspace).
    pub cwd: String,
    /// Дата сеанса в формате вызывающей стороны.
    pub date: String,
    /// Текущая ветка git, если удалось определить.
    pub git_branch: Option<String>,
}

impl EnvContext {
    /// Снять контекст с текущего процесса: ОС из `std::env::consts`, шелл из `$SHELL`,
    /// cwd из `current_dir`, ветку git — чтением `.git/HEAD` (без запуска процессов).
    /// Дата передаётся вызывающей стороной (как и в `memory.rs` theseus).
    pub fn detect(date: impl Into<String>) -> Self {
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let git_branch = if cwd.is_empty() {
            None
        } else {
            git_branch_near(Path::new(&cwd))
        };
        EnvContext {
            os: std::env::consts::OS.to_string(),
            shell: std::env::var("SHELL").unwrap_or_default(),
            cwd,
            date: date.into(),
            git_branch,
        }
    }

    /// Отрендерить секцию `## Environment`; `None`, если все поля пусты.
    /// Публично: харнесс может отправлять окружение отдельным сообщением (как codex),
    /// а не только внутри общего системного промпта.
    pub fn render(&self) -> Option<String> {
        let mut lines: Vec<String> = Vec::new();
        for (label, value) in [
            ("OS", self.os.trim()),
            ("Shell", self.shell.trim()),
            ("CWD", self.cwd.trim()),
            ("Date", self.date.trim()),
        ] {
            if !value.is_empty() {
                lines.push(format!("- {label}: {value}"));
            }
        }
        if let Some(branch) = &self.git_branch {
            let branch = branch.trim();
            if !branch.is_empty() {
                lines.push(format!("- Git branch: {branch}"));
            }
        }
        if lines.is_empty() {
            return None;
        }
        Some(format!("## Environment\n\n{}", lines.join("\n")))
    }
}

/// Дайджест скилла для системного промпта: имя + краткое описание.
///
/// Намеренно самодостаточная структура: чтобы не зависеть от `crate::skills`,
/// вызывающая сторона проецирует свои спецификации скиллов в этот тип.
#[derive(Debug, Clone, Default)]
pub struct SkillDigest {
    /// Имя скилла (то, что передают в инструмент `skill`).
    pub name: String,
    /// Однострочное описание; в дайджесте усекается до [`SKILL_DESC_MAX_CHARS`].
    pub desc: String,
}

impl SkillDigest {
    /// Создать дайджест из имени и описания.
    pub fn new(name: impl Into<String>, desc: impl Into<String>) -> Self {
        SkillDigest { name: name.into(), desc: desc.into() }
    }
}

/// Сборщик системного промпта (builder-стиль).
///
/// Порядок секций фиксирован (см. документацию модуля), пустые секции
/// пропускаются. `new()` стартует с [`DEFAULT_BASE_INSTRUCTIONS`];
/// `empty()` — полностью пустой сборщик для кастомных сценариев.
///
/// ```
/// use theseus::prompts::{EnvContext, PromptBuilder, SkillDigest};
///
/// let prompt = PromptBuilder::new()
///     .env(EnvContext { os: "linux".into(), cwd: "/repo".into(), ..Default::default() })
///     .skills(&[SkillDigest::new("demo-skill", "тестовый скилл")])
///     .goal(Some("доделать задачу".to_string()))
///     .build();
/// assert!(prompt.contains("## Environment"));
/// assert!(prompt.contains("- demo-skill: тестовый скилл"));
/// ```
#[derive(Debug, Clone)]
pub struct PromptBuilder {
    base: String,
    env: Option<EnvContext>,
    agents_global: String,
    agents_workspace: String,
    agents_md_limit: usize,
    skills: Vec<SkillDigest>,
    plan_mode: bool,
    goal: Option<String>,
}

impl Default for PromptBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl PromptBuilder {
    /// Сборщик с базовыми инструкциями по умолчанию и лимитом [`DEFAULT_AGENTS_MD_LIMIT`].
    pub fn new() -> Self {
        PromptBuilder {
            base: DEFAULT_BASE_INSTRUCTIONS.to_string(),
            env: None,
            agents_global: String::new(),
            agents_workspace: String::new(),
            agents_md_limit: DEFAULT_AGENTS_MD_LIMIT,
            skills: Vec::new(),
            plan_mode: false,
            goal: None,
        }
    }

    /// Полностью пустой сборщик: даже базовые инструкции отключены.
    pub fn empty() -> Self {
        PromptBuilder { base: String::new(), ..Self::new() }
    }

    /// Заменить базовые инструкции. Пустая (после trim) строка — секция пропускается.
    pub fn base(mut self, text: impl Into<String>) -> Self {
        self.base = text.into();
        self
    }

    /// Задать контекст окружения (`## Environment`).
    pub fn env(mut self, env: EnvContext) -> Self {
        self.env = Some(env);
        self
    }

    /// Задать слои AGENTS.md: глобальный (`~/.kimi-code/AGENTS.md`) и workspace.
    /// Пустой (после trim) слой считается отсутствующим.
    pub fn agents_md(mut self, global: impl Into<String>, workspace: impl Into<String>) -> Self {
        self.agents_global = global.into();
        self.agents_workspace = workspace.into();
        self
    }

    /// Переопределить лимит длины слоя AGENTS.md (в символах).
    /// Лимит 0 допустим: любой непустой слой будет заменён одной пометкой об обрезке.
    pub fn agents_md_limit(mut self, limit: usize) -> Self {
        self.agents_md_limit = limit;
        self
    }

    /// Задать дайджест скиллов (заменяет ранее установленный список).
    pub fn skills(mut self, skills: &[SkillDigest]) -> Self {
        self.skills = skills.to_vec();
        self
    }

    /// Включить (`true`) / выключить (`false`) секцию plan-режима.
    pub fn plan_mode(mut self, on: bool) -> Self {
        self.plan_mode = on;
        self
    }

    /// Задать цель сессии. `None` или пустая (после trim) строка — секция пропускается.
    pub fn goal(mut self, goal: Option<String>) -> Self {
        self.goal = goal;
        self
    }

    /// Собрать финальный системный промпт.
    ///
    /// Секции идут в фиксированном порядке и склеиваются одной пустой строкой;
    /// завершающего перевода строки у результата нет. Метод не потребляет
    /// сборщик — `build()` можно звать повторно.
    pub fn build(&self) -> String {
        let mut sections: Vec<String> = Vec::new();
        if let Some(s) = render_base(&self.base) {
            sections.push(s);
        }
        if let Some(s) = self.env.as_ref().and_then(EnvContext::render) {
            sections.push(s);
        }
        if let Some(s) = render_agents_md(&self.agents_global, &self.agents_workspace, self.agents_md_limit) {
            sections.push(s);
        }
        if let Some(s) = render_skills(&self.skills) {
            sections.push(s);
        }
        if self.plan_mode {
            sections.push(PLAN_MODE_SECTION.to_string());
        }
        if let Some(s) = render_goal(&self.goal) {
            sections.push(s);
        }
        sections.join("\n\n")
    }
}

/// Базовая секция: заголовок уже внутри текста/константы, здесь только trim-проверка.
fn render_base(base: &str) -> Option<String> {
    let text = base.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// Секция AGENTS.md: склейка слоёв с подзаголовками, каждый слой — по лимиту символов.
fn render_agents_md(global: &str, workspace: &str, limit: usize) -> Option<String> {
    let mut layers: Vec<String> = Vec::new();
    if let Some(text) = cleaned_layer(global) {
        layers.push(format!(
            "### Global layer (~/.kimi-code/AGENTS.md)\n\n{}",
            truncate_layer(text, limit)
        ));
    }
    if let Some(text) = cleaned_layer(workspace) {
        layers.push(format!(
            "### Workspace layer (AGENTS.md)\n\n{}",
            truncate_layer(text, limit)
        ));
    }
    if layers.is_empty() {
        return None;
    }
    Some(format!(
        "## AGENTS.md\n\nProject instructions merged from layered AGENTS.md files \
         (general → specific; on conflicts the later layer wins).\n\n{}",
        layers.join("\n\n")
    ))
}

/// Trim слоя AGENTS.md; пустой после trim слой считается отсутствующим.
fn cleaned_layer(raw: &str) -> Option<&str> {
    let text = raw.trim();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Посимвольная обрезка слоя с явной пометкой (безопасна для UTF-8/кириллицы).
fn truncate_layer(text: &str, limit: usize) -> String {
    let total = text.chars().count();
    if total <= limit {
        return text.to_string();
    }
    let head: String = text.chars().take(limit).collect();
    format!(
        "{}\n\n[... truncated: the layer has {total} chars, exceeding the limit of {limit}; \
         the rest is omitted ...]",
        head.trim_end()
    )
}

/// Секция дайджеста скиллов: имя + усечённое однострочное описание,
/// не более [`MAX_SKILLS_IN_DIGEST`] строк; остаток — пометкой «and N more».
fn render_skills(skills: &[SkillDigest]) -> Option<String> {
    let valid_total = skills.iter().filter(|s| !s.name.trim().is_empty()).count();
    let mut lines: Vec<String> = Vec::new();
    for s in skills {
        let name = s.name.trim();
        if name.is_empty() {
            continue;
        }
        if lines.len() >= MAX_SKILLS_IN_DIGEST {
            break;
        }
        let flat = s.desc.split_whitespace().collect::<Vec<_>>().join(" ");
        let desc = truncate_inline(&flat, SKILL_DESC_MAX_CHARS);
        if desc.is_empty() {
            lines.push(format!("- {name}"));
        } else {
            lines.push(format!("- {name}: {desc}"));
        }
    }
    if lines.is_empty() {
        return None;
    }
    let mut out = String::from(
        "## Available skills\n\nCall the `skill` tool with a skill name to load its full instructions.\n\n",
    );
    out.push_str(&lines.join("\n"));
    let hidden = valid_total.saturating_sub(lines.len());
    if hidden > 0 {
        out.push_str(&format!("\n- ... and {hidden} more skill(s) not listed"));
    }
    Some(out)
}

/// Однострочная посимвольная обрезка с хвостом `...` (для описаний скиллов).
fn truncate_inline(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let head: String = text.chars().take(limit).collect();
    format!("{}...", head.trim_end())
}

/// Секция цели: идёт последней в промпте — максимальная заметность (recency).
fn render_goal(goal: &Option<String>) -> Option<String> {
    let text = goal.as_ref()?.trim();
    if text.is_empty() {
        return None;
    }
    Some(format!(
        "## Goal\n\nThe user set a goal for this session. Keep working until it is verifiably \
         complete; do not stop at partial results.\n\n{text}"
    ))
}

/// Ветка git без запуска процессов: читаем `.git/HEAD`, поднимаясь от `start`
/// вверх не более чем на [`GIT_SEARCH_DEPTH`] уровней.
/// `ref: refs/heads/X` → `X`; detached HEAD (hex-хэш) → первые 7 символов хэша.
fn git_branch_near(start: &Path) -> Option<String> {
    let mut dir = Some(start);
    for _ in 0..GIT_SEARCH_DEPTH {
        let current = dir?;
        let head = current.join(".git").join("HEAD");
        if head.is_file() {
            let text = std::fs::read_to_string(head).ok()?;
            let text = text.trim();
            if let Some(branch) = text.strip_prefix("ref: refs/heads/") {
                if !branch.is_empty() {
                    return Some(branch.to_string());
                }
                return None;
            }
            let bytes = text.as_bytes();
            let is_hash = (7..=64).contains(&bytes.len()) && bytes.iter().all(u8::is_ascii_hexdigit);
            return if is_hash {
                Some(text.chars().take(7).collect())
            } else {
                None
            };
        }
        dir = current.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Уникальный временный каталог для теста (тесты бегут параллельно).
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("theseus_prompts_test_{}_{}", std::process::id(), tag));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Короткий конструктор дайджеста для тестов.
    fn skill(name: &str, desc: &str) -> SkillDigest {
        SkillDigest::new(name, desc)
    }

    #[test]
    fn default_builder_contains_only_base() {
        let out = PromptBuilder::new().build();
        assert!(out.starts_with("## Base instructions"));
        assert!(!out.contains("## Environment"));
        assert!(!out.contains("## AGENTS.md"));
        assert!(!out.contains("## Available skills"));
        assert!(!out.contains("## Plan mode"));
        assert!(!out.contains("## Goal"));
    }

    #[test]
    fn empty_builder_produces_empty_string() {
        assert_eq!(PromptBuilder::empty().build(), "");
    }

    #[test]
    fn section_order_is_fixed() {
        let out = PromptBuilder::new()
            .env(EnvContext { os: "linux".into(), ..Default::default() })
            .agents_md("global rules", "workspace rules")
            .skills(&[skill("demo", "desc")])
            .plan_mode(true)
            .goal(Some("ship it".to_string()))
            .build();
        let order = [
            out.find("## Base instructions").unwrap(),
            out.find("## Environment").unwrap(),
            out.find("## AGENTS.md").unwrap(),
            out.find("## Available skills").unwrap(),
            out.find("## Plan mode").unwrap(),
            out.find("## Goal").unwrap(),
        ];
        assert!(order.windows(2).all(|w| w[0] < w[1]), "порядок секций нарушен: {order:?}");
    }

    #[test]
    fn empty_sections_are_skipped() {
        let out = PromptBuilder::empty()
            .base("   ")
            .env(EnvContext::default())
            .agents_md("  ", "\n")
            .skills(&[])
            .plan_mode(false)
            .goal(Some("   ".to_string()))
            .build();
        assert_eq!(out, "");
    }

    #[test]
    fn base_override_replaces_default() {
        let out = PromptBuilder::new().base("CUSTOM BASE").build();
        assert_eq!(out, "CUSTOM BASE");
        assert!(!out.contains("Theseus"));
    }

    #[test]
    fn sections_separated_by_blank_line() {
        let out = PromptBuilder::empty().base("AAA").goal(Some("BBB".to_string())).build();
        assert!(out.contains("AAA\n\n## Goal"), "секции склеены пустой строкой: {out:?}");
    }

    #[test]
    fn env_renders_only_filled_fields() {
        let env = EnvContext {
            os: "linux".into(),
            shell: String::new(),
            cwd: "/tmp/x".into(),
            date: "2026-07-18".into(),
            git_branch: None,
        };
        let section = env.render().unwrap();
        assert!(section.contains("- OS: linux"));
        assert!(section.contains("- CWD: /tmp/x"));
        assert!(!section.contains("Shell"));
        assert!(!section.contains("Git branch"));
    }

    #[test]
    fn env_all_empty_yields_none() {
        assert!(EnvContext::default().render().is_none());
        let env = EnvContext { os: "  ".into(), ..Default::default() };
        assert!(env.render().is_none(), "whitespace-поля — это пустая секция");
    }

    #[test]
    fn env_git_branch_line() {
        let env = EnvContext { git_branch: Some("main".into()), ..Default::default() };
        let section = env.render().unwrap();
        assert!(section.contains("- Git branch: main"));
        // пустая ветка — как отсутствующая
        let env = EnvContext { git_branch: Some("  ".into()), ..Default::default() };
        assert!(env.render().is_none());
    }

    #[test]
    fn env_detect_smoke() {
        let env = EnvContext::detect("2026-07-18");
        assert!(!env.os.is_empty());
        assert!(!env.cwd.is_empty());
        assert_eq!(env.date, "2026-07-18");
        assert!(env.render().is_some(), "секция рендерится даже без ветки git");
    }

    #[test]
    fn agents_md_layers_glued_with_subheaders() {
        let out = PromptBuilder::empty().agents_md("GLOBAL BODY", "WORKSPACE BODY").build();
        let g = out.find("### Global layer (~/.kimi-code/AGENTS.md)").unwrap();
        let w = out.find("### Workspace layer (AGENTS.md)").unwrap();
        assert!(g < w, "глобальный слой должен идти раньше workspace");
        assert!(out.contains("GLOBAL BODY"));
        assert!(out.contains("WORKSPACE BODY"));
        assert!(out.starts_with("## AGENTS.md"));
    }

    #[test]
    fn agents_md_single_layer_only() {
        let out = PromptBuilder::empty().agents_md("", "ONLY WS").build();
        assert!(!out.contains("Global layer"));
        assert!(out.contains("### Workspace layer"));
        assert!(out.contains("ONLY WS"));
    }

    #[test]
    fn agents_md_all_blank_no_section() {
        let out = PromptBuilder::new().agents_md("  ", "\n\t").build();
        assert!(!out.contains("AGENTS.md"));
    }

    #[test]
    fn agents_md_truncation_with_note() {
        let long = "a".repeat(100);
        let out = PromptBuilder::empty().agents_md_limit(10).agents_md(long, "").build();
        assert!(out.contains("truncated"), "должна быть пометка об обрезке: {out}");
        assert!(out.contains("100"), "в пометке указан полный размер: {out}");
        assert!(out.contains("aaaaaaaaaa"), "голова — ровно 10 символов");
        assert!(!out.contains("aaaaaaaaaaa"), "11-й символ уже обрезан");
    }

    #[test]
    fn agents_md_truncation_is_char_safe_for_cyrillic() {
        // 120 символов кириллицы (2 байта/символ в UTF-8): побайтовая обрезка сломала бы границу.
        let text = "Привет".repeat(20);
        let out = PromptBuilder::empty().agents_md_limit(13).agents_md("", text).build();
        assert!(out.contains("ПриветПриветП"), "13 символов = 2 слова + 'П': {out}");
        assert!(out.contains("truncated"));
    }

    #[test]
    fn skills_digest_rendering() {
        let skills = [skill("demo-a", "первая"), skill("demo-b", "вторая")];
        let out = PromptBuilder::empty().skills(&skills).build();
        assert!(out.starts_with("## Available skills"));
        assert!(out.contains("- demo-a: первая"));
        assert!(out.contains("- demo-b: вторая"));
    }

    #[test]
    fn skills_desc_truncated_and_single_line() {
        let long_desc = "x".repeat(200) + "\nnewline-word";
        let out = PromptBuilder::empty().skills(&[skill("s", &long_desc)]).build();
        let expect_head = format!("{}...", "x".repeat(SKILL_DESC_MAX_CHARS));
        assert!(out.contains(&expect_head), "описание усечено до 80 + ...: {out}");
        assert!(!out.contains("newline-word"), "перевод строки свёрнут, хвост отрезан");
    }

    #[test]
    fn skills_cap_with_more_note() {
        let total = MAX_SKILLS_IN_DIGEST + 10;
        let skills: Vec<SkillDigest> = (0..total).map(|i| skill(&format!("sk-{i:02}"), "d")).collect();
        let out = PromptBuilder::empty().skills(&skills).build();
        let last_shown = format!("sk-{:02}", MAX_SKILLS_IN_DIGEST - 1);
        let first_hidden = format!("sk-{MAX_SKILLS_IN_DIGEST:02}");
        assert!(out.contains("- sk-00: d"));
        assert!(out.contains(&format!("- {last_shown}: d")));
        assert!(!out.contains(&first_hidden));
        assert!(out.contains("10 more skill(s)"), "пометка о скрытых: {out}");
    }

    #[test]
    fn skills_empty_name_entries_skipped() {
        let out = PromptBuilder::empty()
            .skills(&[skill("", "no name"), skill("  ", "blank"), skill("ok", "yes")])
            .build();
        assert!(out.contains("- ok: yes"));
        assert!(!out.contains("no name"));
        // секция исчезает целиком, если валидных имён нет
        let none = PromptBuilder::empty().skills(&[skill("", "x")]).build();
        assert_eq!(none, "");
    }

    #[test]
    fn plan_mode_toggle() {
        let on = PromptBuilder::empty().plan_mode(true).build();
        assert!(on.contains("## Plan mode"));
        assert!(on.contains("read-only"));
        let off = PromptBuilder::empty().plan_mode(false).build();
        assert!(!off.contains("Plan mode"));
    }

    #[test]
    fn goal_rendered_last_and_trimmed() {
        let out = PromptBuilder::new()
            .plan_mode(true)
            .goal(Some("  победить  ".to_string()))
            .build();
        let plan_pos = out.find("## Plan mode").unwrap();
        let goal_pos = out.find("## Goal").unwrap();
        assert!(plan_pos < goal_pos, "goal — последняя секция");
        assert!(out.trim_end().ends_with("победить"), "цель триммится: {out:?}");
    }

    #[test]
    fn goal_empty_or_none_skipped() {
        assert!(!PromptBuilder::empty().goal(None).build().contains("Goal"));
        assert!(!PromptBuilder::empty().goal(Some(String::new())).build().contains("Goal"));
        assert!(!PromptBuilder::empty().goal(Some("   ".into())).build().contains("## Goal"));
    }

    #[test]
    fn git_branch_from_head_ref() {
        let dir = temp_dir("gitref");
        let git = dir.join(".git");
        fs::create_dir_all(&git).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/feature-x\n").unwrap();
        assert_eq!(git_branch_near(&dir), Some("feature-x".to_string()));
        // подъём наверх из вложенного подкаталога
        let sub = dir.join("a/b");
        fs::create_dir_all(&sub).unwrap();
        assert_eq!(git_branch_near(&sub), Some("feature-x".to_string()));
    }

    #[test]
    fn git_branch_detached_head_short_hash() {
        let dir = temp_dir("gitdetached");
        let git = dir.join(".git");
        fs::create_dir_all(&git).unwrap();
        fs::write(git.join("HEAD"), "0123456789abcdef0123456789abcdef01234567").unwrap();
        assert_eq!(git_branch_near(&dir), Some("0123456".to_string()));
    }

    #[test]
    fn git_branch_absent_returns_none() {
        let dir = temp_dir("gitnone");
        assert_eq!(git_branch_near(&dir), None);
    }

    #[test]
    fn builder_reusable_and_deterministic() {
        let b = PromptBuilder::new().goal(Some("g".to_string()));
        assert_eq!(b.build(), b.build());
    }
}
