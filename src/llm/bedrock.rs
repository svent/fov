//! AWS Bedrock: the Converse API with `toolChoice: auto`. The SDK client is created with the
//! first request, so loading AWS credentials doesn't delay the first frame (§11).

use std::collections::HashMap;

use anyhow::{Result, anyhow};
use aws_config::{BehaviorVersion, Region};
use aws_sdk_bedrockruntime::error::DisplayErrorContext;
use aws_sdk_bedrockruntime::types::{
    AutoToolChoice, ContentBlock, ConversationRole, ConverseOutput, Message, SystemContentBlock,
    Tool, ToolChoice, ToolConfiguration, ToolInputSchema, ToolResultBlock, ToolResultContentBlock,
    ToolSpecification, ToolUseBlock,
};
use aws_smithy_types::{Document, Number};
use serde_json::Value;
use tokio::sync::OnceCell;

use super::{Conversation, Reply, TOOL_DESCRIPTION, TOOL_NAME, ToolCall, Usage, tool_parameters};

pub struct Client {
    model: String,
    profile: Option<String>,
    region: Option<String>,
    sdk: OnceCell<aws_sdk_bedrockruntime::Client>,
}

impl Client {
    pub fn new(model: &str, profile: Option<String>, region: Option<String>) -> Client {
        Client {
            model: model.to_string(),
            profile,
            region,
            sdk: OnceCell::new(),
        }
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub async fn propose(&self, conversation: &Conversation) -> Result<Reply> {
        let sdk = self
            .sdk
            .get_or_init(|| async {
                let mut loader = aws_config::defaults(BehaviorVersion::latest());
                if let Some(p) = &self.profile {
                    loader = loader.profile_name(p);
                }
                if let Some(r) = &self.region {
                    loader = loader.region(Region::new(r.clone()));
                }
                aws_sdk_bedrockruntime::Client::new(&loader.load().await)
            })
            .await;
        send(sdk, &self.model, conversation)
            .await
            .map_err(|e| self.hint(e))
    }

    fn hint(&self, e: anyhow::Error) -> anyhow::Error {
        let text = format!("{e:#}");
        let lower = text.to_lowercase();
        if lower.contains("sso") && (lower.contains("expired") || lower.contains("token")) {
            let p = self.profile.as_deref().unwrap_or("<profile>");
            return anyhow!(
                "{text}\nThe SSO session may have expired: run `aws sso login --profile {p}`"
            );
        }
        e
    }
}

/// Send one Converse request. Public so the wire format can be tested without network.
pub async fn send(
    sdk: &aws_sdk_bedrockruntime::Client,
    model: &str,
    conversation: &Conversation,
) -> Result<Reply> {
    let tool = ToolSpecification::builder()
        .name(TOOL_NAME)
        .description(TOOL_DESCRIPTION)
        .input_schema(ToolInputSchema::Json(to_document(&tool_parameters())))
        .strict(true)
        .build()?;
    // Not forced: Claude Sonnet 5.5 and Opus 5.5 reject a forced tool call (§7.2). The
    // system prompt asks for the call instead.
    let config = ToolConfiguration::builder()
        .tools(Tool::ToolSpec(tool))
        .tool_choice(ToolChoice::Auto(AutoToolChoice::builder().build()))
        .build()?;
    let out = sdk
        .converse()
        .model_id(model)
        .system(SystemContentBlock::Text(conversation.system.clone()))
        .set_messages(Some(messages(conversation)?))
        .tool_config(config)
        .send()
        .await
        .map_err(|e| anyhow!("bedrock: {}", DisplayErrorContext(&e)))?;
    let usage = out.usage().map_or_else(Usage::default, |u| Usage {
        input_tokens: u.input_tokens().max(0) as u64,
        output_tokens: u.output_tokens().max(0) as u64,
    });
    let call = match out.output() {
        Some(ConverseOutput::Message(m)) => m.content().iter().find_map(|b| match b {
            ContentBlock::ToolUse(t) if t.name() == TOOL_NAME => Some(ToolCall {
                id: t.tool_use_id().to_string(),
                arguments: Ok(from_document(t.input())),
            }),
            _ => None,
        }),
        _ => None,
    };
    Ok(Reply { call, usage })
}

/// Roles alternate: a follow-up query goes into the same user message as the preceding tool result.
fn messages(conversation: &Conversation) -> Result<Vec<Message>> {
    let mut out: Vec<(ConversationRole, Vec<ContentBlock>)> = Vec::new();
    for turn in &conversation.turns {
        let text = ContentBlock::Text(turn.user.clone());
        match out.last_mut() {
            Some((ConversationRole::User, blocks)) => blocks.push(text),
            _ => out.push((ConversationRole::User, vec![text])),
        }
        for a in &turn.attempts {
            let call = ToolUseBlock::builder()
                .tool_use_id(&a.call_id)
                .name(TOOL_NAME)
                .input(to_document(&a.arguments))
                .build()?;
            out.push((
                ConversationRole::Assistant,
                vec![ContentBlock::ToolUse(call)],
            ));
            let result = ToolResultBlock::builder()
                .tool_use_id(&a.call_id)
                .content(ToolResultContentBlock::Text(a.result.to_string()))
                .build()?;
            out.push((
                ConversationRole::User,
                vec![ContentBlock::ToolResult(result)],
            ));
        }
    }
    out.into_iter()
        .map(|(role, content)| {
            Ok(Message::builder()
                .role(role)
                .set_content(Some(content))
                .build()?)
        })
        .collect()
}

fn to_document(v: &Value) -> Document {
    match v {
        Value::Null => Document::Null,
        Value::Bool(b) => Document::Bool(*b),
        Value::Number(n) => Document::Number(if let Some(u) = n.as_u64() {
            Number::PosInt(u)
        } else if let Some(i) = n.as_i64() {
            Number::NegInt(i)
        } else {
            Number::Float(n.as_f64().unwrap_or(0.0))
        }),
        Value::String(s) => Document::String(s.clone()),
        Value::Array(a) => Document::Array(a.iter().map(to_document).collect()),
        Value::Object(o) => Document::Object(
            o.iter()
                .map(|(k, v)| (k.clone(), to_document(v)))
                .collect::<HashMap<_, _>>(),
        ),
    }
}

fn from_document(d: &Document) -> Value {
    match d {
        Document::Null => Value::Null,
        Document::Bool(b) => Value::Bool(*b),
        Document::Number(Number::PosInt(u)) => Value::from(*u),
        Document::Number(Number::NegInt(i)) => Value::from(*i),
        Document::Number(Number::Float(f)) => Value::from(*f),
        Document::String(s) => Value::String(s.clone()),
        Document::Array(a) => Value::Array(a.iter().map(from_document).collect()),
        Document::Object(o) => Value::Object(
            o.iter()
                .map(|(k, v)| (k.clone(), from_document(v)))
                .collect(),
        ),
    }
}
