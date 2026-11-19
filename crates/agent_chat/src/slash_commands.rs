use agent_harness::SlashCommand;
use i18n::{SharedString, t};
use std::ops::Range;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandToken {
    pub range: Range<usize>,
    pub query: String,
}

fn is_name_char(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '-' | '_' | ':' | '.')
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TokenKind {
    Command,
    Mention,
    Skill,
}

impl TokenKind {
    fn prefix(self) -> char {
        match self {
            TokenKind::Command => '/',
            TokenKind::Mention => '@',
            TokenKind::Skill => '$',
        }
    }

    fn accepts(self, character: char) -> bool {
        match self {
            TokenKind::Mention => !character.is_whitespace() && !matches!(character, '@' | '`'),
            TokenKind::Command | TokenKind::Skill => is_name_char(character),
        }
    }
}

#[cfg(test)]
pub(crate) fn command_token(text: &str, cursor: usize) -> Option<CommandToken> {
    token_of_kind(text, cursor, TokenKind::Command)
}

pub(crate) fn completion_token(text: &str, cursor: usize) -> Option<(TokenKind, CommandToken)> {
    [TokenKind::Command, TokenKind::Mention, TokenKind::Skill]
        .into_iter()
        .find_map(|kind| token_of_kind(text, cursor, kind).map(|token| (kind, token)))
}

fn token_of_kind(text: &str, cursor: usize, kind: TokenKind) -> Option<CommandToken> {
    if cursor > text.len() || !text.is_char_boundary(cursor) {
        return None;
    }
    let start = text[..cursor]
        .char_indices()
        .rev()
        .find(|(_, character)| {
            character.is_whitespace() || matches!(character, '(' | '[' | '{' | '>')
        })
        .map_or(0, |(index, character)| index + character.len_utf8());
    let query = text[start..cursor].strip_prefix(kind.prefix())?;
    if !query.chars().all(|character| kind.accepts(character))
        || (kind == TokenKind::Skill && query.starts_with(|character: char| character.is_numeric()))
    {
        return None;
    }
    let end = text[start + 1..]
        .char_indices()
        .find(|(_, character)| !kind.accepts(*character))
        .map_or(text.len(), |(offset, _)| start + 1 + offset);
    // A slash right after the name means a path like /usr/bin.
    if kind == TokenKind::Command && text[end..].starts_with('/') {
        return None;
    }
    Some(CommandToken {
        range: start..end,
        query: query.to_string(),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AppCommand {
    Model,
    New,
    Resume,
    Settings,
    Diff,
    Files,
    Terminal,
    Stop,
}

impl AppCommand {
    const ALL: [AppCommand; 8] = [
        AppCommand::Model,
        AppCommand::New,
        AppCommand::Resume,
        AppCommand::Settings,
        AppCommand::Diff,
        AppCommand::Files,
        AppCommand::Terminal,
        AppCommand::Stop,
    ];

    fn name(self) -> &'static str {
        match self {
            AppCommand::Model => "model",
            AppCommand::New => "new",
            AppCommand::Resume => "resume",
            AppCommand::Settings => "settings",
            AppCommand::Diff => "diff",
            AppCommand::Files => "files",
            AppCommand::Terminal => "terminal",
            AppCommand::Stop => "stop",
        }
    }

    fn description(self) -> SharedString {
        match self {
            AppCommand::Model => t!("IDE: choose model and reasoning"),
            AppCommand::New => t!("IDE: start a new chat"),
            AppCommand::Resume => t!("IDE: open chat history"),
            AppCommand::Settings => t!("IDE: open settings"),
            AppCommand::Diff => t!("IDE: open changes"),
            AppCommand::Files => t!("IDE: open project files"),
            AppCommand::Terminal => t!("IDE: open a terminal"),
            AppCommand::Stop => t!("IDE: stop the agent"),
        }
    }

    /// Dispatched by name so this crate needn't depend on every panel crate.
    pub fn action_name(self) -> Option<&'static str> {
        match self {
            AppCommand::Settings => Some("wu::OpenSettings"),
            AppCommand::Diff => Some("git_panel::ToggleFocus"),
            AppCommand::Files => Some("project_panel::ToggleFocus"),
            AppCommand::Terminal => Some("terminal_panel::ToggleFocus"),
            AppCommand::Model | AppCommand::New | AppCommand::Resume | AppCommand::Stop => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ExtensionSlashCommand {
    pub extension_id: Arc<str>,
    pub name: Arc<str>,
    pub description: String,
    pub requires_argument: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CommandTarget {
    Agent,
    App(AppCommand),
    Extension {
        extension_id: Arc<str>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CommandItem {
    pub name: String,
    pub detail: String,
    pub target: CommandTarget,
    pub is_skill: bool,
}

pub(crate) fn command_items(
    agent_commands: &[SlashCommand],
    extension_commands: &[ExtensionSlashCommand],
) -> Vec<CommandItem> {
    let mut items: Vec<CommandItem> = agent_commands
        .iter()
        .map(|command| CommandItem {
            name: command.name.clone(),
            detail: match (command.description.is_empty(), &command.input_hint) {
                (_, None) => command.description.clone(),
                (true, Some(hint)) => format!("<{hint}>"),
                (false, Some(hint)) => format!("{} · <{hint}>", command.description),
            },
            target: CommandTarget::Agent,
            is_skill: command.is_skill,
        })
        .collect();
    for command in extension_commands {
        // Harness-provided commands run remotely and win over extension
        // commands registered under the same name.
        if items.iter().any(|item| item.name == command.name.as_ref()) {
            continue;
        }
        items.push(CommandItem {
            name: command.name.to_string(),
            detail: command.description.clone(),
            target: CommandTarget::Extension {
                extension_id: command.extension_id.clone(),
            },
            is_skill: false,
        });
    }
    for command in AppCommand::ALL {
        let mut name = command.name().to_string();
        while items.iter().any(|item| item.name == name) {
            name = format!("wu:{name}");
        }
        items.push(CommandItem {
            name,
            detail: command.description().to_string(),
            target: CommandTarget::App(command),
            is_skill: false,
        });
    }
    items
}

pub(crate) fn filter_commands(query: &str, items: Vec<CommandItem>) -> Vec<CommandItem> {
    let query = query.to_lowercase();
    let mut ranked: Vec<(usize, usize, CommandItem)> = items
        .into_iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let name = item.name.to_lowercase();
            let rank = if name.starts_with(&query) {
                0
            } else if name.contains(&query) {
                1
            } else {
                return None;
            };
            Some((rank, index, item))
        })
        .collect();
    ranked.sort_by_key(|(rank, index, _)| (*rank, *index));
    ranked.into_iter().map(|(_, _, item)| item).collect()
}

/// An extension slash command found in a message, ready to be run.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ExtensionCommandCall {
    pub extension_id: Arc<str>,
    pub name: Arc<str>,
    pub arguments: Vec<String>,
    /// The range in the message text that the command and its arguments
    /// occupy, from the leading `/` to the end of the line.
    pub range: Range<usize>,
}

fn is_command_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':' | b'.')
}

/// Finds extension slash commands in a message.
///
/// A command is recognized when the first non-whitespace token of a line is
/// `/<name>` matching one of `commands`. The arguments are the remaining
/// whitespace-separated words on that line.
pub(crate) fn extension_command_calls(
    text: &str,
    commands: &[ExtensionSlashCommand],
) -> Vec<ExtensionCommandCall> {
    let mut calls = Vec::new();
    let mut line_start = 0;
    for line in text.split('\n') {
        let indent = line.len() - line.trim_start().len();
        let trimmed = &line[indent..];
        let name = trimmed
            .strip_prefix('/')
            .map(|rest| {
                let name_len = rest
                    .bytes()
                    .take_while(|byte| is_command_name_byte(*byte))
                    .count();
                &rest[..name_len]
            })
            .filter(|name| !name.is_empty());
        if let Some(name) = name {
            // A slash right after the name means a path like /usr/bin, not a command.
            let not_a_path = trimmed[1 + name.len()..]
                .strip_prefix('/')
                .is_none();
            if let Some(command) = commands
                .iter()
                .find(|command| command.name.as_ref() == name)
                && not_a_path
            {
                let arguments = trimmed[1 + name.len()..]
                    .split_whitespace()
                    .map(str::to_string)
                    .collect();
                calls.push(ExtensionCommandCall {
                    extension_id: command.extension_id.clone(),
                    name: command.name.clone(),
                    arguments,
                    range: line_start + indent..line_start + line.len(),
                });
            }
        }
        line_start += line.len() + 1;
    }
    calls
}

/// Replaces the text occupied by a command call with the command's output.
pub(crate) fn replace_extension_command_call(
    text: &str,
    call: &ExtensionCommandCall,
    output: &str,
) -> String {
    let start = call.range.start.min(text.len());
    let end = call.range.end.max(start).min(text.len());
    let mut result = String::with_capacity(text.len());
    result.push_str(&text[..start]);
    result.push_str(output);
    result.push_str(&text[end..]);
    result
}

pub(crate) fn app_command_for_text(text: &str, items: &[CommandItem]) -> Option<AppCommand> {
    let name = text.trim().strip_prefix('/')?;
    items.iter().find_map(|item| match item.target {
        CommandTarget::App(command) if item.name == name => Some(command),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(text: &str) -> Option<(Range<usize>, String)> {
        command_token(text, text.len()).map(|token| (token.range, token.query))
    }

    #[test]
    fn tokens_start_at_word_boundaries() {
        assert_eq!(token("/"), Some((0..1, String::new())));
        assert_eq!(token("/rev"), Some((0..4, "rev".into())));
        assert_eq!(token("please /rev"), Some((7..11, "rev".into())));
        assert_eq!(token("(/rev"), Some((1..5, "rev".into())));
        assert_eq!(token("a/rev"), None);
        assert_eq!(token("/rev iew"), None);
        assert_eq!(token("/usr/bin"), None);
        assert_eq!(token("/a+b"), None);
    }

    #[test]
    fn mentions_and_skills_have_their_own_rules() {
        let kind = |text: &str| completion_token(text, text.len()).map(|(kind, token)| (kind, token.query));
        assert_eq!(kind("see @src/ma"), Some((TokenKind::Mention, "src/ma".into())));
        assert_eq!(kind("mail me@example.com"), None);
        assert_eq!(kind("use $review"), Some((TokenKind::Skill, "review".into())));
        assert_eq!(kind("costs $5"), None);
        assert_eq!(kind("/comp"), Some((TokenKind::Command, "comp".into())));
    }

    #[test]
    fn token_covers_the_whole_name_around_the_cursor() {
        let text = "/review now";
        assert_eq!(
            command_token(text, 3),
            Some(CommandToken {
                range: 0..7,
                query: "re".into()
            })
        );
        assert_eq!(command_token("/usr/bin", 4), None);
        assert_eq!(command_token("é/x", 1), None);
    }

    fn agent(name: &str) -> SlashCommand {
        SlashCommand {
            name: name.into(),
            description: String::new(),
            input_hint: None,
            is_skill: false,
        }
    }

    fn extension(name: &str) -> ExtensionSlashCommand {
        ExtensionSlashCommand {
            extension_id: "example-extension".into(),
            name: name.into(),
            description: "Runs in the example extension".into(),
            requires_argument: false,
        }
    }

    #[test]
    fn wu_commands_step_aside_for_agent_commands() {
        let items = command_items(&[agent("review"), agent("model")], &[]);
        let names: Vec<&str> = items.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(&names[..3], ["review", "model", "wu:model"]);
        assert_eq!(
            app_command_for_text(" /wu:model ", &items),
            Some(AppCommand::Model)
        );
        assert_eq!(app_command_for_text("/model", &items), None);
        assert_eq!(app_command_for_text("/new", &items), Some(AppCommand::New));
        assert_eq!(app_command_for_text("/new chat", &items), None);
    }

    #[test]
    fn details_show_argument_hints() {
        let command = SlashCommand {
            name: "review".into(),
            description: "Review a pull request".into(),
            input_hint: Some("[pr number]".into()),
            is_skill: false,
        };
        assert_eq!(
            command_items(&[command], &[])[0].detail,
            "Review a pull request · <[pr number]>"
        );
    }

    #[test]
    fn prefix_matches_come_first() {
        let items = command_items(
            &[agent("compact"), agent("pr-comments"), agent("compress")],
            &[],
        );
        let names: Vec<String> = filter_commands("COM", items)
            .into_iter()
            .map(|item| item.name)
            .collect();
        assert_eq!(names, ["compact", "compress", "pr-comments"]);
    }

    #[test]
    fn extension_commands_merge_with_the_others() {
        let items = command_items(&[agent("model")], &[extension("flash"), extension("model")]);
        let names: Vec<&str> = items.iter().map(|item| item.name.as_str()).collect();
        // The agent's `model` wins over the extension's, `flash` joins the
        // list and the app command steps aside for both.
        assert_eq!(&names[..3], &["model", "flash", "wu:model"]);
        assert!(matches!(items[1].target, CommandTarget::Extension { .. }));
    }

    #[test]
    fn extension_commands_are_found_at_line_starts() {
        let commands = [extension("flash"), extension("registers")];
        let calls = extension_command_calls(
            "
  /flash  --target f407 
plain text
/registers arm m4
/usr/bin/not-a-command",
            &commands,
        );
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name.as_ref(), "flash");
        assert_eq!(calls[0].arguments, ["--target", "f407"]);
        assert_eq!(calls[0].range, 3..25);
        assert_eq!(calls[1].name.as_ref(), "registers");
        assert_eq!(calls[1].arguments, ["arm", "m4"]);

        let expanded = replace_extension_command_call(
            "
  /flash  --target f407 
plain text",
            &calls[0],
            "Flashed to 0x08000000",
        );
        assert_eq!(expanded, "
  Flashed to 0x08000000
plain text");
    }
}
