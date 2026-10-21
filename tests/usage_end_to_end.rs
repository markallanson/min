//! End-to-end: a real request over a real socket, through the real agent,
//! into the real ledger.
//!
//! The fake OpenAI endpoint is a thread in this process (an ephemeral port),
//! so nothing is spawned and nothing is left behind. The chain exercised is
//! the whole one:
//!
//! ```text
//!   Llm (real HTTP + SSE) ──▶ agent ──▶ events::Usage ──▶ min-usage ──▶ ledger
//! ```

use min_core::usage::UsageLedger;
use min_core::{events, Hub, Plugin};
use std::any::Any;
use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::rc::Rc;

/// A fake OpenAI-compatible endpoint: text, then a finish reason, then a
/// usage chunk — the order the real API uses.
fn spawn_fake_endpoint() -> (u16, std::thread::JoinHandle<bool>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line.trim().is_empty() {
                break;
            }
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).unwrap();
        let body = String::from_utf8_lossy(&body).to_string();

        let mut out = stream;
        out.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n")
            .unwrap();
        out.write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n")
            .unwrap();
        out.write_all(b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n")
            .unwrap();
        out.write_all(
            b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":4242,\"completion_tokens\":17}}\n\n",
        )
        .unwrap();
        out.write_all(b"data: [DONE]\n\n").unwrap();
        out.flush().unwrap();

        // Did the request ask for usage? It must have.
        body.contains("\"include_usage\":true")
    });
    (port, handle)
}

/// Drives the turn: emits the user's message, then reads the ledger once
/// the turn is over.
struct Driver {
    snapshot: Rc<RefCell<Option<min_core::usage::UsageSnapshot>>>,
}

impl Plugin for Driver {
    fn name(&self) -> &'static str {
        "driver"
    }

    fn start(&mut self, hub: &Hub) {
        // The turn runs inside this call: the agent handles the input
        // synchronously, and the usage plugin — which is not busy — records
        // the usage event the agent emits along the way.
        hub.emit(events::UserInput {
            text: "say hello".into(),
        });
        *self.snapshot.borrow_mut() = hub.service::<UsageLedger>().map(|l| l.snapshot());
        hub.quit();
    }

    fn handle(&mut self, _hub: &Hub, _event: &dyn Any) {}
}

#[test]
fn a_turn_reports_its_tokens_into_the_ledger() {
    let (port, server) = spawn_fake_endpoint();

    // Point min at the fake endpoint. The environment is process-global, so
    // these are set once, here, before any plugin reads them.
    std::env::set_var("MIN_API", "openai");
    std::env::set_var("MIN_BASE_URL", format!("http://127.0.0.1:{port}/v1"));
    std::env::set_var("MIN_API_KEY", "test-key");
    std::env::set_var("MIN_MODEL", "gpt-4o");

    let snapshot = Rc::new(RefCell::new(None));
    let plugins: Vec<Box<dyn Plugin>> = vec![
        Box::new(min_llm::RouterPlugin),
        Box::new(min_llm::OpenAiPlugin),
        Box::new(min_usage::UsagePlugin::new()),
        Box::new(min_agent::AgentPlugin::new()),
        Box::new(Driver {
            snapshot: snapshot.clone(),
        }),
    ];
    min_core::run(plugins);

    assert!(server.join().unwrap(), "the request asked for usage");

    let s = snapshot
        .borrow()
        .expect("the ledger was published as a service");
    assert_eq!(s.requests, 1, "one request, one record");
    assert_eq!(s.total.input_tokens, 4242);
    assert_eq!(s.total.output_tokens, 17);
    assert_eq!(s.total.total(), 4259);
    // The context as the provider saw it.
    assert_eq!(s.last_prompt_tokens, 4242);
    assert!(s.reported);
}
