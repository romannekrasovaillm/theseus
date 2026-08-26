//! События агента для TUI/лога и канала событий.

/// События агента для TUI/лога
#[derive(Debug, Clone)]
pub enum AgentEvent {
    UserMsg(String),
    AgentText(String),
    AgentTextDelta(String),
    Reasoning(usize),
    ToolCall {
        name: String,
        args: String,
        decision: String,
    },
    ToolResult {
        name: String,
        preview: String,
        ok: bool,
    },
    Status {
        turns: usize,
        est_tokens: usize,
        mode: String,
    },
    Compact {
        from_msgs: usize,
        to_msgs: usize,
    },
    TodoRejected(String),
    Finished(String),
    Error(String),
    PermAsk {
        key: u64,
        question: String,
    },
    Accounting {
        calls: u64,
        prompt_t: u64,
        completion_t: u64,
    },
    GoalSet(String),
    PlanChanged(bool),
    MemoryConsolidated(usize),
    HookNote(String),
    /// Дельта текста пир-агента (stream-json, нативный стриминг фаз 1-2):
    /// TUI рендерит peer-блок на месте; peer — имя из реестра (claude/kimi).
    PeerDelta {
        /// Имя пира из реестра.
        peer: String,
        /// Текстовый блок ассистента пира.
        text: String,
    },
    /// Вызов инструмента пир-агентом (из stream-json разбора).
    PeerToolUse {
        /// Имя пира из реестра.
        peer: String,
        /// Имя инструмента пира.
        name: String,
        /// Аргументы вызова (JSON-строкой).
        args: String,
    },
}
