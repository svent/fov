//! Providers, the conversation, and the `propose_pipeline` tool (§7).

pub mod bedrock;
pub mod openai;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde_json::{Value, json};

use crate::config::ProviderConfig;

pub const TOOL_NAME: &str = "propose_pipeline";
pub const TOOL_DESCRIPTION: &str =
    "Propose a linear pipeline that turns stdin into the requested output.";

/// The JSON schema of the tool's arguments (§7.2).
pub fn tool_parameters() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["commands", "explanation"],
        "properties": {
            "commands": {
                "type": "array",
                "description": "Pipeline stages in order. Empty if the request can't be done with the allowed tools.",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["program", "args"],
                    "properties": {
                        "program": { "type": "string", "description": "Bare name of an allowed tool." },
                        "args": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "The arguments after the program name (argv[1:]), without shell quoting. Never starts with the program name."
                        }
                    }
                }
            },
            "explanation": {
                "type": "string",
                "description": "One sentence: what the pipeline does, or why it can't be done."
            }
        }
    })
}

/// The conversation, in our own terms. Every client maps it to its wire format.
///
/// Invariants (§7.3): every attempt has its tool result; only complete turns are kept.
#[derive(Clone, Debug, Default)]
pub struct Conversation {
    pub system: String,
    pub turns: Vec<Turn>,
}

/// One query and its attempts.
#[derive(Clone, Debug)]
pub struct Turn {
    pub user: String,
    pub attempts: Vec<Attempt>,
}

/// A proposal (the tool call) with its outcome (the tool result).
#[derive(Clone, Debug)]
pub struct Attempt {
    pub call_id: String,
    pub arguments: Value,
    pub result: Value,
}

#[derive(Clone, Debug)]
pub struct Reply {
    pub call: Option<ToolCall>,
    pub usage: Usage,
}

#[derive(Clone, Debug)]
pub struct ToolCall {
    pub id: String,
    /// The arguments, or the raw text when it isn't valid JSON.
    pub arguments: Result<Value, String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

pub enum Provider {
    OpenAiCompat(openai::Client),
    Bedrock(bedrock::Client),
    Scripted(Arc<Scripted>),
}

impl Provider {
    pub fn from_config(c: &ProviderConfig) -> Provider {
        match c {
            ProviderConfig::OpenAi {
                model,
                api_key,
                base_url,
                reasoning_effort,
            } => Provider::OpenAiCompat(openai::Client::new(
                openai::Api::Responses,
                model,
                api_key,
                base_url.as_deref().unwrap_or(openai::OPENAI_BASE),
                reasoning_effort.clone(),
            )),
            ProviderConfig::OpenRouter {
                model,
                api_key,
                base_url,
                reasoning_effort,
            } => Provider::OpenAiCompat(openai::Client::new(
                openai::Api::OpenRouterChat,
                model,
                api_key,
                base_url.as_deref().unwrap_or(openai::OPENROUTER_BASE),
                reasoning_effort.clone(),
            )),
            ProviderConfig::Bedrock {
                model,
                profile,
                region,
            } => Provider::Bedrock(bedrock::Client::new(model, profile.clone(), region.clone())),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Provider::OpenAiCompat(c) => c.name(),
            Provider::Bedrock(_) => "bedrock",
            Provider::Scripted(_) => "scripted",
        }
    }

    pub fn model(&self) -> &str {
        match self {
            Provider::OpenAiCompat(c) => c.model(),
            Provider::Bedrock(c) => c.model(),
            Provider::Scripted(_) => "scripted",
        }
    }

    /// Ask for the next proposal. An error is a transport error (§7.6).
    pub async fn propose(&self, conversation: &Conversation) -> Result<Reply> {
        match self {
            Provider::OpenAiCompat(c) => c.propose(conversation).await,
            Provider::Bedrock(c) => c.propose(conversation).await,
            Provider::Scripted(s) => s.propose(conversation).await,
        }
    }
}

/// A canned reply for tests.
#[derive(Clone, Debug)]
pub enum Canned {
    /// A tool call with these arguments.
    Call(Value),
    /// A tool call whose arguments aren't valid JSON.
    Invalid(String),
    /// A reply without a tool call.
    NoCall,
    /// A transport error.
    Error(String),
    /// Never answers.
    Hang,
}

/// Returns canned replies in order and records every conversation it was sent.
#[derive(Debug, Default)]
pub struct Scripted {
    replies: Mutex<VecDeque<Canned>>,
    seen: Mutex<Vec<Conversation>>,
}

impl Scripted {
    pub fn new(replies: impl IntoIterator<Item = Canned>) -> Arc<Scripted> {
        Arc::new(Scripted {
            replies: Mutex::new(replies.into_iter().collect()),
            seen: Mutex::default(),
        })
    }

    pub fn push(&self, reply: Canned) {
        self.replies.lock().unwrap().push_back(reply);
    }

    pub fn seen(&self) -> Vec<Conversation> {
        self.seen.lock().unwrap().clone()
    }

    async fn propose(&self, conversation: &Conversation) -> Result<Reply> {
        let n = {
            let mut seen = self.seen.lock().unwrap();
            seen.push(conversation.clone());
            seen.len()
        };
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Canned::NoCall);
        let id = format!("call_{n}");
        let call = match reply {
            Canned::Call(v) => Some(ToolCall {
                id,
                arguments: Ok(v),
            }),
            Canned::Invalid(s) => Some(ToolCall {
                id,
                arguments: Err(s),
            }),
            Canned::NoCall => None,
            Canned::Error(e) => anyhow::bail!(e),
            Canned::Hang => std::future::pending().await,
        };
        Ok(Reply {
            call,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
            },
        })
    }
}
