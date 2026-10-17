//! Tools are how plugins extend the agent's capabilities.
//!
//! The core only stores, looks up and runs them — schemas and behaviour
//! belong entirely to the plugin that registers the tool. Input and output
//! are JSON strings so the core stays dependency-free.

/// A callable tool. Register instances with [`crate::Hub::tools`].
pub trait Tool {
    /// The name the model calls the tool by (must be unique; a later
    /// registration with the same name replaces the earlier one).
    fn name(&self) -> &'static str;

    /// Tool spec as a JSON object string:
    /// `{"description": "...", "input_schema": {...}}`.
    /// The registry adds `"name"` when building the request for the model.
    fn spec(&self) -> &'static str;

    /// Run the tool. `input` is the model's JSON arguments as a string.
    /// `Err` marks the result as a tool error for the model.
    fn run(&self, input: &str) -> Result<String, String>;
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.retain(|t| t.name() != tool.name());
        self.tools.push(tool);
    }

    /// `(name, spec)` pairs, in registration order.
    pub fn specs(&self) -> Vec<(&'static str, &'static str)> {
        self.tools.iter().map(|t| (t.name(), t.spec())).collect()
    }

    pub fn run(&self, name: &str, input: &str) -> Result<String, String> {
        match self.tools.iter().find(|t| t.name() == name) {
            Some(t) => t.run(input),
            None => Err(format!("unknown tool `{name}`")),
        }
    }
}
