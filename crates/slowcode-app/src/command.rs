//! Parsed slash commands for the TUI and REPL.
//!
//! Commands are resolved before any prompt is sent to the model. Unknown
//! commands are reported, never forwarded as ordinary text.

use slowcode_roles::RoleName;

/// One parsed slash command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    Help(Option<String>),
    Model(Option<String>),
    Session(SessionCommand),
    Role(Option<RoleName>),
    Context(Option<String>),
    Cache,
    Login,
    Logout,
    Skills(Option<String>),
    Mcp(Option<String>),
    Feedback(String),
    Update,
    Quit,
    Plan(String),
    Answer(String),
    Scaffold,
    Continue,
    Test,
    Fix,
    Execute,
    Distill(String),
    Approve,
    Status(Option<String>),
    Next,
    Workflow(Option<String>),
    Reclassify(String),
    Hint(String),
    Ask(String),
    Unknown(String),
}

/// `/session` subcommands.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionCommand {
    List,
    New,
    Rename(String),
    Switch(String),
}

/// Static description used by `/help` and completion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandSpec {
    pub name: &'static str,
    pub args: &'static str,
    pub description: &'static str,
}

/// Commands whose first action may call the model.
///
/// Keep every command in exactly one of this list and [`LOCAL_COMMANDS`].
/// The REPL uses this table to fail at the command boundary instead of
/// discovering a missing credential inside a model turn.
const MODEL_COMMANDS: &[&str] = &[
    "/plan",
    "/answer",
    "/scaffold",
    "/continue",
    "/test",
    "/fix",
    "/execute",
    "/distill",
    "/approve",
    "/reclassify",
    "/hint",
    "/ask",
];

/// Commands that never call the model.
const LOCAL_COMMANDS: &[&str] = &[
    "/help",
    "/model",
    "/session",
    "/role",
    "/context",
    "/cache",
    "/login",
    "/logout",
    "/skills",
    "/mcp",
    "/feedback",
    "/update",
    "/status",
    "/next",
    "/workflow",
    "/quit",
];

impl CommandSpec {
    /// Whether starting this command may call the model.
    #[must_use]
    pub fn requires_model(&self) -> bool {
        command_requires_model(self.name)
    }
}

/// Whether a command name may call the model.
///
/// Unknown names are treated as local: the caller must reject them anyway.
#[must_use]
pub fn command_requires_model(name: &str) -> bool {
    if LOCAL_COMMANDS.contains(&name) {
        return false;
    }
    MODEL_COMMANDS.contains(&name)
}

/// Commands available in the TUI. Order is stable for rendering.
pub const COMMAND_SPECS: &[CommandSpec] = &[
    CommandSpec {
        name: "/help",
        args: "[command]",
        description: "show help and key bindings",
    },
    CommandSpec {
        name: "/model",
        args: "[name]",
        description: "list models or switch model",
    },
    CommandSpec {
        name: "/session",
        args: "[list|new|rename|switch]",
        description: "list, create, rename or switch sessions",
    },
    CommandSpec {
        name: "/role",
        args: "[navigator|driver]",
        description: "show or switch the active role",
    },
    CommandSpec {
        name: "/context",
        args: "[reload [file]]",
        description: "show or reload project context",
    },
    CommandSpec {
        name: "/cache",
        args: "",
        description: "show prompt-cache counters",
    },
    CommandSpec {
        name: "/login",
        args: "",
        description: "show the credential or enter an API key",
    },
    CommandSpec {
        name: "/logout",
        args: "",
        description: "remove the API key from the workspace .env",
    },
    CommandSpec {
        name: "/skills",
        args: "[install|remove|reload]",
        description: "list or manage skills",
    },
    CommandSpec {
        name: "/mcp",
        args: "[add|enable|disable|test|tools|remove|reload]",
        description: "list or manage MCP servers",
    },
    CommandSpec {
        name: "/feedback",
        args: "[--title TEXT] [--note TEXT] [--plan PATH] [text]",
        description: "write an issue-ready feedback bundle for this session",
    },
    CommandSpec {
        name: "/update",
        args: "",
        description: "check for a newer release, then update after confirmation",
    },
    CommandSpec {
        name: "/plan",
        args: "[--new] <task>",
        description: "explore and navigate, then pause with a plan",
    },
    CommandSpec {
        name: "/answer",
        args: "<text>",
        description: "record decisions and reclassify the plan",
    },
    CommandSpec {
        name: "/scaffold",
        args: "",
        description: "generate scaffolds for user-owned tasks",
    },
    CommandSpec {
        name: "/continue",
        args: "",
        description: "check the user implementation and enter test fix",
    },
    CommandSpec {
        name: "/test",
        args: "",
        description: "run the fixed test contract",
    },
    CommandSpec {
        name: "/fix",
        args: "",
        description: "fix known failures from the last test run",
    },
    CommandSpec {
        name: "/execute",
        args: "",
        description: "compatibility alias for /fix followed by /test",
    },
    CommandSpec {
        name: "/distill",
        args: "<explanation>",
        description: "record a teach-back and produce a gap report",
    },
    CommandSpec {
        name: "/approve",
        args: "",
        description: "approve the current task classification",
    },
    CommandSpec {
        name: "/status",
        args: "[--workflow <id>]",
        description: "show the active workflow phase and next action",
    },
    CommandSpec {
        name: "/next",
        args: "",
        description: "show the next question or phase action",
    },
    CommandSpec {
        name: "/workflow",
        args: "[list|use <id>|end|selector [selector]]",
        description: "list, switch, end or reset the test selector",
    },
    CommandSpec {
        name: "/reclassify",
        args: "<text>",
        description: "freeze writes and ask for a new unknown-domain gap",
    },
    CommandSpec {
        name: "/hint",
        args: "<question>",
        description: "ask for a one-run read-only hint without changing the phase",
    },
    CommandSpec {
        name: "/ask",
        args: "<question>",
        description: "ask a side question without answering the open plan question",
    },
    CommandSpec {
        name: "/quit",
        args: "",
        description: "write HANDOFF.md with the handoff skill, then exit",
    },
];

/// Return the help table.
pub fn command_specs() -> &'static [CommandSpec] {
    COMMAND_SPECS
}

/// One inline completion suggestion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandCompletion {
    /// Text to append at the cursor when the user presses Tab.
    pub suffix: String,
    /// Complete input after accepting the suggestion.
    pub full: String,
    /// One-line explanation of what accepting the suggestion does.
    pub description: Option<String>,
}

/// Suggest a command name or first argument based on the current input.
///
/// Completion only applies at the end of the composer. The caller renders the
/// suffix dimly and accepts it with Tab.
#[must_use]
pub fn complete(input: &str, cursor: usize) -> Option<CommandCompletion> {
    if cursor != input.len() || !input.starts_with('/') {
        return None;
    }
    let first_end = input.find(char::is_whitespace).unwrap_or(input.len());
    if first_end == input.len() {
        return complete_from(input, input, command_names());
    }

    let command = &input[..first_end];
    let rest = input[first_end..].trim_start();
    if rest.contains(char::is_whitespace) {
        return None;
    }
    if rest.is_empty() && command != "/feedback" {
        return None;
    }
    complete_from(
        input,
        rest,
        argument_candidates(command)
            .iter()
            .copied()
            .map(|candidate| (candidate, argument_description(command, candidate))),
    )
}

/// Complete `/answer` with an open workflow question id.
///
/// `questions` should be the open questions in presentation order. The caller
/// is responsible for reading them from the active workflow.
#[must_use]
pub fn complete_answer(
    input: &str,
    cursor: usize,
    questions: &[crate::workflow::Question],
) -> Option<CommandCompletion> {
    if cursor != input.len() || !input.starts_with("/answer") {
        return None;
    }
    let rest = &input["/answer".len()..];
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = rest.trim_start();
    if rest.contains(char::is_whitespace) {
        return None;
    }
    let prefix = if rest.is_empty() { "" } else { rest };
    if !prefix.is_empty() && !prefix.starts_with('#') {
        return None;
    }
    let question = questions
        .iter()
        .find(|question| format!("#{}", question.question_id).starts_with(prefix))?;
    let candidate = format!("#{}", question.question_id);
    let suffix = candidate[prefix.len()..].to_owned();
    Some(CommandCompletion {
        full: format!("{input}{suffix}"),
        suffix,
        description: Some(question.prompt.clone()),
    })
}

fn complete_from(
    full_input: &str,
    prefix: &str,
    candidates: impl IntoIterator<Item = (&'static str, Option<&'static str>)>,
) -> Option<CommandCompletion> {
    let candidate = candidates
        .into_iter()
        .find(|(candidate, _)| candidate.starts_with(prefix) && *candidate != prefix)?;
    let suffix = candidate.0[prefix.len()..].to_owned();
    Some(CommandCompletion {
        full: format!("{full_input}{suffix}"),
        suffix,
        description: candidate.1.map(str::to_owned),
    })
}

fn command_names() -> impl Iterator<Item = (&'static str, Option<&'static str>)> {
    COMMAND_SPECS
        .iter()
        .map(|spec| (spec.name, Some(spec.description)))
}

fn argument_candidates(command: &str) -> &'static [&'static str] {
    match command {
        "/session" => &["list", "new", "rename", "switch"],
        "/role" => &["navigator", "driver"],
        "/model" => &[
            "deepseek-flash",
            "deepseek-v4-pro",
            "deepseek-v4-flash",
            "deepseek-v4-flash-vision-exp",
        ],
        "/context" => &["reload"],
        "/skills" => &["install", "remove", "reload"],
        "/mcp" => &["add", "test", "tools", "remove", "reload"],
        "/feedback" => &["--plan", "--title", "--note"],
        "/workflow" => &["list", "use", "end", "selector"],
        "/help" => &[
            "help",
            "model",
            "session",
            "role",
            "context",
            "cache",
            "login",
            "logout",
            "skills",
            "mcp",
            "feedback",
            "update",
            "plan",
            "answer",
            "scaffold",
            "continue",
            "test",
            "fix",
            "execute",
            "distill",
            "approve",
            "status",
            "next",
            "workflow",
            "reclassify",
            "quit",
        ],
        _ => &[],
    }
}

fn argument_description(command: &str, candidate: &str) -> Option<&'static str> {
    match (command, candidate) {
        ("/feedback", "--plan") => Some("attach a plan artifact"),
        ("/feedback", "--title") => Some("override the derived title"),
        ("/feedback", "--note") => Some("same as typing what actually happened"),
        _ => None,
    }
}

/// Parse one line. Returns `None` for ordinary prompts.
pub fn parse_command(input: &str) -> Option<Command> {
    let input = input.trim();
    if !input.starts_with('/') {
        return None;
    }
    let (name, rest) = split_name(input);
    Some(match lookup(name) {
        Some(parse) => parse(rest),
        None => Command::Unknown(name.to_owned()),
    })
}

type CommandParser = fn(&str) -> Command;

/// Dispatch table. `/exit` is an alias and deliberately absent from
/// `COMMAND_SPECS`.
const COMMAND_TABLE: &[(&str, CommandParser)] = &[
    ("/help", parse_help),
    ("/model", parse_model),
    ("/session", parse_session_command),
    ("/role", parse_role_command),
    ("/context", parse_context),
    ("/cache", parse_cache),
    ("/login", parse_login),
    ("/logout", parse_logout),
    ("/skills", parse_skills),
    ("/mcp", parse_mcp),
    ("/feedback", parse_feedback),
    ("/update", parse_update),
    ("/quit", parse_quit),
    ("/exit", parse_quit),
    ("/plan", parse_plan),
    ("/answer", parse_answer),
    ("/scaffold", parse_scaffold),
    ("/continue", parse_continue),
    ("/test", parse_test),
    ("/fix", parse_fix),
    ("/execute", parse_execute),
    ("/distill", parse_distill),
    ("/approve", parse_approve),
    ("/status", parse_status),
    ("/next", parse_next),
    ("/workflow", parse_workflow),
    ("/reclassify", parse_reclassify),
    ("/hint", parse_hint),
    ("/ask", parse_ask),
];

fn lookup(name: &str) -> Option<CommandParser> {
    COMMAND_TABLE
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, parse)| *parse)
}

fn split_name(input: &str) -> (&str, &str) {
    let mut parts = input.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or_default();
    let rest = parts.next().unwrap_or_default().trim();
    (name, rest)
}

fn optional_arg(rest: &str) -> Option<String> {
    (!rest.is_empty()).then(|| rest.to_owned())
}

fn parse_help(rest: &str) -> Command {
    Command::Help(optional_arg(rest))
}

fn parse_model(rest: &str) -> Command {
    Command::Model(optional_arg(rest))
}

fn parse_session_command(rest: &str) -> Command {
    Command::Session(parse_session(rest))
}

fn parse_role_command(rest: &str) -> Command {
    if rest.is_empty() {
        return Command::Role(None);
    }
    match RoleName::parse(rest) {
        Some(role) => Command::Role(Some(role)),
        None => Command::Unknown(format!("unknown role: {rest}")),
    }
}

fn parse_context(rest: &str) -> Command {
    Command::Context(optional_arg(rest))
}

fn parse_cache(_rest: &str) -> Command {
    Command::Cache
}

fn parse_login(_rest: &str) -> Command {
    Command::Login
}

fn parse_logout(_rest: &str) -> Command {
    Command::Logout
}

fn parse_skills(rest: &str) -> Command {
    Command::Skills(optional_arg(rest))
}

fn parse_mcp(rest: &str) -> Command {
    Command::Mcp(optional_arg(rest))
}

fn parse_feedback(rest: &str) -> Command {
    Command::Feedback(rest.to_owned())
}

fn parse_update(_rest: &str) -> Command {
    Command::Update
}

fn parse_quit(_rest: &str) -> Command {
    Command::Quit
}

fn parse_plan(rest: &str) -> Command {
    Command::Plan(rest.to_owned())
}

fn parse_answer(rest: &str) -> Command {
    Command::Answer(rest.to_owned())
}

fn parse_scaffold(_rest: &str) -> Command {
    Command::Scaffold
}

fn parse_continue(_rest: &str) -> Command {
    Command::Continue
}

fn parse_test(_rest: &str) -> Command {
    Command::Test
}

fn parse_fix(_rest: &str) -> Command {
    Command::Fix
}

fn parse_execute(_rest: &str) -> Command {
    Command::Execute
}

fn parse_distill(rest: &str) -> Command {
    Command::Distill(rest.to_owned())
}

fn parse_approve(_rest: &str) -> Command {
    Command::Approve
}

fn parse_status(rest: &str) -> Command {
    Command::Status(optional_arg(rest))
}

fn parse_next(_rest: &str) -> Command {
    Command::Next
}

fn parse_workflow(rest: &str) -> Command {
    Command::Workflow(optional_arg(rest))
}

fn parse_reclassify(rest: &str) -> Command {
    Command::Reclassify(rest.to_owned())
}

fn parse_hint(rest: &str) -> Command {
    Command::Hint(rest.to_owned())
}

fn parse_ask(rest: &str) -> Command {
    Command::Ask(rest.to_owned())
}

fn parse_session(rest: &str) -> SessionCommand {
    let mut parts = rest.splitn(2, char::is_whitespace);
    let verb = parts.next().unwrap_or_default();
    let argument = parts.next().unwrap_or_default().trim();
    match verb {
        "" | "list" => SessionCommand::List,
        "new" => SessionCommand::New,
        "rename" => SessionCommand::Rename(argument.to_owned()),
        "switch" => SessionCommand::Switch(argument.to_owned()),
        other => SessionCommand::Switch(other.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_prompt_is_not_a_command() {
        assert_eq!(parse_command("read hello.txt"), None);
    }

    #[test]
    fn every_command_declares_whether_it_needs_the_model() {
        for spec in COMMAND_SPECS {
            let model = MODEL_COMMANDS.contains(&spec.name);
            let local = LOCAL_COMMANDS.contains(&spec.name);
            assert_ne!(
                model, local,
                "{} must appear in exactly one capability list",
                spec.name
            );
            assert_eq!(spec.requires_model(), model);
        }
        assert_eq!(
            MODEL_COMMANDS.len() + LOCAL_COMMANDS.len(),
            COMMAND_SPECS.len(),
            "a command is missing from both capability lists"
        );
    }

    #[test]
    fn plan_commands_need_the_model_but_status_does_not() {
        assert!(command_requires_model("/plan"));
        assert!(command_requires_model("/execute"));
        assert!(!command_requires_model("/status"));
        assert!(!command_requires_model("/role"));
    }

    #[test]
    fn parses_high_frequency_commands() {
        assert_eq!(parse_command("/help"), Some(Command::Help(None)));
        assert_eq!(parse_command("/model"), Some(Command::Model(None)));
        assert_eq!(parse_command("/login"), Some(Command::Login));
        assert_eq!(
            parse_command("/model deepseek-v4-pro"),
            Some(Command::Model(Some("deepseek-v4-pro".to_owned())))
        );
        assert_eq!(parse_command("/cache"), Some(Command::Cache));
        assert_eq!(parse_command("/update"), Some(Command::Update));
        assert_eq!(parse_command("/quit"), Some(Command::Quit));
        assert_eq!(parse_command("/exit"), Some(Command::Quit));
    }

    #[test]
    fn parses_session_commands() {
        assert_eq!(
            parse_command("/session"),
            Some(Command::Session(SessionCommand::List))
        );
        assert_eq!(
            parse_command("/session new"),
            Some(Command::Session(SessionCommand::New))
        );
        assert_eq!(
            parse_command("/session switch session-abc"),
            Some(Command::Session(SessionCommand::Switch(
                "session-abc".to_owned()
            )))
        );
    }

    #[test]
    fn parses_roles_and_plan_commands() {
        assert_eq!(
            parse_command("/role navigator"),
            Some(Command::Role(Some(RoleName::Navigator)))
        );
        assert_eq!(
            parse_command("/plan build a parser"),
            Some(Command::Plan("build a parser".to_owned()))
        );
        assert_eq!(
            parse_command("/distill I used a trait"),
            Some(Command::Distill("I used a trait".to_owned()))
        );
        assert_eq!(parse_command("/approve"), Some(Command::Approve));
        assert_eq!(parse_command("/test"), Some(Command::Test));
        assert_eq!(parse_command("/fix"), Some(Command::Fix));
        assert_eq!(parse_command("/next"), Some(Command::Next));
        assert_eq!(
            parse_command("/status --workflow plan-1"),
            Some(Command::Status(Some("--workflow plan-1".to_owned())))
        );
        assert_eq!(
            parse_command("/workflow use plan-1"),
            Some(Command::Workflow(Some("use plan-1".to_owned())))
        );
        assert_eq!(
            parse_command("/reclassify cache is undefined"),
            Some(Command::Reclassify("cache is undefined".to_owned()))
        );
    }

    #[test]
    fn unknown_command_is_not_forwarded() {
        assert_eq!(
            parse_command("/frobnicate"),
            Some(Command::Unknown("/frobnicate".to_owned()))
        );
    }

    #[test]
    fn empty_and_whitespace_input_is_not_a_command() {
        assert_eq!(parse_command(""), None);
        assert_eq!(parse_command("   "), None);
        assert_eq!(parse_command("\t\n"), None);
    }

    #[test]
    fn slash_only_is_an_unknown_command() {
        assert_eq!(parse_command("/"), Some(Command::Unknown("/".to_owned())));
        assert_eq!(
            parse_command("   /   "),
            Some(Command::Unknown("/".to_owned()))
        );
    }

    #[test]
    fn trims_outer_whitespace_and_preserves_inner_whitespace() {
        assert_eq!(parse_command("  /help  "), Some(Command::Help(None)));
        assert_eq!(
            parse_command("/help  topic   detail  "),
            Some(Command::Help(Some("topic   detail".to_owned())))
        );
        assert_eq!(
            parse_command("/plan   build  a parser"),
            Some(Command::Plan("build  a parser".to_owned()))
        );
    }

    #[test]
    fn command_names_are_case_sensitive() {
        assert_eq!(
            parse_command("/HELP"),
            Some(Command::Unknown("/HELP".to_owned()))
        );
        assert_eq!(
            parse_command("/Quit"),
            Some(Command::Unknown("/Quit".to_owned()))
        );
    }

    #[test]
    fn tab_separates_name_from_arguments() {
        assert_eq!(
            parse_command("/plan\tbuild a parser"),
            Some(Command::Plan("build a parser".to_owned()))
        );
        assert_eq!(
            parse_command("/session\tnew"),
            Some(Command::Session(SessionCommand::New))
        );
    }

    #[test]
    fn optional_argument_commands_pass_rest_or_omit_it() {
        for (input, expected) in [
            ("/help", Command::Help(None)),
            ("/help commands", Command::Help(Some("commands".to_owned()))),
            ("/model", Command::Model(None)),
            (
                "/model deepseek-v4-pro",
                Command::Model(Some("deepseek-v4-pro".to_owned())),
            ),
            ("/context", Command::Context(None)),
            (
                "/context reload GLOSSARY.md",
                Command::Context(Some("reload GLOSSARY.md".to_owned())),
            ),
            ("/skills", Command::Skills(None)),
            (
                "/skills install ./demo",
                Command::Skills(Some("install ./demo".to_owned())),
            ),
            ("/mcp", Command::Mcp(None)),
            (
                "/mcp add demo python3 server.py",
                Command::Mcp(Some("add demo python3 server.py".to_owned())),
            ),
        ] {
            assert_eq!(parse_command(input), Some(expected), "input {input}");
        }
    }

    #[test]
    fn argument_less_commands_ignore_rest() {
        for input in [
            "/cache now",
            "/login",
            "/login key",
            "/logout",
            "/scaffold all",
            "/continue now",
            "/test now",
            "/fix now",
            "/execute now",
            "/quit please",
            "/exit",
        ] {
            assert_eq!(
                parse_command(input),
                Some(expected_argument_less(input)),
                "input {input}"
            );
        }
    }

    fn expected_argument_less(input: &str) -> Command {
        match input.split_whitespace().next().unwrap_or_default() {
            "/cache" => Command::Cache,
            "/login" => Command::Login,
            "/logout" => Command::Logout,
            "/scaffold" => Command::Scaffold,
            "/continue" => Command::Continue,
            "/test" => Command::Test,
            "/fix" => Command::Fix,
            "/execute" => Command::Execute,
            "/quit" | "/exit" => Command::Quit,
            other => panic!("test bug: {other} is not argument-less"),
        }
    }

    #[test]
    fn text_commands_keep_rest_verbatim_including_quotes_and_escapes() {
        for (input, expected) in [
            (
                "/plan build a parser",
                Command::Plan("build a parser".to_owned()),
            ),
            ("/plan", Command::Plan(String::new())),
            (
                "/plan say \"hi there\"",
                Command::Plan("say \"hi there\"".to_owned()),
            ),
            ("/plan a\\ b", Command::Plan("a\\ b".to_owned())),
            (
                "/answer use option B",
                Command::Answer("use option B".to_owned()),
            ),
            ("/answer", Command::Answer(String::new())),
            (
                "/distill I used a trait",
                Command::Distill("I used a trait".to_owned()),
            ),
            (
                "/distill explain \"lifetimes\"",
                Command::Distill("explain \"lifetimes\"".to_owned()),
            ),
            ("/distill", Command::Distill(String::new())),
            (
                "/hint where should I start?",
                Command::Hint("where should I start?".to_owned()),
            ),
            ("/hint", Command::Hint(String::new())),
            (
                "/ask which phase is this?",
                Command::Ask("which phase is this?".to_owned()),
            ),
            ("/ask", Command::Ask(String::new())),
        ] {
            assert_eq!(parse_command(input), Some(expected), "input {input}");
        }
    }

    #[test]
    fn parses_session_verb_and_argument_edge_cases() {
        for (input, expected) in [
            ("/session list", SessionCommand::List),
            ("/session new", SessionCommand::New),
            ("/session new extra words", SessionCommand::New),
            (
                "/session rename my session",
                SessionCommand::Rename("my session".to_owned()),
            ),
            ("/session rename", SessionCommand::Rename(String::new())),
            ("/session switch", SessionCommand::Switch(String::new())),
            (
                "/session switch session-abc extra",
                SessionCommand::Switch("session-abc extra".to_owned()),
            ),
            (
                "/session frobnicate",
                SessionCommand::Switch("frobnicate".to_owned()),
            ),
            (
                "/session frobnicate ignored",
                SessionCommand::Switch("frobnicate".to_owned()),
            ),
        ] {
            assert_eq!(
                parse_command(input),
                Some(Command::Session(expected)),
                "input {input}"
            );
        }
    }

    #[test]
    fn parses_role_edge_cases() {
        for (input, expected) in [
            ("/role", Command::Role(None)),
            ("/role navigator", Command::Role(Some(RoleName::Navigator))),
            ("/role driver", Command::Role(Some(RoleName::Driver))),
            ("/role DRIVER", Command::Role(Some(RoleName::Driver))),
            (
                "/role  navigator  ",
                Command::Role(Some(RoleName::Navigator)),
            ),
            (
                "/role bogus",
                Command::Unknown("unknown role: bogus".to_owned()),
            ),
            (
                "/role navigator extra",
                Command::Unknown("unknown role: navigator extra".to_owned()),
            ),
        ] {
            assert_eq!(parse_command(input), Some(expected), "input {input}");
        }
    }

    #[test]
    fn unknown_command_reports_only_the_name() {
        assert_eq!(
            parse_command("/frobnicate arg one"),
            Some(Command::Unknown("/frobnicate".to_owned()))
        );
        assert_eq!(
            parse_command("/frobnicate\targ"),
            Some(Command::Unknown("/frobnicate".to_owned()))
        );
    }

    #[test]
    fn command_table_matches_help_specs() {
        for (name, _) in COMMAND_TABLE {
            if *name == "/exit" {
                continue;
            }
            assert!(
                COMMAND_SPECS.iter().any(|spec| spec.name == *name),
                "{name} is dispatchable but missing from COMMAND_SPECS"
            );
        }
        for spec in COMMAND_SPECS {
            assert!(
                COMMAND_TABLE.iter().any(|(name, _)| *name == spec.name),
                "{} is documented but not dispatchable",
                spec.name
            );
        }
    }

    #[test]
    fn help_specs_cover_high_frequency_commands() {
        for command in [
            "/help", "/model", "/session", "/role", "/context", "/cache", "/login", "/logout",
            "/skills", "/mcp", "/quit",
        ] {
            assert!(
                COMMAND_SPECS.iter().any(|spec| spec.name == command),
                "missing {command}"
            );
        }
    }

    #[test]
    fn every_help_spec_parses_to_a_known_command() {
        for spec in COMMAND_SPECS {
            let parsed = parse_command(spec.name);
            assert!(parsed.is_some(), "{} did not parse", spec.name);
            assert!(
                !matches!(parsed, Some(Command::Unknown(_))),
                "{} parsed as unknown",
                spec.name
            );
        }
    }

    #[test]
    fn parses_extension_subcommands() {
        assert_eq!(
            parse_command("/skills install ./demo"),
            Some(Command::Skills(Some("install ./demo".to_owned())))
        );
        assert_eq!(
            parse_command("/mcp add demo python3 server.py"),
            Some(Command::Mcp(Some("add demo python3 server.py".to_owned())))
        );
        assert_eq!(
            parse_command("/context reload GLOSSARY.md"),
            Some(Command::Context(Some("reload GLOSSARY.md".to_owned())))
        );
    }

    #[test]
    fn completes_command_names_from_slash_prefix() {
        assert_eq!(
            complete("/ro", 3),
            Some(CommandCompletion {
                suffix: "le".to_owned(),
                full: "/role".to_owned(),
                description: Some("show or switch the active role".to_owned()),
            })
        );
        assert_eq!(complete("/role", 5), None);
        assert_eq!(complete("/z", 2), None);
        assert_eq!(complete("hello", 5), None);
        assert_eq!(complete("/ro", 1), None, "only complete at the cursor end");
    }

    #[test]
    fn completes_command_arguments() {
        assert_eq!(
            complete("/role dr", 8),
            Some(CommandCompletion {
                suffix: "iver".to_owned(),
                full: "/role driver".to_owned(),
                description: None,
            })
        );
        assert_eq!(
            complete("/session sw", 11),
            Some(CommandCompletion {
                suffix: "itch".to_owned(),
                full: "/session switch".to_owned(),
                description: None,
            })
        );
        assert_eq!(
            complete("/model deepseek-v4-p", 20),
            Some(CommandCompletion {
                suffix: "ro".to_owned(),
                full: "/model deepseek-v4-pro".to_owned(),
                description: None,
            })
        );
        assert_eq!(complete("/plan build", 11), None);
    }

    #[test]
    fn completes_feedback_flags_with_descriptions() {
        let completion = complete("/feedback ", 10).expect("feedback flag completion");
        assert_eq!(completion.suffix, "--plan");
        assert_eq!(completion.full, "/feedback --plan");
        assert_eq!(
            completion.description.as_deref(),
            Some("attach a plan artifact")
        );

        let completion = complete("/feedback --n", 13).expect("note flag completion");
        assert_eq!(completion.suffix, "ote");
        assert_eq!(completion.full, "/feedback --note");
        assert_eq!(
            completion.description.as_deref(),
            Some("same as typing what actually happened")
        );

        assert_eq!(complete("/feedback the plan", 18), None);
    }

    #[test]
    fn completes_open_question_ids_for_answer() {
        use crate::workflow::{Question, QuestionKind, QuestionStatus};

        let question = Question {
            question_id: "q-2".to_owned(),
            task_id: None,
            kind: QuestionKind::SemiKnown,
            prompt: "选 A 还是 B？".to_owned(),
            options: vec!["A".to_owned(), "B".to_owned()],
            recommendation: None,
            reason: String::new(),
            status: QuestionStatus::Open,
            criteria: String::new(),
            created_at: 1,
            answer: None,
            answer_source: None,
            answer_snapshot: None,
            attempts: 0,
            last_gap: None,
        };
        let completion = complete_answer("/answer ", 8, &[question]).expect("question completion");
        assert_eq!(completion.suffix, "#q-2");
        assert_eq!(completion.full, "/answer #q-2");
        assert_eq!(completion.description.as_deref(), Some("选 A 还是 B？"));
        assert_eq!(complete_answer("/answer #q", 10, &[]), None);
    }
}
