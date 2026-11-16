//! Terminal-safe rendering helpers shared by the CLI and the TUI.

use slowcode_kernel::{agent::TurnOutcome, model::ContentBlockKind};

/// Join the final assistant text blocks from one turn.
#[must_use]
pub fn outcome_text(outcome: &TurnOutcome) -> String {
    let mut text = String::new();
    if let Some(message) = outcome.final_message() {
        for block in message.content() {
            if let ContentBlockKind::Text { text: block_text } = block.kind() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(block_text);
            }
        }
    }
    text
}

/// Escape untrusted text for terminal rendering.
///
/// Newlines and tabs are preserved so multi-line previews keep their shape.
/// Every other control character is replaced with `?` so untrusted tool output
/// cannot move the cursor or rewrite the screen.
#[must_use]
pub fn escape_terminal(text: &str) -> String {
    text.chars()
        .map(|character| match character {
            '\n' | '\t' => character,
            character if character.is_control() => '?',
            character => character,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_printable_text_and_layout_characters() {
        assert_eq!(escape_terminal("a\tb\nc"), "a\tb\nc");
    }

    #[test]
    fn replaces_other_control_characters() {
        assert_eq!(escape_terminal("a\u{1b}[31mb\r"), "a?[31mb?");
    }
}
