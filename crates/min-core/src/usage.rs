//! Token accounting — the vocabulary plugins share when they talk about cost.
//!
//! min talks to five different endpoints, and each one counts tokens its
//! own way: Anthropic reports cache reads and cache writes *apart* from the
//! prompt tokens, OpenAI folds everything into `prompt_tokens` and (only if
//! the request asked for it) sends the count in a trailing chunk, Ollama
//! reports a prompt count and an eval count, and Vertex may report nothing
//! at all. This module is where those become one shape, so a plugin that
//! wants to know how big a conversation is never has to know which endpoint
//! answered.
//!
//! Two types live here, and both are pure data:
//!
//! - [`TokenUsage`] — what one request cost, in tokens.
//! - [`UsageLedger`] — the running total over a session, published as a
//!   service by whichever plugin owns it.
//!
//! The ledger *counts*; it does not *decide*. Deciding what to do with the
//! number — compact the conversation, warn about a context window, bill
//! someone — is another plugin's job, and that plugin can read the ledger
//! through the service without the accounting plugin knowing it exists.

use std::cell::Cell;
use std::rc::Rc;

/// Normalized token usage for one LLM request.
///
/// The four fields are the union of what the supported endpoints report.
/// A field is 0 when the endpoint did not report it — use
/// [`TokenUsage::is_empty`] to tell "no tokens" apart from "no report".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TokenUsage {
    /// Prompt tokens the provider did *not* serve from its cache.
    pub input_tokens: u64,
    /// Tokens the model generated.
    pub output_tokens: u64,
    /// Prompt tokens served from the provider's cache.
    pub cache_read_tokens: u64,
    /// Prompt tokens written into the provider's cache.
    pub cache_write_tokens: u64,
}

impl TokenUsage {
    /// Every token the request touched.
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_write_tokens
    }

    /// The prompt as the provider saw it — the number a context window is
    /// measured against. Anthropic reports cached tokens *apart* from
    /// `input_tokens`, so they are added back here; OpenAI's `prompt_tokens`
    /// already includes its cached ones, so the dialects agree on this
    /// number once they are normalized.
    pub fn prompt_tokens(&self) -> u64 {
        self.input_tokens + self.cache_read_tokens + self.cache_write_tokens
    }

    /// True when nothing at all was reported — the endpoint did not send
    /// usage, as opposed to reporting zero tokens.
    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }
}

/// A plain copy of a ledger's state — cheap to read, cheap to pass around,
/// and it borrows nothing, so a consumer can hold it while printing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsageSnapshot {
    /// How many requests were recorded.
    pub requests: u64,
    /// The summed usage over all recorded requests.
    pub total: TokenUsage,
    /// Prompt tokens of the most recent request that reported usage — the
    /// context as the provider last saw it.
    pub last_prompt_tokens: u64,
    /// Whether any request reported usage at all. False means the endpoint
    /// does not report usage, not that the session was free.
    pub reported: bool,
}

/// The counters behind a [`UsageLedger`]. Shared by every clone of the
/// ledger, so the plugin that records and the plugins that read all see
/// the same numbers.
#[derive(Debug, Default)]
struct LedgerState {
    requests: Cell<u64>,
    total: Cell<TokenUsage>,
    /// Prompt tokens of the most recent request that reported usage.
    last_prompt: Cell<u64>,
    /// Whether any request reported usage at all.
    reported: Cell<bool>,
}

/// The running token total for a session.
///
/// Published as a service so that any plugin can ask "how much has this
/// session used so far" without keeping a tally of its own. The counters
/// are `Cell`s, so the ledger can be recorded into through the shared
/// reference the hub hands out (`hub.service::<UsageLedger>()`), and read
/// without one (`snapshot`).
///
/// Cloning a ledger shares its counters — the clone is a second handle on
/// the same numbers, not a copy. That is what lets the owning plugin keep
/// one handle to record into and publish another.
#[derive(Debug, Default, Clone)]
pub struct UsageLedger {
    state: Rc<LedgerState>,
}

impl UsageLedger {
    /// Add one request's usage to the ledger.
    ///
    /// A request that reported nothing still counts as a request, but does
    /// not move the totals and does not overwrite the last known context
    /// size — an endpoint that reports usage only sometimes must not drag
    /// the numbers back to zero.
    pub fn record(&self, usage: TokenUsage) {
        let s = &self.state;
        s.requests.set(s.requests.get() + 1);
        if usage.is_empty() {
            return;
        }
        s.reported.set(true);
        s.last_prompt.set(usage.prompt_tokens());
        let mut total = s.total.get();
        total.input_tokens += usage.input_tokens;
        total.output_tokens += usage.output_tokens;
        total.cache_read_tokens += usage.cache_read_tokens;
        total.cache_write_tokens += usage.cache_write_tokens;
        s.total.set(total);
    }

    /// A plain copy of the ledger's state.
    pub fn snapshot(&self) -> UsageSnapshot {
        let s = &self.state;
        UsageSnapshot {
            requests: s.requests.get(),
            total: s.total.get(),
            last_prompt_tokens: s.last_prompt.get(),
            reported: s.reported.get(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_tokens_adds_cache_tokens_back_in() {
        // Anthropic reports cache tokens apart from the prompt tokens.
        let u = TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 100,
            cache_write_tokens: 20,
        };
        assert_eq!(u.prompt_tokens(), 130);
        assert_eq!(u.total(), 135);
        assert!(!u.is_empty());
    }

    #[test]
    fn empty_usage_means_not_reported() {
        assert!(TokenUsage::default().is_empty());
        assert_eq!(TokenUsage::default().total(), 0);
    }

    #[test]
    fn ledger_accumulates_across_requests() {
        let ledger = UsageLedger::default();
        assert_eq!(ledger.snapshot().requests, 0);
        assert!(!ledger.snapshot().reported);

        ledger.record(TokenUsage {
            input_tokens: 10,
            output_tokens: 2,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        });
        ledger.record(TokenUsage {
            input_tokens: 20,
            output_tokens: 3,
            cache_read_tokens: 5,
            cache_write_tokens: 0,
        });

        let s = ledger.snapshot();
        assert_eq!(s.requests, 2);
        assert_eq!(s.total.input_tokens, 30);
        assert_eq!(s.total.output_tokens, 5);
        assert_eq!(s.total.cache_read_tokens, 5);
        assert_eq!(s.total.total(), 40);
        // The context as the provider last saw it: 20 + 5 cached.
        assert_eq!(s.last_prompt_tokens, 25);
        assert!(s.reported);
    }

    #[test]
    fn a_request_without_usage_counts_but_moves_no_numbers() {
        let ledger = UsageLedger::default();
        ledger.record(TokenUsage {
            input_tokens: 10,
            output_tokens: 2,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        });
        // The endpoint stopped reporting: the request counts, the totals
        // stay, and the known context size is not dragged back to zero.
        ledger.record(TokenUsage::default());

        let s = ledger.snapshot();
        assert_eq!(s.requests, 2);
        assert_eq!(s.total.input_tokens, 10);
        assert_eq!(s.last_prompt_tokens, 10);
        assert!(s.reported, "an earlier report still counts as reported");
    }

    #[test]
    fn clones_share_the_same_counters() {
        let ledger = UsageLedger::default();
        let clone = ledger.clone();
        ledger.record(TokenUsage {
            input_tokens: 7,
            output_tokens: 1,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        });
        // The clone is a second handle on the same numbers, not a copy.
        assert_eq!(clone.snapshot().total.input_tokens, 7);
        assert_eq!(clone.snapshot().requests, 1);
    }
}
