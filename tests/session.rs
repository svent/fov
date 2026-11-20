//! Session tests with the Scripted provider (§13).

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use fov::config::Settings;
use fov::input::Input;
use fov::llm::{Canned, Conversation, Provider, Scripted};
use fov::sandbox::Sandbox;
use fov::session::{EventKind, Session, TurnResult};
use fov::tools::Tools;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;

struct Harness {
    session: Session,
    rx: UnboundedReceiver<fov::session::Event>,
    script: Arc<Scripted>,
    _dir: tempfile::TempDir,
}

fn harness(replies: Vec<Canned>) -> Harness {
    let (exec, dir) = executor(Sandbox::On);
    let tools = Tools::resolve(&["awk".into(), "sort".into(), "head".into(), "wc".into()]);
    let input = Input::file(&fixture("sales.csv"), "sales.csv").unwrap();
    let settings = Settings {
        head_bytes: 16 << 10,
        head_max_bytes: 64 << 10,
        timeout: Duration::from_secs(10),
        sample_bytes: 1 << 20,
    };
    let script = Scripted::new(replies);
    let (session, rx) = Session::new(
        Provider::Scripted(script.clone()),
        exec,
        tools,
        input,
        settings,
    );
    Harness {
        session,
        rx,
        script,
        _dir: dir,
    }
}

impl Harness {
    /// Submit and collect the turn's events up to Done.
    async fn turn(&mut self, query: &str) -> (Vec<EventKind>, TurnResult) {
        let id = self.session.submit(query);
        self.collect(id).await
    }

    async fn collect(&mut self, id: u64) -> (Vec<EventKind>, TurnResult) {
        let mut events = Vec::new();
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(20), self.rx.recv())
                .await
                .unwrap()
                .unwrap();
            if ev.turn != id {
                continue;
            }
            if let EventKind::Done { result } = ev.kind {
                return (events, result);
            }
            events.push(ev.kind);
        }
    }
}

fn call(commands: Value) -> Canned {
    Canned::Call(json!({ "commands": commands, "explanation": "test" }))
}

fn revenue() -> Canned {
    call(json!([
        { "program": "awk", "args": ["-F,", "$6==\"completed\"{s[$3]+=$7*$8} END{for(r in s) printf \"%s %.2f\\n\",r,s[r]}"] },
        { "program": "sort", "args": ["-k2", "-rn"] }
    ]))
}

fn failing() -> Canned {
    call(json!([{ "program": "awk", "args": ["BEGIN{print \"boom\" > \"/dev/stderr\"; exit 3}"] }]))
}

/// Every turn but the last has an attempt, and every attempt has a result: the request is valid.
fn assert_well_formed(c: &Conversation) {
    let (last, done) = c.turns.split_last().expect("a turn");
    assert!(
        done.iter().all(|t| !t.attempts.is_empty()),
        "a completed turn without attempts"
    );
    assert!(last.attempts.iter().all(|a| a.result.is_object()));
}

fn statuses(c: &Conversation, turn: usize) -> Vec<String> {
    c.turns[turn]
        .attempts
        .iter()
        .map(|a| a.result["status"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn multi_turn() {
    let mut h = harness(vec![
        revenue(),
        call(json!([{ "program": "wc", "args": ["-l"] }])),
    ]);
    let (events, result) = h.turn("revenue per region").await;
    assert_eq!(result, TurnResult::Ok);
    assert!(matches!(events[0], EventKind::Thinking));
    let ran = events
        .iter()
        .find_map(|e| {
            if let EventKind::Ran { run } = e {
                Some(run)
            } else {
                None
            }
        })
        .unwrap();
    assert!(String::from_utf8_lossy(&ran.stdout).starts_with("North 295430.31\n"));
    assert!(
        h.session
            .current()
            .unwrap()
            .command
            .ends_with("< sales.csv | sort -k2 -rn")
    );

    let (_, result) = h.turn("just count lines").await;
    assert_eq!(result, TurnResult::Ok);
    let seen = h.script.seen();
    assert_eq!(seen.len(), 2);
    let second = &seen[1];
    assert_well_formed(second);
    assert_eq!(second.turns.len(), 2);
    assert!(
        second.turns[0]
            .user
            .starts_with("Request: revenue per region")
    );
    assert!(second.turns[0].user.contains("<<<INPUT "));
    assert_eq!(second.turns[1].user, "Follow-up: just count lines");
    assert_eq!(statuses(second, 0), ["ok"]);
    let result = &second.turns[0].attempts[0].result;
    assert_eq!(result["output_lines"], 5);
    assert!(
        result["stdout_head"]
            .as_str()
            .unwrap()
            .starts_with("<<<STDOUT ")
    );
    assert_eq!(h.session.current().unwrap().command, "wc -l < sales.csv");
}

#[tokio::test]
async fn repair_after_reject() {
    let mut h = harness(vec![
        call(json!([{ "program": "xargs", "args": [] }])),
        revenue(),
    ]);
    let (events, result) = h.turn("revenue").await;
    assert_eq!(result, TurnResult::Ok);
    assert!(events.iter().any(
        |e| matches!(e, EventKind::Rejected { reason, .. } if reason == "`xargs` is not allowed")
    ));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EventKind::Repairing { attempt: 1, .. }))
    );
    let seen = h.script.seen();
    assert_well_formed(&seen[1]);
    assert_eq!(statuses(&seen[1], 0), ["rejected"]);
    assert_eq!(statuses(&h.session.conversation(), 0), ["rejected", "ok"]);
}

#[tokio::test]
async fn repair_after_schema_mismatch() {
    let mut h = harness(vec![
        Canned::Call(json!({ "commands": "awk", "explanation": "x" })),
        revenue(),
    ]);
    let (_, result) = h.turn("revenue").await;
    assert_eq!(result, TurnResult::Ok);
    let seen = h.script.seen();
    assert_eq!(statuses(&seen[1], 0), ["rejected"]);
    assert!(
        seen[1].turns[0].attempts[0].result["reason"]
            .as_str()
            .unwrap()
            .contains("schema")
    );
}

#[tokio::test]
async fn repair_after_failure() {
    let mut h = harness(vec![failing(), revenue()]);
    let (events, result) = h.turn("revenue").await;
    assert_eq!(result, TurnResult::Ok);
    let repairing = events.iter().find_map(|e| {
        if let EventKind::Repairing { reason, .. } = e {
            Some(reason)
        } else {
            None
        }
    });
    assert!(repairing.unwrap().contains("exited with status 3: boom"));
    let r = &h.script.seen()[1].turns[0].attempts[0].result;
    assert_eq!(
        (r["status"].as_str(), r["exit_code"].as_i64()),
        (Some("failed"), Some(3))
    );
    assert!(r["stderr_head"].as_str().unwrap().contains("boom"));
}

#[tokio::test]
async fn unparseable_reply_repeats_the_request() {
    let mut h = harness(vec![Canned::NoCall, Canned::Invalid("{".into()), revenue()]);
    let (_, result) = h.turn("revenue").await;
    assert_eq!(result, TurnResult::Ok);
    let seen = h.script.seen();
    assert_eq!(seen.len(), 3);
    assert!(
        seen.iter()
            .all(|c| c.turns.len() == 1 && c.turns[0].attempts.is_empty())
    );
    assert_eq!(statuses(&h.session.conversation(), 0), ["ok"]);
}

#[tokio::test]
async fn exhausted_retry_budget() {
    let mut h = harness(vec![failing(), failing(), failing(), revenue()]);
    let (_, result) = h.turn("revenue").await;
    assert!(
        matches!(result, TurnResult::Failed { ref reason } if reason.contains("status 3")),
        "{result:?}"
    );
    assert_eq!(h.script.seen().len(), 3);
    assert_eq!(
        statuses(&h.session.conversation(), 0),
        ["failed", "failed", "failed"]
    );
    assert!(
        h.session.current().unwrap().pipeline.is_some(),
        "a failed pipeline can still be accepted"
    );
}

#[tokio::test]
async fn only_unparseable_replies_discard_the_turn() {
    let mut h = harness(vec![
        Canned::NoCall,
        Canned::NoCall,
        Canned::NoCall,
        revenue(),
    ]);
    let (_, result) = h.turn("revenue").await;
    assert!(matches!(result, TurnResult::Error { .. }));
    assert!(h.session.conversation().turns.is_empty());
    let (_, result) = h.turn("revenue").await;
    assert_eq!(result, TurnResult::Ok);
    assert_well_formed(h.script.seen().last().unwrap());
}

#[tokio::test]
async fn decline_then_follow_up() {
    let mut h = harness(vec![call(json!([])), revenue()]);
    let (events, result) = h.turn("make coffee").await;
    assert_eq!(result, TurnResult::Declined);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EventKind::Declined { .. }))
    );
    assert!(h.session.current().is_none());
    let (_, result) = h.turn("revenue then").await;
    assert_eq!(result, TurnResult::Ok);
    let seen = h.script.seen();
    assert_well_formed(&seen[1]);
    assert_eq!(statuses(&seen[1], 0), ["declined"]);
    assert_eq!(seen[1].turns[1].user, "Follow-up: revenue then");
}

#[tokio::test]
async fn cancel_discards_the_turn() {
    let mut h = harness(vec![Canned::Hang]);
    let id = h.session.submit("revenue");
    let ev = h.rx.recv().await.unwrap();
    assert!(ev.turn == id && matches!(ev.kind, EventKind::Thinking));
    tokio::time::sleep(Duration::from_millis(50)).await;
    h.session.cancel();
    h.script.push(revenue());
    let (_, result) = h.turn("revenue again").await;
    assert_eq!(result, TurnResult::Ok);
    let seen = h.script.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[1].turns.len(), 1, "the cancelled turn is gone");
    assert!(seen[1].turns[0].user.starts_with("Request: revenue again"));
}

#[tokio::test]
async fn cancel_during_preview() {
    let mut h = harness(vec![call(
        json!([{ "program": "awk", "args": ["BEGIN{for(;;)x++}"] }]),
    )]);
    let id = h.session.submit("spin");
    loop {
        let ev = h.rx.recv().await.unwrap();
        if ev.turn == id && matches!(ev.kind, EventKind::Running { .. }) {
            break;
        }
    }
    h.session.cancel();
    assert!(h.session.conversation().turns.is_empty());
    h.script.push(revenue());
    let (_, result) = h.turn("revenue").await;
    assert_eq!(result, TurnResult::Ok);
}

#[tokio::test]
async fn transport_error_discards_the_turn() {
    let mut h = harness(vec![revenue(), Canned::Error("HTTP 503".into()), revenue()]);
    assert_eq!(h.turn("revenue").await.1, TurnResult::Ok);
    let (_, result) = h.turn("refine").await;
    assert!(matches!(result, TurnResult::Error { ref message } if message.contains("503")));
    assert_eq!(h.session.conversation().turns.len(), 1);
    assert_eq!(h.turn("refine").await.1, TurnResult::Ok);
    let last = h.script.seen().pop().unwrap();
    assert_well_formed(&last);
    assert_eq!(last.turns.len(), 2);
}

#[tokio::test]
async fn usage_is_summed() {
    let mut h = harness(vec![failing(), revenue()]);
    h.turn("revenue").await;
    assert_eq!(h.session.usage().input_tokens, 20);
}
