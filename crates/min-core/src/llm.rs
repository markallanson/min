use std::cell::RefCell;
use std::rc::Rc;

/// The callbacks a completion reports through while it streams.
///
/// Grouped into one struct because they always travel together: every
/// provider takes all three, and a call site that has to name them
/// individually is a call site that can get the order wrong.
pub struct Callbacks<'a> {
    /// Text as it arrives, for the model to read.
    pub on_delta: &'a mut dyn FnMut(&str),
    /// Non-text work: `(reasoning_text, tool_name)`.
    pub on_thinking: &'a mut dyn FnMut(&str, &str),
    /// Human-readable progress — retries, endpoint switches.
    pub on_status: &'a mut dyn FnMut(&str),
}

pub trait ChatProvider {
    fn can_handle(&self, model: &str) -> bool;

    /// The model this provider will actually ask for.
    ///
    /// The provider owns the model: it resolves `MIN_MODEL` against the
    /// endpoint's default. A consumer must not invent a name of its own —
    /// it would report a model that was never used.
    fn model(&self) -> &str;

    fn complete(
        &self,
        model: &str,
        system: &str,
        messages_json: &str,
        tools_json: &str,
        callbacks: Callbacks<'_>,
    ) -> Result<String, String>;
}

#[derive(Default)]
pub struct LlmRouter {
    providers: Rc<RefCell<Vec<Box<dyn ChatProvider>>>>,
}

impl LlmRouter {
    pub fn register(&self, provider: Box<dyn ChatProvider>) {
        self.providers.borrow_mut().push(provider);
    }

    /// The model the routed provider will actually use, if one is
    /// registered. Consumers report *this* — never a name they made up.
    pub fn model(&self) -> Option<String> {
        let providers = self.providers.borrow();
        providers
            .iter()
            .find(|p| p.can_handle(""))
            .map(|p| p.model().to_string())
    }

    pub fn complete(
        &self,
        model: &str,
        system: &str,
        messages_json: &str,
        tools_json: &str,
        callbacks: Callbacks<'_>,
    ) -> Result<String, String> {
        let providers = self.providers.borrow();
        for provider in providers.iter() {
            if provider.can_handle(model) {
                return provider.complete(
                    model, system, messages_json, tools_json, callbacks
                );
            }
        }
        Err(format!("no LLM provider configured for model `{}`", model))
    }
}
