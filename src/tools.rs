//! Инструменты агента: схемы + исполнение. Уроки: read-before-edit НЕ требуем (Grok),
//! но edit — exact-match (Claude); выводы ограничены 20 КБ; .git защищён.

use anyhow::{anyhow, Result};
use crate::matchers::{MatchError, MatchKind};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const OUTPUT_LIMIT: usize = 20 * 1024;

pub fn tool_specs() -> serde_json::Value {
    json!([
        {"type":"function","function":{
            "name":"read_file",
            "description":"Read a text file. Returns numbered lines (like cat -n). Use offset/limit to page large files.",
            "parameters":{"type":"object","properties":{
                "path":{"type":"string"},
                "offset":{"type":"integer","description":"1-based start line (default 1)"},
                "limit":{"type":"integer","description":"max lines (default 400)"}
            },"required":["path"]}}},
        {"type":"function","function":{
            "name":"write_file",
            "description":"Create or overwrite a file inside the workspace. Parent dirs are created.",
            "parameters":{"type":"object","properties":{
                "path":{"type":"string"},
                "content":{"type":"string"}
            },"required":["path","content"]}}},
        {"type":"function","function":{
            "name":"edit_file",
            "description":"Replace an exact string in a file. old_string must match exactly once (or use replace_all).",
            "parameters":{"type":"object","properties":{
                "path":{"type":"string"},
                "old_string":{"type":"string"},
                "new_string":{"type":"string"},
                "replace_all":{"type":"boolean"}
            },"required":["path","old_string","new_string"]}}},
        {"type":"function","function":{
            "name":"apply_patch",
            "description":"Apply a patch in Codex format: '*** Begin Patch', then ops '*** Add File' (lines with '+'), '*** Update File' (optional '@@ anchor', then ' '/'-'/'+' lines), '*** Delete File', then '*** End Patch'. Paths are relative to the workspace. Returns touched files with a diff preview.",
            "parameters":{"type":"object","properties":{
                "patch":{"type":"string"}
            },"required":["patch"]}}},
        {"type":"function","function":{
            "name":"list_files",
            "description":"List files in the workspace (glob-like, newest not guaranteed). Skips .git and target/.",
            "parameters":{"type":"object","properties":{
                "path":{"type":"string","description":"subdir (default .)"},
                "max_results":{"type":"integer","description":"default 100"}
            }}}},
        {"type":"function","function":{
            "name":"grep",
            "description":"Search file contents with a plain substring or simple pattern. Returns path:line: text (max 50).",
            "parameters":{"type":"object","properties":{
                "pattern":{"type":"string"},
                "path":{"type":"string","description":"subdir (default .)"}
            },"required":["pattern"]}}},
        {"type":"function","function":{
            "name":"concept_search",
            "description":"Search ML concepts library (/home/roman/library): RL/LLM/agent concept cards with definitions and relations. Returns ranked slug list.",
            "parameters":{"type":"object","properties":{
                "query":{"type":"string"},
                "limit":{"type":"integer","description":"default 5"}
            },"required":["query"]}}},
        {"type":"function","function":{
            "name":"concept_explain",
            "description":"Explain one ML concept card by slug (definition, motivation, related concepts). Use concept_search first to find the slug.",
            "parameters":{"type":"object","properties":{
                "slug":{"type":"string"}
            },"required":["slug"]}}},
        {"type":"function","function":{
            "name":"library_search",
            "description":"Search the ML papers library (recipes_taxonomy, ~14k arXiv PDFs + docs on LLM/RL/agents). Returns paths with snippets; follow up with library_read.",
            "parameters":{"type":"object","properties":{
                "query":{"type":"string"},
                "limit":{"type":"integer","description":"default 8"}
            },"required":["query"]}}},
        {"type":"function","function":{
            "name":"library_read",
            "description":"Read an excerpt from a library document found via library_search (path relative to the library root).",
            "parameters":{"type":"object","properties":{
                "path":{"type":"string"},
                "max_chars":{"type":"integer","description":"default 4000"}
            },"required":["path"]}}},
        {"type":"function","function":{
            "name":"ariadna_ask",
            "description":"Ask Ariadna — a fast local ML helper model (Qwen3.5-4B GRPO, runs on local GPU via llama.cpp). Good for drafts, classification, extraction, simple Q&A. Starts the local server on first use (~30s).",
            "parameters":{"type":"object","properties":{
                "task":{"type":"string"}
            },"required":["task"]}}},
        {"type":"function","function":{
            "name":"digest_search",
            "description":"Search daily news digests (AINews/Raschka/SimonWillison + HuggingFace digests) by keyword. Returns dated hits with snippets; read one with digest_read.",
            "parameters":{"type":"object","properties":{
                "query":{"type":"string"},
                "days":{"type":"integer","description":"only last N days (optional)"},
                "limit":{"type":"integer","description":"default 6"}
            },"required":["query"]}}},
        {"type":"function","function":{
            "name":"digest_read",
            "description":"Read a digest file found via digest_search (absolute path).",
            "parameters":{"type":"object","properties":{
                "path":{"type":"string"},
                "max_chars":{"type":"integer","description":"default 6000"}
            },"required":["path"]}}},
        {"type":"function","function":{
            "name":"hf_collections",
            "description":"Search HuggingFace collections database (300+ collections from 24 providers: DeepSeek, Qwen, NVIDIA, Mistral...). Filter by provider, rank by relevance/upvotes.",
            "parameters":{"type":"object","properties":{
                "query":{"type":"string"},
                "provider":{"type":"string","description":"e.g. deepseek-ai, qwen (optional)"},
                "limit":{"type":"integer","description":"default 8"}
            },"required":["query"]}}},
        {"type":"function","function":{
            "name":"peer_ask",
            "description":"Ask an external CLI agent installed on this machine (claude=Claude Code, kimi=Kimi Code, codewhale, hermes=Hermes Agent, openclaw). Runs the agent headless with your task and returns its answer. Powerful: requires ask/yolo mode.",
            "parameters":{"type":"object","properties":{
                "agent":{"type":"string","description":"claude | kimi | codewhale | hermes | openclaw"},
                "task":{"type":"string"},
                "timeout_secs":{"type":"integer","description":"optional, per-agent default"}
            },"required":["agent","task"]}}},
        {"type":"function","function":{
            "name":"bash",
            "description":"Run a shell command in the workspace. Subject to permission rules. Timeout default 120s. Output capped at 20KB. Set is_background=true for long-running commands (returns task id; use task_output/task_stop).",
            "parameters":{"type":"object","properties":{
                "command":{"type":"string"},
                "timeout_secs":{"type":"integer","description":"default 120, max 600"},
                "is_background":{"type":"boolean","description":"run detached, returns bg id"}
            },"required":["command"]}}},
        {"type":"function","function":{
            "name":"task_output",
            "description":"Read current output/status of a background task by id.",
            "parameters":{"type":"object","properties":{
                "id":{"type":"integer"}
            },"required":["id"]}}},
        {"type":"function","function":{
            "name":"task_stop",
            "description":"Stop a background task by id.",
            "parameters":{"type":"object","properties":{
                "id":{"type":"integer"}
            },"required":["id"]}}},
        {"type":"function","function":{
            "name":"skill",
            "description":"Load a skill's instructions by name (discovered SKILL.md packages). Call once per needed skill.",
            "parameters":{"type":"object","properties":{
                "name":{"type":"string"}
            },"required":["name"]}}},
        {"type":"function","function":{
            "name":"skill_search",
            "description":"Search the skills library by keyword (name/description). Returns ranked skill names with short descriptions; load the chosen one with the skill tool.",
            "parameters":{"type":"object","properties":{
                "query":{"type":"string"},
                "limit":{"type":"integer","description":"default 8"}
            },"required":["query"]}}},
        {"type":"function","function":{
            "name":"memory_write",
            "description":"Store an important fact about the user/project into long-term memory (MEMORY.md). Use sparingly for durable facts.",
            "parameters":{"type":"object","properties":{
                "fact":{"type":"string"}
            },"required":["fact"]}}},
        {"type":"function","function":{
            "name":"memory_search",
            "description":"Search long-term memory for relevant facts.",
            "parameters":{"type":"object","properties":{
                "query":{"type":"string"}
            },"required":["query"]}}},
        {"type":"function","function":{
            "name":"web_fetch",
            "description":"Fetch a web page and return its text (HTML stripped). Only allowed domains per config.",
            "parameters":{"type":"object","properties":{
                "url":{"type":"string"}
            },"required":["url"]}}},
        {"type":"function","function":{
            "name":"web_search",
            "description":"Search the web (DuckDuckGo + Wikipedia). Returns short results with URLs. Enabled only when web_allowed_domains is non-empty.",
            "parameters":{"type":"object","properties":{
                "query":{"type":"string"}
            },"required":["query"]}}},
        {"type":"function","function":{
            "name":"exit_plan_mode",
            "description":"Call when the plan is presented and ready for approval. Only meaningful in plan mode.",
            "parameters":{"type":"object","properties":{
                "plan_summary":{"type":"string"}
            },"required":["plan_summary"]}}},
        {"type":"function","function":{
            "name":"todo_write",
            "description":"Update the shared todo list (planning discipline). Exactly one item may be in_progress.",
            "parameters":{"type":"object","properties":{
                "todos":{"type":"array","items":{"type":"object","properties":{
                    "content":{"type":"string"},
                    "status":{"type":"string","enum":["pending","in_progress","done"]}
                },"required":["content","status"]}}
            },"required":["todos"]}}},
        {"type":"function","function":{
            "name":"task",
            "description":"Delegate a subtask to a subagent (isolated context, own budget). Agents: explore — codebase Q&A with file:line refs (readonly); plan — architecture implementation plan (readonly); code_review — diff/file review with findings by severity (readonly); test_runner — run build/tests/lint and report faithfully (bash, no source edits). Use to keep your own context clean.",
            "parameters":{"type":"object","properties":{
                "agent":{"type":"string","description":"explore | plan | code_review | test_runner (default: explore)"},
                "prompt":{"type":"string"}
            },"required":["prompt"]}}},
        {"type":"function","function":{
            "name":"finish",
            "description":"Call ONCE when the task is fully done. Provide an honest summary of what was achieved.",
            "parameters":{"type":"object","properties":{
                "summary":{"type":"string"}
            },"required":["summary"]}}}
    ])
}

#[derive(Debug, Clone)]
pub struct TodoItem {
    pub content: String,
    pub status: String,
}

pub struct ToolEnv {
    pub workspace: PathBuf,
    pub todos: Vec<TodoItem>,
    /// Внутреннее хранилище todo-списка (crate::todo): атомарная замена, метки
    /// времени, события. Внешний контракт для гейта в agent — строковый снимок
    /// `todos`; гейт-логика из tools не переезжает.
    todo_list: crate::todo::TodoList,
    pub finished: Option<String>,
    /// kernel sandbox (landlock) для bash (v0.3.1)
    pub sandbox: bool,
    /// Индекс концептов ML-библиотеки (ленивая одноразовая загрузка, v0.5)
    concepts: std::sync::OnceLock<crate::ml_concepts::ConceptIndex>,
    /// Индекс recipes_taxonomy (ленивый; None — корень недоступен)
    library: std::sync::OnceLock<Option<crate::library::LibraryIndex>>,
}

impl ToolEnv {
    pub fn new(workspace: &Path) -> Self {
        ToolEnv {
            workspace: workspace.to_path_buf(),
            todos: vec![],
            todo_list: crate::todo::TodoList::new(),
            finished: None,
            sandbox: false,
            concepts: std::sync::OnceLock::new(),
            library: std::sync::OnceLock::new(),
        }
    }

    /// Ленивый индекс концептов (/home/roman/library/concepts)
    fn concept_index(&self) -> &crate::ml_concepts::ConceptIndex {
        self.concepts.get_or_init(|| {
            crate::ml_concepts::ConceptIndex::build(Path::new("/home/roman/library/concepts"))
        })
    }

    /// Ленивый индекс ML-библиотеки (recipes_taxonomy); None — корень недоступен
    fn library_index(&self) -> Option<&crate::library::LibraryIndex> {
        self.library
            .get_or_init(|| crate::library::LibraryIndex::load(
                Path::new(crate::library::DEFAULT_ROOT)).ok())
            .as_ref()
    }

    fn resolve(&self, path: &str) -> PathBuf {
        let p = Path::new(path);
        if p.is_absolute() { p.to_path_buf() } else { self.workspace.join(p) }
    }

    pub fn call(&mut self, name: &str, args: &serde_json::Value) -> String {
        match self.dispatch(name, args) {
            Ok(s) => {
                // маркер пустого результата (урок Claude Code: пустой tool_result
                // провоцирует модели завершать ход молча)
                if s.trim().is_empty() {
                    format!("({name} completed with no output)")
                } else {
                    s
                }
            }
            Err(e) => format!("ERROR: {e}"),
        }
    }

    fn dispatch(&mut self, name: &str, args: &serde_json::Value) -> Result<String> {
        match name {
            "read_file" => self.read_file(args),
            "write_file" => self.write_file(args),
            "edit_file" => self.edit_file(args),
            "apply_patch" => self.apply_patch(args),
            "list_files" => self.list_files(args),
            "grep" => self.grep(args),
            "bash" => self.bash(args),
            "todo_write" => self.todo_write(args),
            // --- ML-инструменты (v0.5): концепты, библиотека, субагент Ариадна ---
            "concept_search" => {
                let q = args["query"].as_str().unwrap_or("").to_string();
                let limit = args["limit"].as_u64().unwrap_or(5) as usize;
                let idx = self.concept_index();
                let hits = idx.search(&q, limit);
                if hits.is_empty() {
                    return Ok(format!("концептов по запросу «{q}» не найдено (индекс: {} шт.)",
                        idx.stats().0));
                }
                let mut out = format!("концептов: {} (показано {})\n", idx.stats().0, hits.len());
                for c in hits {
                    out.push_str(&format!("- {} [{}|{}] {} — семья «{}»\n",
                        c.slug, c.card_type, c.level, c.title, c.family));
                }
                Ok(out)
            }
            "concept_explain" => {
                let slug = args["slug"].as_str().unwrap_or("");
                match self.concept_index().explain(slug) {
                    Some(text) => Ok(text),
                    None => {
                        let near = self.concept_index().search(slug, 5);
                        Ok(concept_not_found_message(slug, &near))
                    }
                }
            }
            "library_search" => {
                let Some(idx) = self.library_index() else {
                    return Ok("библиотека недоступна: корень recipes_taxonomy не найден".into());
                };
                let q = args["query"].as_str().unwrap_or("").to_string();
                let limit = args["limit"].as_u64().unwrap_or(8) as usize;
                let hits = idx.search_docs(&q, limit);
                if hits.is_empty() {
                    return Ok(format!("по запросу «{q}» в библиотеке ничего не найдено"));
                }
                let mut out = String::new();
                for h in hits {
                    out.push_str(&format!("- {} (score {})\n  {}\n", h.path.display(), h.score, h.snippet));
                }
                Ok(out)
            }
            "library_read" => {
                let Some(idx) = self.library_index() else {
                    return Ok("библиотека недоступна: корень recipes_taxonomy не найден".into());
                };
                let path = args["path"].as_str().unwrap_or("");
                let max = args["max_chars"].as_u64().unwrap_or(4000) as usize;
                idx.read_excerpt(Path::new(path), max)
            }
            "ariadna_ask" => {
                let task = args["task"].as_str().unwrap_or("");
                let cfg = crate::ariadna::AriadnaConfig::default();
                if !crate::ariadna::is_available(&cfg) {
                    return Ok("Ариадна недоступна: нет бинаря llama-server или GGUF \
                        (проверьте пути в AriadnaConfig)".into());
                }
                crate::ariadna::run_task(&cfg,
                    "Ты — Ариадна, локальный быстрый ML-помощник (Qwen3.5-4B). \
                     Отвечай кратко и по делу, на русском, сразу текстом ответа, \
                     без блоков <think>.",
                    task)
            }
            // --- Дайджесты новостей и HF-коллекции (v0.5.1) ---
            "digest_search" => {
                let q = args["query"].as_str().unwrap_or("").to_string();
                let limit = args["limit"].as_u64().unwrap_or(6) as usize;
                let days = args["days"].as_u64().map(|d| d as u32);
                // новостные дайджесты + HF-дайджесты: один формат YYYY-MM-DD_*.md
                let mut entries = crate::digests::scan_digests(
                    Path::new(crate::digests::NEWS_ROOT), "новости");
                entries.extend(crate::digests::scan_digests(
                    Path::new(crate::digests::HF_ROOT), "HF"));
                entries.sort_by(|a, b| b.date.cmp(&a.date));
                let hits = crate::digests::search_digests(&entries, &q, days, limit);
                if hits.is_empty() {
                    return Ok(format!("по запросу «{q}» в дайджестах ничего не найдено"));
                }
                let mut out = String::new();
                for h in hits {
                    out.push_str(&format!("- [{}|{}] {} — {}\n  {}\n",
                        h.entry.date, h.entry.source, h.entry.title,
                        h.entry.path.display(), h.snippet));
                }
                Ok(out)
            }
            "digest_read" => {
                let path = args["path"].as_str().unwrap_or("");
                let max = args["max_chars"].as_u64().unwrap_or(6000) as usize;
                crate::digests::read_digest(Path::new(path), max)
                    .map_err(|e| anyhow!("{e}"))
            }
            "hf_collections" => {
                let q = args["query"].as_str().unwrap_or("").to_string();
                let limit = args["limit"].as_u64().unwrap_or(8) as usize;
                let provider = args["provider"].as_str();
                let cols = crate::digests::load_collections(Path::new(crate::digests::HF_ROOT)
                    .join("collections_data.json").as_path())?;
                let hits = crate::digests::search_collections(&cols, &q, provider, limit);
                if hits.is_empty() {
                    return Ok(format!("коллекций по «{q}» не найдено (всего: {})", cols.len()));
                }
                let mut out = format!("коллекций: {} (показано {})\n", cols.len(), hits.len());
                for c in hits {
                    out.push_str(&format!(
                        "- {} [{}|▲{}|{} шт.] {}\n  тема: {} | {}\n",
                        c.slug, c.provider_key, c.upvotes, c.item_count, c.title,
                        c.theme, c.url));
                }
                Ok(out)
            }
            "finish" => {
                let s = args["summary"].as_str().unwrap_or("").to_string();
                self.finished = Some(s.clone());
                Ok(s)
            }
            other => {
                // полезная ошибка вместо голой «unknown tool» (урок тройки):
                // список реальных инструментов + подсказка про скилл — иначе
                // модель флаила несколько ходов (живой кейс 21.07: модель
                // вызвала «agent-sessions» — это скилл, а не инструмент)
                let specs = tool_specs();
                let names: Vec<&str> = specs.as_array()
                    .map(|a| a.iter().filter_map(|t| t["function"]["name"].as_str()).collect())
                    .unwrap_or_default();
                Err(anyhow!(
                    "unknown tool {other}. Доступные инструменты: {}. \
                     Если «{other}» — это скилл, загрузите его инструментом skill \
                     (поиск по имени — skill_search).",
                    names.join(", ")
                ))
            }
        }
    }

    fn read_file(&self, args: &serde_json::Value) -> Result<String> {
        let path = self.resolve(args["path"].as_str().unwrap_or(""));
        let text = std::fs::read_to_string(&path)
            .map_err(|e| anyhow!("{e}: {}", path.display()))?;
        let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
        let limit = args["limit"].as_u64().unwrap_or(400) as usize;
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();
        let slice: Vec<String> = lines.iter().skip(offset - 1).take(limit)
            .enumerate().map(|(i, l)| format!("{:>6}\t{}", offset + i, l)).collect();
        if slice.is_empty() && total == 0 { return Ok("(пустой файл)".into()); }
        let mut out = slice.join("\n");
        if offset - 1 + limit < total {
            out += &format!("\n... (ещё {} строк; offset {})", total - (offset - 1 + limit), offset + limit);
        }
        Ok(cap(out))
    }

    fn write_file(&self, args: &serde_json::Value) -> Result<String> {
        let path = self.resolve(args["path"].as_str().unwrap_or(""));
        let content = args["content"].as_str().unwrap_or("");
        if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
        std::fs::write(&path, content)?;
        Ok(format!("OK: записано {} байт в {}", content.len(), path.display()))
    }

    fn edit_file(&self, args: &serde_json::Value) -> Result<String> {
        let rel = args["path"].as_str().unwrap_or("");
        let path = self.resolve(rel);
        let old = args["old_string"].as_str().unwrap_or("\u{0}");
        let new = args["new_string"].as_str().unwrap_or("");
        let replace_all = args["replace_all"].as_bool().unwrap_or(false);
        let text = std::fs::read_to_string(&path).map_err(|e| anyhow!("{e}: {}", path.display()))?;
        // Уровень 0 (классика): exact-substring — сохраняет семантику v0.1 для подстрок в строке
        let sub_count = text.matches(old).count();
        if sub_count > 0 {
            if sub_count > 1 && !replace_all {
                // BUG-QA-EDIT-01: exact-путь тоже сообщает строки вхождений
                // (1-based, без дублей), как fuzzy-вентиль multi-occurrence.
                let mut lines: Vec<usize> = text
                    .match_indices(old)
                    .map(|(pos, _)| text[..pos].matches('\n').count() + 1)
                    .collect();
                lines.sort_unstable();
                lines.dedup();
                let list = lines.iter().map(usize::to_string).collect::<Vec<_>>().join(", ");
                return Err(anyhow!(
                    "old_string встречается {sub_count} раз (строки: {list}); \
                     уточните контекст или replace_all=true"
                ));
            }
            let out = if replace_all { text.replace(old, new) } else { text.replacen(old, new, 1) };
            let preview = diff_preview(&text, &out, rel);
            std::fs::write(&path, &out)?;
            return Ok(format!("OK: заменено {} вхождение(й) в {} (exact){preview}",
                if replace_all { sub_count } else { 1 }, path.display()));
        }

        // Fuzzy-каскад crate::matchers (урок OpenDev 9-pass): 8 матчеров от строгого
        // к мягкому + вентиль multi-occurrence на каждом уровне.
        if replace_all {
            // replace_all вне exact-пути: заменяем все блоки, совпадающие построчно
            // по trim (семантика v0.1); если таких нет — пробуем единственный fuzzy-матч.
            let old_lines: Vec<&str> = old.trim_matches('\n').split('\n').collect();
            // BUG-QA-EDIT-02: строки замены приводим к доминирующему окончанию
            // строк файла — '\r' на конце каждой строки блока восстановит CRLF
            // при join('\n') в replace_all_trim_blocks.
            let crlf = dominant_newline(&text) == "\r\n";
            let new_lines: Vec<String> = new
                .trim_matches('\n')
                .split('\n')
                .map(|l| {
                    // нормализация: свой '\r' (CRLF-вход от модели) снимаем,
                    // затем восстанавливаем по доминирующему окончанию файла
                    let l = l.strip_suffix('\r').unwrap_or(l);
                    if crlf { format!("{l}\r") } else { l.to_string() }
                })
                .collect();
            let (out, count) = replace_all_trim_blocks(&text, &old_lines, &new_lines);
            if count > 0 {
                let preview = diff_preview(&text, &out, rel);
                std::fs::write(&path, &out)?;
                return Ok(format!("OK: заменено {count} вхождение(й) в {} (fuzzy: trim){preview}",
                    path.display()));
            }
        }
        match crate::matchers::find_match(&text, old) {
            Ok(m) => {
                // Диапазон совпадения — байтовый, на границах UTF-8; хвостовой \n
                // файла не входит в диапазон и потому сохраняется сам собой.
                // BUG-QA-EDIT-02: замена приводится к доминирующему окончанию
                // строк файла — LF-блок от модели не «размывает» CRLF-файл.
                let new = adapt_newlines(new, dominant_newline(&text));
                let mut out = String::with_capacity(text.len() - (m.end - m.start) + new.len());
                out.push_str(&text[..m.start]);
                out.push_str(&new);
                out.push_str(&text[m.end..]);
                let level_note = if m.kind == MatchKind::Exact {
                    String::new()
                } else {
                    format!(" (fuzzy: {})", m.kind.as_str())
                };
                let preview = diff_preview(&text, &out, rel);
                std::fs::write(&path, &out)?;
                Ok(format!("OK: заменено 1 вхождение(й) в {}{level_note}{preview}", path.display()))
            }
            Err(MatchError::Ambiguous { lines }) => {
                let list = lines.iter().map(usize::to_string).collect::<Vec<_>>().join(", ");
                Err(anyhow!(
                    "old_string встречается {} раз (строки: {list}); уточните контекст или replace_all=true",
                    lines.len()
                ))
            }
            Err(MatchError::NotFound { closest }) => {
                let hint = match closest {
                    Some(line) => format!("ближайшее похожее место — строка {line}"),
                    None => "совпадающих блоков нет".to_string(),
                };
                Err(anyhow!("old_string не найден в {} (9-уровневый fuzzy-каскад).\nПодсказка: {hint}",
                    path.display()))
            }
        }
    }

    /// Инструмент apply_patch: мультифайловые правки в формате Codex
    /// (`*** Begin Patch` … `*** End Patch`) через crate::patch.
    /// Ошибки парсинга/контекста/путей уходят модели текстом (ERROR: ...), без паник.
    fn apply_patch(&self, args: &serde_json::Value) -> Result<String> {
        let patch = args["patch"].as_str().unwrap_or("");
        // Снапшот «до» для diff-превью. Порядок записей совпадает с порядком операций
        // (и значит, с `touched` ниже). Ошибку разбора здесь не глотаем вслепую:
        // apply_patch повторит разбор и вернёт ту же ошибку модели.
        let before: Vec<(String, Option<String>)> = match crate::patch::parse_patch(patch) {
            Ok(ops) => ops
                .iter()
                .map(|op| {
                    let rel = op.path().display().to_string();
                    let old = std::fs::read_to_string(self.workspace.join(op.path())).ok();
                    (rel, old)
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        let touched = crate::patch::apply_patch(patch, &self.workspace)?;
        let mut out = format!("OK: apply_patch применён, затронуто файлов: {}", touched.len());
        for p in &touched {
            out += &format!("\n- {}", p.display());
        }
        // Превью фактических правок: unified diff «до → после» по каждому файлу
        // (удалённый файл читается как пустой, добавленный — из пустого).
        let mut diffs = String::new();
        for ((rel, old), abs) in before.iter().zip(touched.iter()) {
            let new = std::fs::read_to_string(abs).unwrap_or_default();
            diffs.push_str(&crate::diffview::unified_diff(old.as_deref().unwrap_or(""), &new, rel, 2));
        }
        out += &preview_block(&diffs);
        Ok(out)
    }

    fn list_files(&self, args: &serde_json::Value) -> Result<String> {
        let base = self.resolve(args["path"].as_str().unwrap_or("."));
        let max = args["max_results"].as_u64().unwrap_or(100) as usize;
        // явные ошибки вместо «(пусто)»: иначе модель принимала несуществующий
        // путь за пустой каталог и уходила в ложную разведку (живой кейс 21.07:
        // list_files library/index → «(пусто)» при отсутствующем каталоге)
        if !base.exists() {
            return Err(anyhow!("нет такого файла или каталога: {}", base.display()));
        }
        if !base.is_dir() {
            return Err(anyhow!("не каталог: {}", base.display()));
        }
        let mut out = vec![];
        walk(&base, &base, &mut out, max, 0)?;
        if out.is_empty() { return Ok("(пусто)".into()); }
        Ok(out.join("\n"))
    }

    fn grep(&self, args: &serde_json::Value) -> Result<String> {
        let pat = args["pattern"].as_str().unwrap_or("").to_string();
        let base = self.resolve(args["path"].as_str().unwrap_or("."));
        let mut out = vec![];
        grep_walk(&base, &base, &pat, &mut out, 50)?;
        if out.is_empty() { return Ok("(совпадений нет)".into()); }
        Ok(out.join("\n"))
    }

    fn bash(&self, args: &serde_json::Value) -> Result<String> {
        let cmd = args["command"].as_str().unwrap_or("");
        let timeout = Duration::from_secs(args["timeout_secs"].as_u64().unwrap_or(120).min(600));
        run_bash(cmd, &self.workspace, timeout, self.sandbox)
    }

    fn todo_write(&mut self, args: &serde_json::Value) -> Result<String> {
        let items = args["todos"].as_array().cloned().unwrap_or_default();
        let mut todos = vec![];
        for it in items {
            let content = it["content"].as_str().unwrap_or("").to_string();
            let status = it["status"].as_str().unwrap_or("pending").to_string();
            if !content.is_empty() {
                todos.push(TodoItem { content, status });
            }
        }
        let in_prog = todos.iter().filter(|t| t.status == "in_progress").count();
        if in_prog > 1 {
            return Err(anyhow!("должен быть ровно один in_progress (сейчас {in_prog})"));
        }
        // Внутреннее хранилище — crate::todo::TodoList (валидация id, метки времени,
        // события для TUI). id генерируем позиционно: внешний JSON-контракт id не имеет,
        // а наследование created_at между перезаписями списка работает по позиции.
        // Нераспознанный статус мягко сводится к Pending — раньше он хранился строкой
        // и гейтом трактовался как «не done», поведение не меняется.
        let stored: Vec<crate::todo::TodoItem> = todos
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let status = t
                    .status
                    .parse::<crate::todo::TodoStatus>()
                    .unwrap_or(crate::todo::TodoStatus::Pending);
                crate::todo::TodoItem::new(&format!("t{i}"), &t.content, status)
            })
            .collect();
        self.todo_list.set_full(stored).map_err(|e| anyhow!("todo: {e}"))?;
        self.todos = todos;
        Ok(format!("OK: {} задач в списке", self.todos.len()))
    }

    /// Полный сброс todo-списка (новая сессия /new, /clear): чистим и снимок,
    /// и внутреннее хранилище, чтобы гейт и TUI не увидели задачи прежней сессии.
    pub fn clear_todos(&mut self) {
        self.todos.clear();
        let _ = self.todo_list.set_full(Vec::new());
    }
}

/// Сообщение об отсутствии концепта (QA-ML-002): список похожих выводится,
/// только если он непуст — пустой список не должен давать оборванный
/// хвост «Похожие: ».
fn concept_not_found_message(slug: &str, near: &[&crate::ml_concepts::ConceptCard]) -> String {
    if near.is_empty() {
        format!("концепт «{slug}» не найден")
    } else {
        let list = near.iter().map(|c| c.slug.as_str()).collect::<Vec<_>>().join(", ");
        format!("концепт «{slug}» не найден. Похожие: {list}")
    }
}

fn walk(base: &Path, dir: &Path, out: &mut Vec<String>, max: usize, depth: usize) -> Result<()> {
    if out.len() >= max || depth > 8 { return Ok(()); }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for e in entries.flatten() {
        if out.len() >= max { break; }
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if name == ".git" || name == "target" || name == "node_modules" { continue; }
        let rel = p.strip_prefix(base).unwrap_or(&p).display().to_string();
        if p.is_dir() {
            walk(base, &p, out, max, depth + 1)?;
        } else {
            out.push(rel);
        }
    }
    Ok(())
}

// ---------- Fuzzy-матчинг блоков строк (урок OpenDev 9-pass) ----------
// Поисковый каскад живёт в crate::matchers (8 уровней + вентиль multi-occurrence);
// здесь остаётся только replace-all замена по trim-блокам и diff-превью правок.

/// Доминирующее окончание строк текста (BUG-QA-EDIT-02): "\r\n", если
/// CRLF-переводов больше, чем одиночных LF; при равенстве или отсутствии
/// переводов строк — "\n".
fn dominant_newline(text: &str) -> &'static str {
    let total = text.bytes().filter(|&b| b == b'\n').count();
    let crlf = text.matches("\r\n").count();
    if crlf > total - crlf { "\r\n" } else { "\n" }
}

/// Приводит переводы строк замены к доминирующему окончанию файла `nl`
/// (BUG-QA-EDIT-02): сначала нормализация к LF (на случай смешанных
/// окончаний в присланном моделью блоке), затем развёртывание до CRLF.
fn adapt_newlines(s: &str, nl: &str) -> String {
    if nl == "\n" { s.to_string() } else { s.replace("\r\n", "\n").replace('\n', "\r\n") }
}

/// Заменить все блоки `old_lines`, совпадающие построчно по trim, на `new_lines`.
/// Возвращает (новый текст, число замен); (текст без изменений, 0), если блока нет.
/// Семантика ветки replace_all из v0.1: хвостовой \n файла сохраняется.
/// `new_lines` — уже приведённые к доминирующему окончанию строк файла
/// (для CRLF-файла каждая строка несёт '\r' на конце, см. edit_file).
fn replace_all_trim_blocks(text: &str, old_lines: &[&str], new_lines: &[String]) -> (String, usize) {
    let trailing_nl = text.ends_with('\n');
    let file_lines: Vec<&str> = text.split('\n').collect();
    if old_lines.is_empty() || old_lines.len() > file_lines.len() {
        return (text.to_string(), 0);
    }
    let n = old_lines.len();
    let mut out_lines: Vec<String> = Vec::with_capacity(file_lines.len());
    let mut count = 0usize;
    let mut i = 0;
    while i < file_lines.len() {
        let block_matches = i + n <= file_lines.len()
            && file_lines[i..i + n].iter().zip(old_lines.iter()).all(|(a, b)| a.trim() == b.trim());
        if block_matches {
            out_lines.extend(new_lines.iter().cloned());
            i += n;
            count += 1;
        } else {
            out_lines.push(file_lines[i].to_string());
            i += 1;
        }
    }
    let mut out = out_lines.join("\n");
    if trailing_nl && !out.ends_with('\n') { out.push('\n'); }
    (out, count)
}

/// Обрезка unified-diff до 40 строк с пометкой; пустой diff превью не даёт.
fn preview_block(diff: &str) -> String {
    const MAX_LINES: usize = 40;
    let trimmed = diff.trim_end_matches('\n');
    if trimmed.is_empty() { return String::new(); }
    let lines: Vec<&str> = trimmed.lines().collect();
    let body = if lines.len() > MAX_LINES {
        format!("{}\n... (diff обрезан: показаны {MAX_LINES} из {} строк)",
            lines[..MAX_LINES].join("\n"), lines.len())
    } else {
        trimmed.to_string()
    };
    format!("\n\n--- preview (unified diff) ---\n{body}")
}

/// Unified-diff превью одиночной правки для модели (context=2, обрезка 40 строк).
fn diff_preview(old: &str, new: &str, path: &str) -> String {
    preview_block(&crate::diffview::unified_diff(old, new, path, 2))
}

fn grep_walk(base: &Path, dir: &Path, pat: &str, out: &mut Vec<String>, max: usize) -> Result<()> {
    if out.len() >= max { return Ok(()); }
    let entries = match std::fs::read_dir(dir) { Ok(e) => e, Err(_) => return Ok(()) };
    for e in entries.flatten() {
        if out.len() >= max { break; }
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        if name == ".git" || name == "target" || name == "node_modules" { continue; }
        if p.is_dir() {
            grep_walk(base, &p, pat, out, max)?;
        } else if let Ok(text) = std::fs::read_to_string(&p) {
            for (i, line) in text.lines().enumerate() {
                if line.contains(pat) {
                    let rel = p.strip_prefix(base).unwrap_or(&p).display().to_string();
                    out.push(format!("{rel}:{}: {}", i + 1, line.trim()));
                    if out.len() >= max { break; }
                }
            }
        }
    }
    Ok(())
}

pub fn run_bash(cmd: &str, cwd: &Path, timeout: Duration, sandbox_on: bool) -> Result<String> {
    // отладочная строка — только по THESEUS_DEBUG=1: безусловный eprintln
    // протекал в строку ввода TUI (агент работает в фоне, ratatui перерисовывает
    // экран, а сырой stderr печатается в позиции курсора — в поле ввода)
    if std::env::var_os("THESEUS_DEBUG").is_some() {
        eprintln!("[debug] run_bash sandbox_on={sandbox_on} status={:?}", crate::sandbox::status());
    }
    let mut command = Command::new("bash");
    command
        .arg("-lc").arg(cmd)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("DEBIAN_FRONTEND", "noninteractive");
    // ядерный sandbox (v0.3.1, урок Codex): Landlock, запись только в cwd + /tmp
    if sandbox_on && crate::sandbox::status() == crate::sandbox::SandboxStatus::Available {
        use std::os::unix::process::CommandExt;
        let ws = cwd.to_path_buf();
        unsafe {
            command.pre_exec(move || {
                crate::sandbox::enforce_workspace(&ws)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::PermissionDenied, e))
            });
        }
    }
    let child = command.spawn()?;
    let t0 = Instant::now();
    let pid = child.id();
    // ожидание с таймаутом через поток
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let status = child.wait_with_output();
        let _ = tx.send(status);
    });
    let out = match rx.recv_timeout(timeout) {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(anyhow!("wait: {e}")),
        Err(_) => {
            let _ = libc_kill(pid);
            return Ok(format!("ERROR: таймаут {}s — процесс убит", timeout.as_secs()));
        }
    };
    let mut s = String::from_utf8_lossy(&out.stdout).to_string();
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    if !err.trim().is_empty() {
        if !s.is_empty() { s.push('\n'); }
        s.push_str(&err);
    }
    let code = out.status.code().unwrap_or(-1);
    s += &format!("\n(exit {code}, {:.1}s)", t0.elapsed().as_secs_f32());
    Ok(cap(s))
}

fn libc_kill(pid: u32) -> i32 {
    extern "C" { fn kill(pid: i32, sig: i32) -> i32; }
    unsafe { kill(pid as i32, 9) }
}

// ---------- Свободные функции для параллельного исполнения (v0.3) ----------

pub fn read_file_free(workspace: &Path, args: &serde_json::Value) -> String {
    let env = ToolEnv::new(workspace);
    env.read_file(args).unwrap_or_else(|e| format!("ERROR: {e}"))
}

pub fn list_files_free(workspace: &Path, args: &serde_json::Value) -> String {
    let env = ToolEnv::new(workspace);
    env.list_files(args).unwrap_or_else(|e| format!("ERROR: {e}"))
}

pub fn grep_free(workspace: &Path, args: &serde_json::Value) -> String {
    let env = ToolEnv::new(workspace);
    env.grep(args).unwrap_or_else(|e| format!("ERROR: {e}"))
}

// ---------- web_fetch (v0.3) ----------

pub fn web_fetch(url: &str, timeout_secs: u64) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs.max(5)))
        .user_agent("theseus/0.3")
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()?;
    let resp = client.get(url).send()?;
    let status = resp.status();
    let html = resp.text().unwrap_or_default();
    Ok(format!("HTTP {}\n\n{}", status.as_u16(), html_to_text(&html)))
}

/// Грубый HTML→text: вырезать script/style, теги → пробелы, сущности, сжать пробелы
fn html_to_text(html: &str) -> String {
    let re_script = regex::Regex::new(r"(?is)<(script|style|noscript)[^>]*>.*?</(script|style|noscript)>").unwrap();
    let re_tags = regex::Regex::new(r"(?s)<[^>]+>").unwrap();
    let re_ws = regex::Regex::new(r"[ \t]+").unwrap();
    let re_nl = regex::Regex::new(r"\n\s*\n+").unwrap();
    let no_script = re_script.replace_all(html, " ");
    let no_tags = re_tags.replace_all(&no_script, "\n");
    let unescaped = no_tags
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    let collapsed = re_ws.replace_all(&unescaped, " ");
    let clean = re_nl.replace_all(&collapsed, "\n");
    let trimmed: Vec<&str> = clean.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    cap(trimmed.join("\n"))
}

/// Обрезка вывода до OUTPUT_LIMIT БАЙТ (BUG-QA-EDIT-03): голова и хвост
/// по половинному байтовому лимиту, срезы только на границах символов
/// UTF-8; маркер показывает реальное число выкинутых байт (всегда > 0:
/// хвост начинается строго позже конца головы, т.к. s.len() > OUTPUT_LIMIT).
fn cap(s: String) -> String {
    if s.len() > OUTPUT_LIMIT {
        let half = OUTPUT_LIMIT / 2;
        // голова — первые `half` байт, отступ назад до границы символа
        let mut head_end = half;
        while !s.is_char_boundary(head_end) {
            head_end -= 1;
        }
        // хвост — последние `half` байт, отступ вперёд до границы символа;
        // s.len() - half > half >= head_end, поэтому части не пересекаются
        let mut tail_start = s.len() - half;
        while !s.is_char_boundary(tail_start) {
            tail_start += 1;
        }
        let skipped = tail_start - head_end;
        format!("{}\n... [обрезано {skipped} байт] ...\n{}", &s[..head_end], &s[tail_start..])
    } else {
        s
    }
}

/// Публичная обёртка cap для других модулей (v0.3)
pub fn cap_pub(s: String) -> String { cap(s) }

// ---------------- web_search (v0.3.1) ----------------

/// Минимальный percent-encoding (alnum + -_.~)
fn urlencode(s: &str) -> String {
    s.bytes().map(|b| match b {
        b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
        b' ' => "+".into(),
        _ => format!("%{b:02X}"),
    }).collect()
}

pub fn web_search(query: &str, timeout_secs: u64) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs.max(5)))
        .user_agent("theseus/0.3.1")
        .build()?;
    let mut out = String::new();

    // 1) DuckDuckGo Instant Answer JSON
    let ddg = format!("https://api.duckduckgo.com/?q={}&format=json&no_html=1&no_redirect=1",
                      urlencode(query));
    if let Ok(resp) = client.get(&ddg).send() {
        if let Ok(text) = resp.text() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(abs) = v["AbstractText"].as_str().filter(|s| !s.is_empty()) {
                    let src = v["AbstractURL"].as_str().unwrap_or("");
                    out += &format!("DDG: {abs} ({src})\n");
                }
                if let Some(topics) = v["RelatedTopics"].as_array() {
                    for t in topics.iter().take(4) {
                        let txt = t["Text"].as_str().unwrap_or("");
                        let url = t["FirstURL"].as_str().unwrap_or("");
                        if !txt.is_empty() {
                            out += &format!("- {txt} — {url}\n");
                        }
                    }
                }
            }
        }
    }

    // 2) Wikipedia OpenSearch (fallback/augment)
    let wiki = format!("https://ru.wikipedia.org/w/api.php?action=opensearch&format=json&limit=5&search={}",
                       urlencode(query));
    if let Ok(resp) = client.get(&wiki).send() {
        if let Ok(text) = resp.text() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                if let (Some(titles), Some(urls)) = (v[1].as_array(), v[3].as_array()) {
                    if !titles.is_empty() {
                        out += "Wikipedia:\n";
                    }
                    for (t, u) in titles.iter().zip(urls.iter()) {
                        out += &format!("- {} — {}\n", t.as_str().unwrap_or(""), u.as_str().unwrap_or(""));
                    }
                }
            }
        }
    }

    if out.is_empty() {
        out = format!("(ничего не найдено по запросу «{query}»)");
    }
    Ok(cap(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_exact_match() {
        let dir = std::env::temp_dir().join("theseus_test_edit");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "hello world\nbye\n").unwrap();
        let mut env = ToolEnv::new(&dir);
        let r = env.call("edit_file", &serde_json::json!({"path":"a.txt","old_string":"world","new_string":"rust"}));
        assert!(r.starts_with("OK"));
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "hello rust\nbye\n");
    }

    #[test]
    fn edit_ambiguous() {
        let dir = std::env::temp_dir().join("theseus_test_edit2");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "x x x\n").unwrap();
        let mut env = ToolEnv::new(&dir);
        let r = env.call("edit_file", &serde_json::json!({"path":"a.txt","old_string":"x","new_string":"y"}));
        assert!(r.contains("ERROR"));
    }

    #[test]
    fn todo_single_in_progress() {
        let dir = std::env::temp_dir().join("theseus_test_todo");
        std::fs::create_dir_all(&dir).unwrap();
        let mut env = ToolEnv::new(&dir);
        let r = env.call("todo_write", &serde_json::json!({"todos":[
            {"content":"a","status":"in_progress"},{"content":"b","status":"in_progress"}]}));
        assert!(r.contains("ERROR"));
    }

    #[test]
    fn edit_fuzzy_indent_flex() {
        // old_string с «чужой» индентацией — матч на уровне indent-flex (OpenDev 9-pass)
        let dir = std::env::temp_dir().join("theseus_test_fuzzy1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "    fn main() {\n        x += 1;\n    }\n").unwrap();
        let mut env = ToolEnv::new(&dir);
        let r = env.call("edit_file", &serde_json::json!({
            "path": "a.rs",
            "old_string": "fn main() {\n    x += 1;\n}",
            "new_string": "fn main() {\n    x += 2;\n}"}));
        assert!(r.starts_with("OK"), "{r}");
        assert!(r.contains("fuzzy"), "{r}");
        assert!(std::fs::read_to_string(dir.join("a.rs")).unwrap().contains("x += 2"));
    }

    #[test]
    fn edit_fuzzy_trailing_ws() {
        // в файле висячие пробелы — матч на уровне trailing-ws
        let dir = std::env::temp_dir().join("theseus_test_fuzzy2");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "hello   \nworld\n").unwrap();
        let mut env = ToolEnv::new(&dir);
        let r = env.call("edit_file", &serde_json::json!({
            "path": "a.txt", "old_string": "hello", "new_string": "привет"}));
        assert!(r.starts_with("OK"), "{r}");
        assert!(std::fs::read_to_string(dir.join("a.txt")).unwrap().starts_with("привет"));
    }

    #[test]
    fn edit_not_found_hint() {
        let dir = std::env::temp_dir().join("theseus_test_fuzzy3");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "alpha beta gamma\n").unwrap();
        let mut env = ToolEnv::new(&dir);
        let r = env.call("edit_file", &serde_json::json!({
            "path": "a.txt", "old_string": "zzz yyy", "new_string": "q"}));
        assert!(r.contains("ERROR") && r.contains("Подсказка"), "{r}");
    }

    /// Уникальный временный каталог на тест (pid + тег), чтобы не было гонок.
    fn tdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("theseus-tools-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn apply_patch_add_file_via_tool() {
        let dir = tdir("patch-add");
        let mut env = ToolEnv::new(&dir);
        let r = env.call("apply_patch", &serde_json::json!({
            "patch": "*** Begin Patch\n*** Add File: sub/new.txt\n+hello\n+world\n*** End Patch"}));
        assert!(r.starts_with("OK"), "{r}");
        assert!(r.contains("new.txt"), "список затронутых файлов: {r}");
        assert_eq!(std::fs::read_to_string(dir.join("sub/new.txt")).unwrap(), "hello\nworld\n");
    }

    #[test]
    fn apply_patch_update_file_via_tool_with_preview() {
        let dir = tdir("patch-upd");
        std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let mut env = ToolEnv::new(&dir);
        let r = env.call("apply_patch", &serde_json::json!({
            "patch": "*** Begin Patch\n*** Update File: a.txt\n two\n-three\n+THREE\n*** End Patch"}));
        assert!(r.starts_with("OK"), "{r}");
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\ntwo\nTHREE\n");
        // diff-превью фактической правки присутствует
        assert!(r.contains("preview (unified diff)"), "{r}");
        assert!(r.contains("-three") && r.contains("+THREE"), "{r}");
    }

    #[test]
    fn apply_patch_delete_file_via_tool() {
        let dir = tdir("patch-del");
        std::fs::write(dir.join("gone.txt"), "bye\n").unwrap();
        let mut env = ToolEnv::new(&dir);
        let r = env.call("apply_patch", &serde_json::json!({
            "patch": "*** Begin Patch\n*** Delete File: gone.txt\n*** End Patch"}));
        assert!(r.starts_with("OK"), "{r}");
        assert!(r.contains("gone.txt"), "{r}");
        assert!(!dir.join("gone.txt").exists());
    }

    #[test]
    fn apply_patch_errors_are_text_not_panic() {
        let dir = tdir("patch-err");
        std::fs::write(dir.join("c.txt"), "actual\ncontent\n").unwrap();
        let mut env = ToolEnv::new(&dir);
        // битый формат
        let r = env.call("apply_patch", &serde_json::json!({"patch": "not a patch"}));
        assert!(r.starts_with("ERROR"), "{r}");
        // контекст не найден — файл не тронут
        let r = env.call("apply_patch", &serde_json::json!({
            "patch": "*** Begin Patch\n*** Update File: c.txt\n-missing\n+new\n*** End Patch"}));
        assert!(r.starts_with("ERROR"), "{r}");
        assert_eq!(std::fs::read_to_string(dir.join("c.txt")).unwrap(), "actual\ncontent\n");
    }

    #[test]
    fn edit_file_diff_preview_present() {
        let dir = tdir("edit-prev");
        std::fs::write(dir.join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let mut env = ToolEnv::new(&dir);
        let r = env.call("edit_file", &serde_json::json!({
            "path": "a.txt", "old_string": "beta", "new_string": "BETA"}));
        assert!(r.starts_with("OK"), "{r}");
        assert!(r.contains("preview (unified diff)"), "{r}");
        assert!(r.contains("@@") && r.contains("-beta") && r.contains("+BETA"), "{r}");
    }

    #[test]
    fn matchers_escaped_newline_edit_passes() {
        // Модель прислала \n двумя символами — срабатывает уровень escape_normalized.
        let dir = tdir("match-esc");
        std::fs::write(dir.join("a.txt"), "line1\nline2\n").unwrap();
        let mut env = ToolEnv::new(&dir);
        let r = env.call("edit_file", &serde_json::json!({
            "path": "a.txt", "old_string": "line1\\nline2", "new_string": "L1\nL2"}));
        assert!(r.starts_with("OK"), "{r}");
        assert!(r.contains("fuzzy: escape_normalized"), "{r}");
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "L1\nL2\n");
    }

    #[test]
    fn matchers_ambiguous_rejected_with_line_numbers() {
        // old_string с «чужими» краями матчится дважды по trim — правка отклоняется.
        let dir = tdir("match-amb");
        std::fs::write(dir.join("a.txt"), "x\ny\nx\n").unwrap();
        let mut env = ToolEnv::new(&dir);
        let r = env.call("edit_file", &serde_json::json!({
            "path": "a.txt", "old_string": "  x", "new_string": "z"}));
        assert!(r.contains("ERROR"), "{r}");
        assert!(r.contains("строки: 1, 3"), "{r}");
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "x\ny\nx\n");
    }

    #[test]
    fn edit_exact_ambiguous_reports_line_numbers() {
        // BUG-QA-EDIT-01: exact-путь при множественном вхождении тоже даёт
        // номера строк вхождений (1-based), как fuzzy-вентиль.
        let dir = tdir("edit-exact-lines");
        std::fs::write(dir.join("a.txt"), "one\ntwo\nfoo\nthree\nfoo\n").unwrap();
        let mut env = ToolEnv::new(&dir);
        let r = env.call("edit_file", &serde_json::json!({
            "path": "a.txt", "old_string": "foo", "new_string": "bar"}));
        assert!(r.contains("ERROR"), "{r}");
        assert!(r.contains("встречается 2 раз (строки: 3, 5)"), "{r}");
        // файл не тронут
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "one\ntwo\nfoo\nthree\nfoo\n");
    }

    #[test]
    fn edit_fuzzy_replacement_uses_dominant_crlf() {
        // BUG-QA-EDIT-02: fuzzy-замена в CRLF-файле вставляет блок с CRLF,
        // а не с «голым» LF, присланным моделью.
        let dir = tdir("edit-crlf");
        std::fs::write(dir.join("a.txt"), "one\r\n  two\r\nthree\r\n").unwrap();
        let mut env = ToolEnv::new(&dir);
        // old_string с хвостовым пробелом: exact не матчится, срабатывает line_trim
        let r = env.call("edit_file", &serde_json::json!({
            "path": "a.txt", "old_string": "two ", "new_string": "TWO\nx"}));
        assert!(r.starts_with("OK"), "{r}");
        assert!(r.contains("fuzzy"), "{r}");
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "one\r\nTWO\r\nx\r\nthree\r\n");
    }

    #[test]
    fn edit_replace_all_trim_blocks_uses_dominant_crlf() {
        // BUG-QA-EDIT-02 (ветка replace_all по trim-блокам): каждая вставленная
        // строка блока получает CRLF, а не только внутренние переводы замены.
        let dir = tdir("edit-crlf-all");
        std::fs::write(dir.join("a.txt"), "x\r\ny\r\nx\r\n").unwrap();
        let mut env = ToolEnv::new(&dir);
        let r = env.call("edit_file", &serde_json::json!({
            "path": "a.txt", "old_string": "x ", "new_string": "z\nw", "replace_all": true}));
        assert!(r.starts_with("OK"), "{r}");
        assert!(r.contains("fuzzy: trim"), "{r}");
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "z\r\nw\r\ny\r\nz\r\nw\r\n");
    }

    #[test]
    fn edit_fuzzy_lf_file_stays_lf() {
        // Контроль BUG-QA-EDIT-02: в LF-файле поведение не меняется.
        let dir = tdir("edit-lf");
        std::fs::write(dir.join("a.txt"), "one\n  two\nthree\n").unwrap();
        let mut env = ToolEnv::new(&dir);
        let r = env.call("edit_file", &serde_json::json!({
            "path": "a.txt", "old_string": "two ", "new_string": "TWO\nx"}));
        assert!(r.starts_with("OK"), "{r}");
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "one\nTWO\nx\nthree\n");
    }

    #[test]
    fn concept_explain_empty_related_has_no_dangling_tail() {
        // QA-ML-002: при пустом списке похожих — без оборванного «Похожие: ».
        let msg = concept_not_found_message("zzz", &[]);
        assert_eq!(msg, "концепт «zzz» не найден");
        assert!(!msg.contains("Похожие"), "{msg}");
        // непустой список — по-прежнему перечисляется
        let card = crate::ml_concepts::ConceptCard {
            slug: "grpo".into(),
            card_type: "algorithmic".into(),
            level: "β".into(),
            formality: "B".into(),
            title: "GRPO".into(),
            aliases: vec![],
            related: vec![],
            family: "rl".into(),
            sources: vec![],
            body: String::new(),
        };
        let msg = concept_not_found_message("zzz", &[&card]);
        assert_eq!(msg, "концепт «zzz» не найден. Похожие: grpo");
    }

    /// Полезная ошибка неизвестного инструмента (живой кейс 21.07 — модель
    /// вызвала скилл «agent-sessions» как инструмент): список реальных
    /// инструментов + подсказка про skill.
    #[test]
    fn unknown_tool_error_lists_tools_and_skill_hint() {
        let mut env = ToolEnv::new(Path::new("/tmp"));
        let out = env.call("agent-sessions", &serde_json::json!({}));
        assert!(out.contains("unknown tool agent-sessions"), "{out}");
        assert!(out.contains("read_file"), "список инструментов: {out}");
        assert!(out.contains("skill"), "подсказка про скилл: {out}");
    }

    /// list_files: несуществующий путь и файл — явные ошибки, а не «(пусто)»
    /// (живой кейс 21.07: модель приняла отсутствующий каталог за пустой).
    #[test]
    fn list_files_errors_on_missing_path_and_file() {
        let dir = tdir("list-missing");
        let mut env = ToolEnv::new(&dir);
        let out = env.call("list_files", &serde_json::json!({"path": "no/such/dir"}));
        assert!(out.starts_with("ERROR"), "{out}");
        assert!(out.contains("нет такого файла или каталога"), "{out}");
        std::fs::write(dir.join("f.txt"), "x").unwrap();
        let out2 = env.call("list_files", &serde_json::json!({"path": "f.txt"}));
        assert!(out2.starts_with("ERROR") && out2.contains("не каталог"), "{out2}");
        // пустой каталог — по-прежнему честное «(пусто)»
        std::fs::create_dir(dir.join("empty")).unwrap();
        let out3 = env.call("list_files", &serde_json::json!({"path": "empty"}));
        assert_eq!(out3, "(пусто)");
    }

    #[test]
    fn todo_write_stores_into_todo_list() {
        let dir = tdir("todo-store");
        let mut env = ToolEnv::new(&dir);
        let r = env.call("todo_write", &serde_json::json!({"todos":[
            {"content":"a","status":"done"},{"content":"b","status":"in_progress"}]}));
        assert!(r.starts_with("OK"), "{r}");
        // внешний строковый снимок для гейта в agent сохранён
        assert_eq!(env.todos.len(), 2);
        assert_eq!(env.todos[1].status, "in_progress");
        // внутреннее хранилище crate::todo::TodoList заполнено теми же задачами
        assert_eq!(env.todo_list.len(), 2);
        assert_eq!(env.todo_list.items()[0].status, crate::todo::TodoStatus::Done);
        assert_eq!(env.todo_list.items()[1].status, crate::todo::TodoStatus::InProgress);
    }

    /// Регрессия (найдено живыми тестами DeepSeek, tests/live_deepseek.rs):
    /// cap() для текста 21–40 КБ с кириллицей паниковал
    /// "attempt to subtract with overflow" — байт много, символов меньше лимита.
    #[test]
    fn cap_unicode_byte_len_above_limit_no_panic() {
        // 12 000 кириллических символов = 24 000 байт > OUTPUT_LIMIT (20 480).
        let s = "ж".repeat(12_000);
        let total = s.len();
        let out = cap(s);
        assert!(out.contains("обрезано"), "{out}");
        // бюджет cap — в БАЙТАХ (половины лимита на голову и хвост + маркер)
        assert!(out.len() <= OUTPUT_LIMIT + 64, "bytes={}", out.len());
        // голова и хвост не пересекаются: сумма их байт <= исходного
        let marker = "\n... [обрезано ";
        let (head, rest) = out.split_once(marker).unwrap();
        let tail = rest.rsplit_once("...\n").map(|x| x.1).unwrap_or("");
        assert!(head.len() + tail.len() <= total);
    }

    /// BUG-QA-EDIT-03: бюджет cap — в байтах. 30 КБ кириллицы (2 байта/символ)
    /// обрезаются до OUTPUT_LIMIT байт + маркер; маркер несёт реальное
    /// (> 0) число выкинутых байт; срезы — на границах символов UTF-8.
    #[test]
    fn cap_byte_budget_cyrillic_30k() {
        let s = "ф".repeat(15_000); // 30 000 байт > OUTPUT_LIMIT
        let out = cap(s.clone());
        assert!(out.len() <= OUTPUT_LIMIT + 64, "bytes={}", out.len());
        let marker = "\n... [обрезано ";
        let (head, rest) = out.split_once(marker).unwrap();
        let (skipped_txt, tail) = rest.split_once(" байт] ...\n").unwrap();
        let skipped: usize = skipped_txt.parse().unwrap();
        assert!(skipped > 0, "маркер обязан показывать реально выкинутые байты");
        // выкинуто ровно столько байт, сколько не вошло между головой и хвостом
        assert_eq!(skipped, s.len() - head.len() - tail.len());
        // срезы на границах символов: голова и хвост — целые «ф» (2 байта)
        assert_eq!(head.len() % 2, 0, "голова резалась не по границе UTF-8");
        assert_eq!(tail.len() % 2, 0, "хвост резался не по границе UTF-8");
        // содержимое головы/хвоста не искажено
        assert!(head.chars().all(|c| c == 'ф') && tail.chars().all(|c| c == 'ф'));
    }
}

#[cfg(test)]
mod sandbox_e2e_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn live_sandbox_blocks_home_write() {
        let dir = std::env::temp_dir().join("theseus_sbx_e2e");
        std::fs::create_dir_all(&dir).unwrap();
        let home = std::env::var("HOME").unwrap();
        let target = format!("{home}/theseus_sbx_live_forbidden.txt");
        let _ = std::fs::remove_file(&target);
        let out = run_bash(&format!("touch {target}"), &dir, Duration::from_secs(10), true).unwrap();
        let exists = std::path::Path::new(&target).exists();
        let _ = std::fs::remove_file(&target);
        assert!(!exists, "файл не должен был создаться; вывод: {out}");
    }
}
