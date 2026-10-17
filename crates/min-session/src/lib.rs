//! min-session — a unique identifier for the work min is currently doing.
//!
//! A session is nothing but an id: minted at startup, announced to every
//! plugin, and replaced whenever the conversation is cleared. Sessions have
//! an optional parent (the session that, say, asked another to do some
//! work), supplied on the command line.
//!
//! ## Lifecycle
//!
//! - At startup the first session is announced from this plugin's **first
//!   `wait`** — the core only calls `wait` after *every* plugin has had its
//!   `start`, so the announcement reaches all plugins, including this one.
//!   (Anything emitted from `start` would be missed by every plugin loaded
//!   later — events are never queued or replayed.)
//! - On [`events::ResetConversation`] (`/clear`) the current session is
//!   closed with [`events::SessionEnd`] and a new one announced with
//!   [`events::SessionStart`], in that order, inside the one dispatch.
//! - On [`events::Quit`] the session is closed.
//!
//! Ids are epoch seconds, stepped past the previous id when two sessions
//! begin within the same second (a `/clear` inside its first second of
//! life). A plugin that wants to track the current session keeps the last
//! `id` it saw in its own state; there is deliberately no session service —
//! the events *are* the interface.

use min_core::{events, Hub, Plugin};
use std::any::Any;

pub struct SessionPlugin {
    /// The open session, announced and not yet ended.
    current: Option<Session>,
    /// Id pinned for the first session (`--session`); used verbatim, once.
    pinned: Option<u64>,
    /// Parent for the *next* session minted (`--parent-session`).
    parent: Option<u64>,
}

#[derive(Clone, Copy)]
struct Session {
    id: u64,
    parent: Option<u64>,
}

impl SessionPlugin {
    /// `session_id`: pin the first session to this id (both switches are
    /// `Some` → `None`: the caller could not have meant both).
    pub fn new(session_id: Option<u64>, parent_id: Option<u64>) -> Self {
        SessionPlugin {
            current: None,
            pinned: session_id,
            parent: if session_id.is_some() && parent_id.is_some() {
                None
            } else {
                parent_id
            },
        }
    }

    /// Mint the next id: the current epoch second — or the pinned id, for
    /// the first session — but never less than one past the id before it,
    /// so ids are unique even when a session is replaced within its first
    /// second. Call it *before* closing the current session: the guard
    /// compares against the session being replaced.
    fn next_id(&self) -> u64 {
        match self.current {
            None => self.pinned.unwrap_or_else(secs_since_epoch),
            Some(s) => secs_since_epoch().max(s.id + 1),
        }
    }

    /// Open a session and announce it. Callers arrange for the borrow of
    /// `self` to be released before the event dispatches (return it from
    /// `wait`), so the announcement reaches every plugin — this one
    /// included.
    fn open(&mut self, id: u64) -> events::SessionStart {
        let session = Session { id, parent: self.parent.take() };
        self.current = Some(session);
        self.pinned = None; // a pin is for the startup session only
        events::SessionStart { id: session.id, parent: session.parent }
    }

    /// Close the open session, if there is one.
    fn close(&mut self) -> Option<events::SessionEnd> {
        let session = self.current.take()?;
        Some(events::SessionEnd { id: session.id })
    }
}

impl Plugin for SessionPlugin {
    fn name(&self) -> &'static str {
        "session"
    }

    /// Nothing to register — the session exists for its events. The first
    /// announcement is deferred to `wait`, below, deliberately.
    fn start(&mut self, _hub: &Hub) {}

    /// Called by the core whenever the bus is idle. The first time through,
    /// the harness is fully bootstrapped — every plugin has started — so
    /// announcing the session here is what makes the startup broadcast
    /// complete (a plugin loaded after us would miss a `start` emission).
    fn wait(&mut self, _hub: &Hub) -> Option<Box<dyn Any>> {
        if self.current.is_none() {
            let id = self.next_id();
            return Some(Box::new(self.open(id)));
        }
        None
    }

    fn handle(&mut self, hub: &Hub, event: &dyn Any) {
        if event.downcast_ref::<events::ResetConversation>().is_some() {
            // Mint before closing: the uniqueness guard compares against the
            // session being replaced.
            let id = self.next_id();
            if let Some(end) = self.close() {
                hub.emit(end);
            }
            hub.emit(self.open(id));
        } else if event.downcast_ref::<events::Quit>().is_some() {
            if let Some(end) = self.close() {
                hub.emit(end);
            }
        }
    }
}

fn secs_since_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Records session events in order, like the core tests' Recorder.
    struct Recorder(Rc<RefCell<Vec<String>>>);
    impl Plugin for Recorder {
        fn name(&self) -> &'static str {
            "recorder"
        }
        fn handle(&mut self, _hub: &Hub, event: &dyn Any) {
            if let Some(e) = event.downcast_ref::<events::SessionStart>() {
                self.0.borrow_mut().push(format!("start:{}:{}", e.id, e.parent.unwrap_or(0)));
            } else if let Some(e) = event.downcast_ref::<events::SessionEnd>() {
                self.0.borrow_mut().push(format!("end:{}", e.id));
            }
        }
    }

    /// The startup path, as the core runs it: every plugin's `start` has
    /// already happened by the first `wait`; the returned event is then
    /// dispatched to *all* plugins. Proves both that the first announcement
    /// is deferred until after start (nothing seen before wait) and that
    /// everyone — this plugin included — receives it.
    #[test]
    fn first_wait_announces_a_fresh_session_to_everyone() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let hub = Hub::new(vec![Box::new(Recorder(log.clone()))]);
        let mut p = SessionPlugin::new(None, None);
        assert!(log.borrow().is_empty(), "silent through start");

        // Core loop, abridged: wait, then dispatch the event to everyone.
        let event = p.wait(&hub).expect("session announces on first wait");
        let e = event.downcast_ref::<events::SessionStart>().unwrap();
        hub.emit(events::SessionStart { id: e.id, parent: e.parent });

        assert_eq!(*log.borrow(), vec![format!("start:{}:0", secs_since_epoch())]);
        // And afterwards the plugin goes quiet — it does not spin the bus.
        assert!(p.wait(&hub).is_none());
    }

    #[test]
    fn pinned_id_is_used_verbatim_for_the_first_session_only() {
        let mut p = SessionPlugin::new(Some(1_000_000), None);
        assert_eq!(p.next_id(), 1_000_000);
        let _ = p.open(1_000_000);
        // After the first session, ids are minted from the clock again.
        p.current = Some(Session { id: 1_000_000, parent: None });
        assert_eq!(p.next_id(), secs_since_epoch().max(1_000_001));
    }

    #[test]
    fn both_switches_is_an_error_and_parent_is_dropped() {
        let p = SessionPlugin::new(Some(42), Some(7));
        assert_eq!(p.parent, None, "session id and parent together → error; parent cleared");
    }

    #[test]
    fn ids_step_past_the_previous_when_within_the_same_second() {
        let mut p = SessionPlugin::new(None, None);
        p.current = Some(Session { id: secs_since_epoch(), parent: None });
        assert_eq!(p.next_id(), secs_since_epoch() + 1);
        // And once the second has rolled over, the plain epoch second is used.
        p.current = Some(Session { id: secs_since_epoch() - 10, parent: None });
        assert_eq!(p.next_id(), secs_since_epoch());
    }

    #[test]
    fn parent_is_taken_by_the_first_session_only() {
        let mut p = SessionPlugin::new(None, Some(7));
        let first = p.open(100);
        assert_eq!((first.id, first.parent), (100, Some(7)));
        let second = p.open(200);
        assert_eq!((second.id, second.parent), (200, None), "parent is one-shot");
    }

    #[test]
    fn clear_closes_then_opens_quit_closes() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let hub = Hub::new(vec![Box::new(Recorder(log.clone()))]);
        let mut p = SessionPlugin::new(None, None);
        let event = p.wait(&hub).expect("first session announced from wait");
        let e = event.downcast_ref::<events::SessionStart>().unwrap();
        hub.emit(events::SessionStart { id: e.id, parent: e.parent }); // the core's job
        let first = p.current.unwrap().id;

        p.handle(&hub, &events::ResetConversation {});
        let second = p.current.unwrap().id;
        assert!(second > first, "fresh id after /clear, even within one second");

        p.handle(&hub, &events::Quit {});
        assert!(p.current.is_none(), "session closed on Quit");

        assert_eq!(
            *log.borrow(),
            vec![
                format!("start:{first}:0"),
                format!("end:{first}"),
                format!("start:{second}:0"),
                format!("end:{second}"),
            ]
        );
    }
}
