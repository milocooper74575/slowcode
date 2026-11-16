//! Shared application layer for the slowcode TUI, headless run and REPL.
//!
//! This crate owns session assembly, model selection and slash-command
//! parsing. It never renders and never reads the terminal.

mod command;
pub mod plan;
mod runtime;
mod text;
pub mod workflow;

pub use command::{
    Command, CommandCompletion, SessionCommand, command_specs, complete, complete_answer,
    parse_command,
};
pub use runtime::{
    ContextBreakdown, ContextSlice, McpInstallReport, Runtime, RuntimeConfig, RuntimeError,
    SessionChoice, StartedRuntime, apply_max_tokens, base_system_prompt, known_models,
    system_prompt,
};
pub use text::{escape_terminal, outcome_text};
