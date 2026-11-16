//! Per-turn prompt-cache accounting derived from the durable session log.

use slowcode_kernel::{
    model::TokenUsage,
    session::{EventKind, Session, TurnId},
};

/// Prompt-cache counters for one turn.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CacheReport {
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
}

impl CacheReport {
    /// Sum every `AssistantMessage` usage recorded for `turn`.
    #[must_use]
    pub fn from_turn(session: &Session, turn: TurnId) -> Self {
        let mut report = Self::default();
        for event in session.events() {
            let EventKind::AssistantMessage {
                turn: event_turn,
                usage: Some(usage),
                ..
            } = event.kind()
            else {
                continue;
            };
            if *event_turn == turn {
                report.add(usage);
            }
        }
        report
    }

    fn add(&mut self, usage: &TokenUsage) {
        self.input_tokens = self.input_tokens.saturating_add(usage.input_tokens().get());
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(usage.cache_read_tokens().map_or(0, |value| value.get()));
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(usage.cache_write_tokens().map_or(0, |value| value.get()));
    }

    /// Uncached input tokens reported by the provider.
    #[must_use]
    pub fn input_tokens(&self) -> u64 {
        self.input_tokens
    }

    /// Tokens served from the provider cache.
    #[must_use]
    pub fn cache_read_tokens(&self) -> u64 {
        self.cache_read_tokens
    }

    /// Tokens written to the provider cache, when reported.
    #[must_use]
    pub fn cache_write_tokens(&self) -> u64 {
        self.cache_write_tokens
    }

    /// Total prompt tokens priced by this turn.
    #[must_use]
    pub fn prompt_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens)
    }

    /// Cache-read share of all prompt tokens, when the provider reported any.
    #[must_use]
    pub fn hit_ratio(&self) -> Option<f64> {
        let total = self.prompt_tokens();
        (total > 0).then(|| self.cache_read_tokens as f64 / total as f64)
    }

    /// One-line terminal report, or `None` when the provider reported nothing.
    #[must_use]
    pub fn render(&self) -> Option<String> {
        let total = self.prompt_tokens();
        if total == 0 {
            return None;
        }
        let ratio = self.hit_ratio().map_or(0.0, |ratio| ratio * 100.0);
        let mut line = format!(
            "[cache: {} read / {} prompt ({ratio:.0}%)",
            self.cache_read_tokens, total
        );
        if self.cache_write_tokens > 0 {
            line.push_str(&format!(", {} written", self.cache_write_tokens));
        }
        line.push(']');
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::CacheReport;

    #[test]
    fn cache_writes_are_part_of_prompt_tokens() {
        let report = CacheReport {
            input_tokens: 10,
            cache_read_tokens: 20,
            cache_write_tokens: 30,
        };
        assert_eq!(report.prompt_tokens(), 60);
        assert_eq!(report.hit_ratio(), Some(1.0 / 3.0));
        let line = report.render().expect("report renders");
        assert!(line.contains("20 read / 60 prompt (33%)"), "{line}");
        assert!(line.contains("30 written"), "{line}");
    }

    #[test]
    fn an_empty_report_does_not_render() {
        assert_eq!(CacheReport::default().render(), None);
    }
}
