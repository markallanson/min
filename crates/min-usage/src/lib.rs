//! min-usage — token accounting for the session.
//!
//! Owns one number: how many tokens this session has used. It listens for
//! [`events::Usage`] — the normalized per-request report the agent emits
//! after every round — and folds each one into a [`UsageLedger`], which it
//! publishes as a service. Any other plugin can then ask "how much has
//! this session used" without keeping a tally of its own:
//!
//! ```ignore
//! if let Some(ledger) = hub.service::<UsageLedger>() {
//!     let snapshot = ledger.snapshot();
//! }
//! ```
//!
//! The plugin *counts*; it does not *decide*. Compacting a conversation
//! when the context grows too large, warning about a context window, or
//! billing someone are all jobs for other plugins — and this plugin does
//! not need to know they exist.

use min_core::usage::UsageLedger;
use min_core::{events, Hub, Plugin};
use std::any::Any;

pub struct UsagePlugin {
    /// The ledger this plugin records into. Cloning a ledger shares its
    /// counters, so the handle published as a service and the handle kept
    /// here are the same numbers.
    ledger: UsageLedger,
}

impl UsagePlugin {
    pub fn new() -> Self {
        UsagePlugin {
            ledger: UsageLedger::default(),
        }
    }
}

impl Default for UsagePlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for UsagePlugin {
    fn name(&self) -> &'static str {
        "usage"
    }

    fn start(&mut self, hub: &Hub) {
        // Publish a second handle on the same counters — not a copy.
        hub.set_service(self.ledger.clone());
    }

    fn handle(&mut self, _hub: &Hub, event: &dyn Any) {
        if let Some(e) = event.downcast_ref::<events::Usage>() {
            self.ledger.record(e.usage);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use min_core::usage::TokenUsage;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A plugin that emits usage events from its `start` and then reads
    /// the ledger back through the service — the way a real consumer
    /// would. It is loaded *after* the usage plugin, so the service is
    /// already published when it looks.
    struct Emitter {
        events: Vec<events::Usage>,
        snapshot: Rc<RefCell<Option<min_core::usage::UsageSnapshot>>>,
    }

    impl Plugin for Emitter {
        fn name(&self) -> &'static str {
            "emitter"
        }
        fn start(&mut self, hub: &Hub) {
            for e in self.events.clone() {
                hub.emit(e);
            }
            // The service must be published, and must reflect what was
            // just recorded.
            *self.snapshot.borrow_mut() = hub.service::<UsageLedger>().map(|l| l.snapshot());
            hub.quit();
        }
    }

    #[test]
    fn usage_events_accumulate_in_the_published_ledger() {
        let snapshot = Rc::new(RefCell::new(None));
        let plugin = UsagePlugin::new();
        let plugins: Vec<Box<dyn Plugin>> = vec![
            Box::new(plugin),
            Box::new(Emitter {
                events: vec![
                    events::Usage {
                        usage: TokenUsage {
                            input_tokens: 100,
                            output_tokens: 20,
                            cache_read_tokens: 0,
                            cache_write_tokens: 0,
                        },
                        model: "test".into(),
                        round: 1,
                    },
                    events::Usage {
                        usage: TokenUsage {
                            input_tokens: 50,
                            output_tokens: 5,
                            cache_read_tokens: 0,
                            cache_write_tokens: 0,
                        },
                        model: "test".into(),
                        round: 2,
                    },
                ],
                snapshot: snapshot.clone(),
            }),
        ];
        min_core::run(plugins);

        let s = snapshot.borrow().expect("the ledger was readable as a service");
        assert_eq!(s.requests, 2);
        assert_eq!(s.total.input_tokens, 150);
        assert_eq!(s.total.output_tokens, 25);
        assert_eq!(s.last_prompt_tokens, 50);
        assert!(s.reported);
    }

    #[test]
    fn a_request_without_usage_is_counted_but_adds_no_tokens() {
        let snapshot = Rc::new(RefCell::new(None));
        let plugins: Vec<Box<dyn Plugin>> = vec![
            Box::new(UsagePlugin::new()),
            Box::new(Emitter {
                events: vec![events::Usage {
                    usage: TokenUsage::default(),
                    model: "test".into(),
                    round: 1,
                }],
                snapshot: snapshot.clone(),
            }),
        ];
        min_core::run(plugins);

        let s = snapshot.borrow().expect("the ledger was readable as a service");
        assert_eq!(s.requests, 1);
        assert!(!s.reported, "an endpoint that reports nothing is not free");
        assert_eq!(s.total.total(), 0);
    }
}
