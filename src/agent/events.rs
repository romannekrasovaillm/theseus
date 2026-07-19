//! События агента для TUI/лога и канала событий.

/// События агента для TUI/лога
#[derive(Debug, Clone)]
pub enum AgentEvent {
    UserMsg(String),
    AgentText(String),
    AgentTextDelta(String),
    Reasoning(usize),
    ToolCall { name: String, args: String, decision: String },
    ToolResult { name: String, preview: String, ok: bool },
    Status { turns: usize, est_tokens: usize, mode: String },
    Compact { from_msgs: usize, to_msgs: usize },
    TodoRejected(String),
    Finished(String),
    Error(String),
    PermAsk { key: u64, question: String },
    Accounting { calls: u64, prompt_t: u64, completion_t: u64 },
    GoalSet(String),
    PlanChanged(bool),
    MemoryConsolidated(usize),
    HookNote(String),
}

