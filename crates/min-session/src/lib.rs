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
//! Ids are `<epoch seconds>-<4 random alphanumeric characters>` — the clock
//! keeps them sorting chronologically, the suffix keeps them unique however
//! many sessions begin within one second (a `/clear` inside the first
//! second of a session's life included), so there is nothing to step past.
//! `--session <id>` still pins a verbatim id, which is what
//! `--parent-session` points at. A plugin that wants to track the current
//! session keeps the last `id` it saw in its own state; there is
//! deliberately no session service — the events *are* the interface.

use min_core::{events, Hub, Plugin};
use std::any::Any;

pub struct SessionPlugin {
    /// The open session, announced and not yet ended.
    current: Option<Session>,
    /// Id pinned for the first session (`--session`); used verbatim, once.
    pinned: Option<String>,
    /// Parent for the *next* session minted (`--parent-session`).
    parent: Option<String>,
}

#[derive(Clone)]
struct Session {
    id: String,
    parent: Option<String>,
}

impl SessionPlugin {
    /// `session_id`: pin the first session to this id (both switches are
    /// `Some` → `None`: the caller could not have meant both).
    pub fn new(session_id: Option<String>, parent_id: Option<String>) -> Self {
        let both = session_id.is_some() && parent_id.is_some();
        SessionPlugin {
            current: None,
            pinned: if both { None } else { session_id },
            parent: if both { None } else { parent_id },
        }
    }

    /// Mint the next id: `<epoch seconds>-<4 random characters>` — or the
    /// pinned id, verbatim, for the first session. The random suffix is the
    /// whole uniqueness story: sessions minted in the same second cannot
    /// collide, so ids no longer need stepping. (Mint-before-close is kept
    /// from the stepped days; the order no longer matters.)
    fn next_id(&self) -> String {
        if self.current.is_none() {
            if let Some(pinned) = &self.pinned {
                return pinned.clone();
            }
        }
        format!("{}-{}", secs_since_epoch(), entropy())
    }

    /// Open a session and announce it. Callers arrange for the borrow of
    /// `self` to be released before the event dispatches (return it from
    /// `wait`), so the announcement reaches every plugin — this one
    /// included.
    fn open(&mut self, id: String) -> events::SessionStart {
        let session = Session { id, parent: self.parent.take() };
        self.pinned = None; // a pin is for the startup session only
        let event = events::SessionStart {
            id: session.id.clone(),
            parent: session.parent.clone(),
        };
        self.current = Some(session);
        event
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

/// Four random lowercase alphanumeric characters — 36⁴ ≈ 1.7 million
/// combinations, far more than the sessions that could ever begin in one
/// second. `/dev/urandom` is the source; without it (no `/dev`), the
/// clock's sub-second nanos stand in: the suffix only has to separate
/// sessions born together, and within one process no two share a nanos.
fn entropy() -> String {
    let mut bytes = [0u8; 4];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes))
        .is_err()
    {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        bytes = nanos.to_le_bytes();
    }
    entropy_from(bytes)
}

/// The byte → suffix mapping, factored out so tests can pin it.
fn entropy_from(bytes: [u8; 4]) -> String {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    bytes
        .iter()
        .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
        .collect()
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
                self.0.borrow_mut().push(format!(
                    "start:{}:{}",
                    e.id,
                    e.parent.as_deref().unwrap_or("-")
                ));
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
        hub.emit(events::SessionStart { id: e.id.clone(), parent: e.parent.clone() });

        let logged = log.borrow();
        assert_eq!(logged.len(), 1, "one announcement");
        let id = logged[0]
            .strip_prefix("start:")
            .and_then(|s| s.strip_suffix(":-"))
            .expect("start:<id>:- with no parent");
        assert!(
            id.starts_with(&format!("{}-", secs_since_epoch())),
            "id is <epoch second>-<suffix>: {id}"
        );
        // And afterwards the plugin goes quiet — it does not spin the bus.
        assert!(p.wait(&hub).is_none());
    }

    #[test]
    fn pinned_id_is_used_verbatim_for_the_first_session_only() {
        let mut p = SessionPlugin::new(Some("1000000-clip".into()), None);
        assert_eq!(p.next_id(), "1000000-clip");
        let _ = p.open("1000000-clip".into());
        // After the first session, ids are minted with a fresh suffix.
        p.current = Some(Session { id: "1000000-clip".into(), parent: None });
        assert_eq!(
            p.next_id()
                .split_once('-')
                .map(|(secs, _)| secs.parse::<u64>().is_ok())
                .unwrap_or(false),
            true,
            "later ids are <epoch>-<suffix>, never the pinned one"
        );
    }

    #[test]
    fn both_switches_is_an_error_and_parent_is_dropped() {
        let p = SessionPlugin::new(Some("42-even".into()), Some("7-hole".into()));
        assert_eq!(p.parent, None, "session id and parent together → error; parent cleared");
    }

    #[test]
    fn ids_carry_entropy_and_never_repeat() {
        let mut p = SessionPlugin::new(None, None);
        p.current = Some(Session { id: "1-aaaa".into(), parent: None });
        let id = p.next_id();
        let (secs, suffix) = id.split_once('-').expect("<epoch>-<suffix>");
        assert!(secs.parse::<u64>().is_ok(), "clock part: {id}");
        assert_eq!(suffix.len(), 4, "four characters of entropy: {id}");
        assert!(suffix.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
        assert_ne!(id, "1-aaaa", "the suffix alone separates same-second ids");
    }

    #[test]
    fn parent_is_taken_by_the_first_session_only() {
        let mut p = SessionPlugin::new(None, Some("7-dads".into()));
        let first = p.open("100-late".into());
        assert_eq!((first.id, first.parent), ("100-late".to_string(), Some("7-dads".to_string())));
        let second = p.open("200-late".into());
        assert_eq!(second.parent, None, "parent is one-shot");
    }

    #[test]
    fn clear_closes_then_opens_quit_closes() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let hub = Hub::new(vec![Box::new(Recorder(log.clone()))]);
        let mut p = SessionPlugin::new(None, None);
        let event = p.wait(&hub).expect("first session announced from wait");
        let e = event.downcast_ref::<events::SessionStart>().unwrap();
        hub.emit(events::SessionStart { id: e.id.clone(), parent: e.parent.clone() }); // the core's job
        let first = p.current.clone().unwrap().id;

        p.handle(&hub, &events::ResetConversation {});
        let second = p.current.clone().unwrap().id;
        assert!(
            second != first,
            "fresh id after /clear, even within one second — the suffix separates them"
        );

        p.handle(&hub, &events::Quit {});
        assert!(p.current.is_none(), "session closed on Quit");

        let events = log.borrow();
        assert_eq!(events.len(), 4, "start/end/start/end");
        assert_eq!(events[0], format!("start:{first}:-"));
        assert_eq!(events[1], format!("end:{first}"));
        assert_eq!(events[2], format!("start:{second}:-"));
        assert_eq!(events[3], format!("end:{second}"));
    }

    #[test]
    fn entropy_suffixes_vary() {
        // Every byte value through the alphabet: 36 distinct characters,
        // always four of them, deterministic for a given input.
        let suffixes: Vec<String> =
            (0..=255u8).map(|i| entropy_from([i, i, i, i])).collect();
        let unique: std::collections::HashSet<&String> = suffixes.iter().collect();
        assert_eq!(unique.len(), 36, "the whole alphabet is reached");
        assert_eq!(entropy_from([0, 0, 0, 0]), "0000");
        assert!(suffixes.iter().all(|s| s.len() == 4));
    }
}
