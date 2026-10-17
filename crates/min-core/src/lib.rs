//! `min-core` — the entire core of min.
//!
//! The core knows nothing about terminals, LLMs, agents, files or shells.
//! It is exactly four things:
//!
//! 1. a plugin contract ([`Plugin`]) — every feature is one,
//! 2. an event bus ([`Hub::emit`]) — plugins talk only through events,
//! 3. shared state ([`Hub::service`]) — typed, optional, plugin-defined,
//! 4. a tool registry ([`Hub::tools`]) — how plugins extend the agent.
//!
//! Core has zero dependencies and (almost) zero behaviour: it starts plugins,
//! pumps events between them, and lets them block while idle. That's it.
//!
//! ## Event semantics
//!
//! [`Hub::emit`] delivers an event **immediately and synchronously** to every
//! plugin, in load order. A plugin that is itself executing (inside `start`,
//! `wait` or `handle`) does not receive it — events are never queued or
//! replayed. This keeps the core tiny; plugins that must not miss events
//! should emit from `wait` (which runs after all borrows are released).
//!
//! ## Idle semantics
//!
//! When nothing is happening, the core calls [`Plugin::wait`] on each plugin
//! in turn. `wait` may block (the repl blocks on stdin); the first plugin to
//! return `Some(event)` injects that event into the bus, and the cycle starts
//! again. A set of plugins whose `wait` never blocks and never returns an
//! event will spin — don't do that.

#![forbid(unsafe_code)]

use std::any::{Any, TypeId};
use std::cell::{Cell, Ref, RefCell, RefMut};
use std::collections::HashMap;

pub mod events;
pub mod tool;

pub use tool::{Tool, ToolRegistry};

/// A plugin is one feature of min: the UI, an LLM client, a tool, the agent
/// loop — anything. Plugins never import each other; they communicate
/// exclusively through events and services obtained from the [`Hub`].
pub trait Plugin {
    /// A short, stable name (used for introspection).
    fn name(&self) -> &'static str;

    /// Called once, in load order, before the main loop starts.
    /// Register tools and services here.
    fn start(&mut self, _hub: &Hub) {}

    /// Called when the system is idle. May block. Return an event to inject
    /// it into the bus (it is emitted only after `wait` has returned, so all
    /// plugins — including this one — are free to receive it).
    fn wait(&mut self, _hub: &Hub) -> Option<Box<dyn Any>> {
        None
    }

    /// Called for every emitted event. Downcast and ignore what isn't yours.
    fn handle(&mut self, _hub: &Hub, _event: &dyn Any) {}
}

/// The hub: all plugin coordination happens through it.
pub struct Hub {
    plugins: RefCell<Vec<RefCell<Box<dyn Plugin>>>>,
    services: RefCell<HashMap<TypeId, Box<dyn Any>>>,
    tools: RefCell<ToolRegistry>,
    running: Cell<bool>,
}

impl Hub {
    /// Build a hub by hand — public so plugins (and plugin tests) can
    /// construct one without going through [`run`].
    pub fn new(plugins: Vec<Box<dyn Plugin>>) -> Self {
        Hub {
            plugins: RefCell::new(plugins.into_iter().map(RefCell::new).collect()),
            services: RefCell::new(HashMap::new()),
            tools: RefCell::new(ToolRegistry::default()),
            running: Cell::new(true),
        }
    }

    /// Emit an event: delivered immediately, in order, to every plugin that
    /// isn't currently executing. See the crate docs for the full semantics.
    pub fn emit<E: Any>(&self, event: E) {
        self.dispatch(&event);
    }

    fn dispatch(&self, event: &dyn Any) {
        for plugin in self.plugins.borrow().iter() {
            if let Ok(mut p) = plugin.try_borrow_mut() {
                p.handle(self, event);
            }
        }
    }

    fn with<R>(&self, idx: usize, f: impl FnOnce(&mut dyn Plugin, &Hub) -> R) -> R {
        let plugins = self.plugins.borrow();
        let mut p = plugins[idx]
            .try_borrow_mut()
            .expect("plugin re-entered its own entry point");
        f(&mut **p, self)
    }

    /// Publish a service: one value of type `S`, retrievable by any plugin.
    /// A second `set_service::<S>` replaces the first.
    pub fn set_service<S: Any>(&self, service: S) {
        self.services
            .borrow_mut()
            .insert(TypeId::of::<S>(), Box::new(service));
    }

    /// Fetch a previously published service, if any.
    pub fn service<S: Any>(&self) -> Option<Ref<'_, S>> {
        Ref::filter_map(self.services.borrow(), |m| {
            m.get(&TypeId::of::<S>())
                .and_then(|b| b.downcast_ref::<S>())
        })
        .ok()
    }

    /// The tool registry. Register tools in `start`; the agent looks them up
    /// by name when the model calls them.
    pub fn tools(&self) -> RefMut<'_, ToolRegistry> {
        RefMut::map(self.tools.borrow_mut(), |t| t)
    }

    /// Stop the harness after the current event finishes propagating.
    pub fn quit(&self) {
        self.running.set(false);
    }

    pub fn is_running(&self) -> bool {
        self.running.get()
    }
}

/// Start the plugins and run until someone calls [`Hub::quit`].
pub fn run(plugins: Vec<Box<dyn Plugin>>) {
    let n = plugins.len();
    if n == 0 {
        return;
    }
    let hub = Hub::new(plugins);
    for i in 0..n {
        hub.with(i, |p, h| p.start(h));
    }
    while hub.is_running() {
        let mut produced: Option<Box<dyn Any>> = None;
        for i in 0..n {
            if !hub.is_running() {
                break;
            }
            if let Some(e) = hub.with(i, |p, h| p.wait(h)) {
                produced = Some(e);
                break;
            }
        }
        if let Some(e) = produced {
            hub.dispatch(&*e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    struct Recorder {
        name: &'static str,
        log: Rc<RefCell<Vec<String>>>,
    }
    impl Plugin for Recorder {
        fn name(&self) -> &'static str {
            self.name
        }
        fn handle(&mut self, _hub: &Hub, event: &dyn Any) {
            if let Some(e) = event.downcast_ref::<events::UserInput>() {
                self.log.borrow_mut().push(format!("{}:{}", self.name, e.text));
            }
        }
    }

    /// A plugin that echoes every UserInput it sees as a new UserInput.
    /// Its own echo must not come back to it (it is busy) — so the chain
    /// ends and no stack overflow occurs.
    struct Echo;
    impl Plugin for Echo {
        fn name(&self) -> &'static str {
            "echo"
        }
        fn handle(&mut self, hub: &Hub, event: &dyn Any) {
            if let Some(e) = event.downcast_ref::<events::UserInput>() {
                hub.emit(events::UserInput {
                    text: format!("{}!", e.text),
                });
            }
        }
    }

    #[test]
    fn events_reach_other_plugins_not_busy_ones() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let plugins: Vec<Box<dyn Plugin>> = vec![
            Box::new(Recorder { name: "a", log: log.clone() }),
            Box::new(Echo),
        ];
        let hub = Hub::new(plugins);
        hub.emit(events::UserInput { text: "hi".into() });
        // recorder saw the original and the echo; echo never saw its own echo
        assert_eq!(*log.borrow(), vec!["a:hi", "a:hi!"]);
    }

    #[test]
    fn services_roundtrip_and_replace() {
        let hub = Hub::new(vec![]);
        assert!(hub.service::<u32>().is_none());
        hub.set_service(7u32);
        assert_eq!(*hub.service::<u32>().unwrap(), 7);
        hub.set_service(9u32);
        assert_eq!(*hub.service::<u32>().unwrap(), 9);
    }

    struct Hi;
    impl Tool for Hi {
        fn name(&self) -> &'static str {
            "hi"
        }
        fn spec(&self) -> &'static str {
            "{}"
        }
        fn run(&self, input: &str) -> Result<String, String> {
            Ok(format!("hi {input}"))
        }
    }

    #[test]
    fn tools_register_and_run() {
        let hub = Hub::new(vec![]);
        hub.tools().register(Box::new(Hi));
        assert_eq!(hub.tools().run("hi", "you").unwrap(), "hi you");
        assert!(hub.tools().run("nope", "").is_err());
        assert_eq!(hub.tools().specs()[0].0, "hi");
    }
}
