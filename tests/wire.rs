//! Provider wire format: snapshots of the request JSON per provider, without network (§13).

use aws_config::{BehaviorVersion, Region};
use aws_sdk_bedrockruntime::config::Credentials;
use fov::llm::{Attempt, Conversation, Turn, bedrock, openai};
use serde_json::{Value, json};

/// A completed turn followed by a follow-up query.
fn conversation() -> Conversation {
    Conversation {
        system: "SYSTEM PROMPT".into(),
        turns: vec![
            Turn {
                user: "Request: x".into(),
                attempts: vec![Attempt {
                    call_id: "call_1".into(),
                    arguments: json!({ "commands": [{ "program": "sort", "args": ["-rn"] }], "explanation": "e" }),
                    result: json!({ "status": "ok", "run": "full", "exit_code": 0 }),
                }],
            },
            Turn {
                user: "Follow-up: y".into(),
                attempts: vec![],
            },
        ],
    }
}

/// Pretty JSON with sorted keys, so the snapshot doesn't depend on map iteration order.
fn pretty(v: &Value) -> String {
    fn sorted(v: &Value) -> Value {
        match v {
            Value::Object(o) => {
                let mut keys: Vec<&String> = o.keys().collect();
                keys.sort();
                Value::Object(
                    keys.into_iter()
                        .map(|k| (k.clone(), sorted(&o[k])))
                        .collect(),
                )
            }
            Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
            v => v.clone(),
        }
    }
    serde_json::to_string_pretty(&sorted(v)).unwrap()
}

#[test]
fn openai_request() {
    insta::assert_snapshot!(pretty(&openai::responses_body(
        "model-x",
        Some("xhigh"),
        &conversation()
    )));
}

#[test]
fn openai_request_without_effort() {
    let body = openai::responses_body("model-x", None, &conversation());
    assert!(body.get("reasoning").is_none());
}

#[test]
fn openrouter_request() {
    insta::assert_snapshot!(pretty(&openai::chat_body(
        "model-x",
        Some("high"),
        &conversation()
    )));
}

#[test]
fn openai_response_parsing() {
    let r = openai::parse_responses(&json!({
        "output": [
            { "type": "reasoning", "id": "rs_1", "summary": [] },
            { "type": "function_call", "id": "fc_1", "call_id": "call_9", "name": "propose_pipeline",
              "arguments": "{\"commands\":[],\"explanation\":\"no\"}" }
        ],
        "usage": { "input_tokens": 120, "output_tokens": 7 }
    }));
    let call = r.call.unwrap();
    assert_eq!(call.id, "call_9");
    assert_eq!(call.arguments.unwrap()["explanation"], "no");
    assert_eq!((r.usage.input_tokens, r.usage.output_tokens), (120, 7));
    let r = openai::parse_responses(&json!({ "output": [{ "type": "message", "content": [] }] }));
    assert!(r.call.is_none());
}

#[test]
fn openrouter_response_parsing() {
    let r = openai::parse_chat(&json!({
        "choices": [{ "message": { "tool_calls": [{
            "id": "call_9", "type": "function",
            "function": { "name": "propose_pipeline", "arguments": "{\"commands\":[],\"explanation\":\"no\"}" }
        }]}}],
        "usage": { "prompt_tokens": 120, "completion_tokens": 7 }
    }));
    let call = r.call.unwrap();
    assert_eq!(call.id, "call_9");
    assert_eq!(call.arguments.unwrap()["explanation"], "no");
    assert_eq!((r.usage.input_tokens, r.usage.output_tokens), (120, 7));

    let r = openai::parse_chat(&json!({ "choices": [{ "message": { "tool_calls": [{
        "id": "c", "function": { "name": "propose_pipeline", "arguments": "{not json" } }] } }] }));
    assert_eq!(r.call.unwrap().arguments.unwrap_err(), "{not json");
    assert!(
        openai::parse_chat(&json!({ "choices": [{ "message": { "content": "hi" } }] }))
            .call
            .is_none()
    );
}

#[tokio::test]
async fn bedrock_request() {
    let (http, rx) = aws_smithy_http_client::test_util::capture_request(None);
    let conf = aws_sdk_bedrockruntime::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .credentials_provider(Credentials::new("AKID", "SECRET", None, None, "test"))
        .http_client(http)
        .build();
    let client = aws_sdk_bedrockruntime::Client::from_conf(conf);
    let _ = bedrock::send(&client, "model-x", &conversation()).await;
    let req = rx.expect_request();
    assert!(
        req.uri().ends_with("/model/model-x/converse"),
        "{}",
        req.uri()
    );
    let body: Value = serde_json::from_slice(req.body().bytes().unwrap()).unwrap();
    // Roles alternate: the follow-up shares the user message with the tool result.
    let roles: Vec<&str> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user", "assistant", "user"]);
    insta::assert_snapshot!(pretty(&body));
}

/// OpenRouter's error object has an integer `code`, which async-openai can't parse; the
/// message must still reach the user.
#[tokio::test]
async fn openrouter_error_is_readable() {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut len = 0;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                break;
            }
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                len = v.trim().parse().unwrap();
            }
        }
        reader.read_exact(&mut vec![0; len]).unwrap();
        let reply =
            r#"{"error":{"message":"No endpoints found that support tool use.","code":404}}"#;
        write!(
            stream,
            "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len()
        )
        .unwrap();
    });
    let client = openai::Client::new(openai::Api::OpenRouterChat, "model-x", "key", &url, None);
    let e = client
        .propose(&conversation())
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        e,
        "openrouter: the model `model-x` doesn't seem to support tool calls, which fov needs: \
         No endpoints found that support tool use."
    );
}
