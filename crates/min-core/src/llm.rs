use std::any::Any;
use std::cell::RefCell;
use std::rc::Rc;

pub trait ChatProvider: Any {
    fn can_handle(&self, model: &str) -> bool;
    
    fn complete(
        &self,
        model: &str,
        system: &str,
        messages_json: &str,
        tools_json: &str,
        on_delta: &mut dyn FnMut(&str),
        on_thinking: &mut dyn FnMut(&str, &str),
        on_status: &mut dyn FnMut(&str),
    ) -> Result<String, String>;
}

#[derive(Default, Clone)]
pub struct LlmRouter {
    providers: Rc<RefCell<Vec<Box<dyn ChatProvider>>>>,
}

impl LlmRouter {
    pub fn register(&self, provider: Box<dyn ChatProvider>) {
        self.providers.borrow_mut().push(provider);
    }
    
    pub fn complete(
        &self,
        model: &str,
        system: &str,
        messages_json: &str,
        tools_json: &str,
        on_delta: &mut dyn FnMut(&str),
        on_thinking: &mut dyn FnMut(&str, &str),
        on_status: &mut dyn FnMut(&str),
    ) -> Result<String, String> {
        let providers = self.providers.borrow();
        for provider in providers.iter() {
            if provider.can_handle(model) {
                return provider.complete(
                    model, system, messages_json, tools_json, on_delta, on_thinking, on_status
                );
            }
        }
        Err(format!("no LLM provider configured for model `{}`", model))
    }
}
