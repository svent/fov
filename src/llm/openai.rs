//! OpenAI and OpenRouter, via async-openai's raw-JSON path.
//!
//! OpenAI uses the Responses API with a forced tool call: newer reasoning models accept
//! function tools together with a reasoning effort only there. OpenRouter uses chat
//! completions with `tool_choice: auto`, which also carries its
//! `provider.require_parameters`.

use anyhow::{Result, anyhow};
use async_openai::config::OpenAIConfig;
use async_openai::error::OpenAIError;
use serde_json::{Value, json};

use super::{Conversation, Reply, TOOL_DESCRIPTION, TOOL_NAME, ToolCall, Usage, tool_parameters};

pub const OPENAI_BASE: &str = "https://api.openai.com/v1";
pub const OPENROUTER_BASE: &str = "https://openrouter.ai/api/v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Api {
    /// OpenAI's `/responses`.
    Responses,
    /// OpenRouter's `/chat/completions`.
    OpenRouterChat,
}

pub struct Client {
    api: Api,
    model: String,
    reasoning_effort: Option<String>,
    inner: async_openai::Client<OpenAIConfig>,
}

impl Client {
    pub fn new(
        api: Api,
        model: &str,
        api_key: &str,
        base_url: &str,
        reasoning_effort: Option<String>,
    ) -> Client {
        let config = OpenAIConfig::new()
            .with_api_key(api_key)
            .with_api_base(base_url.trim_end_matches('/'));
        Client {
            api,
            model: model.to_string(),
            reasoning_effort,
            inner: async_openai::Client::with_config(config),
        }
    }

    pub fn name(&self) -> &'static str {
        match self.api {
            Api::Responses => "openai",
            Api::OpenRouterChat => "openrouter",
        }
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn reasoning_effort(&self) -> Option<&str> {
        self.reasoning_effort.as_deref()
    }

    pub async fn propose(&self, conversation: &Conversation) -> Result<Reply> {
        let effort = self.reasoning_effort.as_deref();
        let response: Value = match self.api {
            Api::Responses => self
                .inner
                .responses()
                .create_byot(responses_body(&self.model, effort, conversation))
                .await
                .map_err(|e| self.error(e))?,
            Api::OpenRouterChat => self
                .inner
                .chat()
                .create_byot(chat_body(&self.model, effort, conversation))
                .await
                .map_err(|e| self.error(e))?,
        };
        Ok(match self.api {
            Api::Responses => parse_responses(&response),
            Api::OpenRouterChat => parse_chat(&response),
        })
    }

    fn error(&self, e: OpenAIError) -> anyhow::Error {
        let name = self.name();
        let (status, text) = match &e {
            OpenAIError::ApiError(api) => (u64::from(api.status_code.as_u16()), e.to_string()),
            // OpenRouter's error object has an integer `code`, which async-openai fails to
            // parse. The body still holds the code and the message.
            OpenAIError::JSONDeserialize(_, body) => {
                let v: Value = serde_json::from_str(body).unwrap_or_default();
                let (Some(code), Some(message)) =
                    (v["error"]["code"].as_u64(), v["error"]["message"].as_str())
                else {
                    return anyhow!("{name}: {e}");
                };
                (code, message.to_string())
            }
            _ => return anyhow!("{name}: {e}"),
        };
        if (400..500).contains(&status) && status != 429 && text.to_lowercase().contains("tool") {
            let calls = match self.api {
                Api::Responses => "forced tool calls",
                Api::OpenRouterChat => "tool calls",
            };
            return anyhow!(
                "{name}: the model `{}` doesn't seem to support {calls}, which fov needs: {text}",
                self.model
            );
        }
        anyhow!("{name}: {text}")
    }
}

/// The Responses API request (§7.2): stateless, the tool forced, strict mode.
pub fn responses_body(
    model: &str,
    reasoning_effort: Option<&str>,
    conversation: &Conversation,
) -> Value {
    let mut input = Vec::new();
    for turn in &conversation.turns {
        input.push(json!({ "role": "user", "content": turn.user }));
        for a in &turn.attempts {
            input.push(json!({
                "type": "function_call",
                "call_id": a.call_id,
                "name": TOOL_NAME,
                "arguments": a.arguments.to_string()
            }));
            input.push(json!({
                "type": "function_call_output",
                "call_id": a.call_id,
                "output": a.result.to_string()
            }));
        }
    }
    let mut body = json!({
        "model": model,
        "instructions": conversation.system,
        "input": input,
        "tools": [{
            "type": "function",
            "name": TOOL_NAME,
            "description": TOOL_DESCRIPTION,
            "parameters": tool_parameters(),
            "strict": true
        }],
        "tool_choice": { "type": "function", "name": TOOL_NAME },
        "store": false,
        "stream": false
    });
    if let Some(effort) = reasoning_effort {
        body["reasoning"] = json!({ "effort": effort });
    }
    body
}

/// The chat completions request (§7.2), with OpenRouter's routing constraint. The tool
/// isn't forced: Claude 5.5 models reject a forced tool call, and OpenRouter routes none
/// of them when it's requested. The system prompt asks for the call instead.
pub fn chat_body(
    model: &str,
    reasoning_effort: Option<&str>,
    conversation: &Conversation,
) -> Value {
    let mut messages = vec![json!({ "role": "system", "content": conversation.system })];
    for turn in &conversation.turns {
        messages.push(json!({ "role": "user", "content": turn.user }));
        for a in &turn.attempts {
            messages.push(json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": a.call_id,
                    "type": "function",
                    "function": { "name": TOOL_NAME, "arguments": a.arguments.to_string() }
                }]
            }));
            messages.push(json!({ "role": "tool", "tool_call_id": a.call_id, "content": a.result.to_string() }));
        }
    }
    let mut body = json!({
        "model": model,
        "messages": messages,
        "tools": [{
            "type": "function",
            "function": {
                "name": TOOL_NAME,
                "description": TOOL_DESCRIPTION,
                "parameters": tool_parameters(),
                "strict": true
            }
        }],
        "tool_choice": "auto",
        "provider": { "require_parameters": true },
        "stream": false
    });
    if let Some(effort) = reasoning_effort {
        body["reasoning"] = json!({ "effort": effort });
    }
    body
}

pub fn parse_responses(response: &Value) -> Reply {
    let usage = Usage {
        input_tokens: response["usage"]["input_tokens"].as_u64().unwrap_or(0),
        output_tokens: response["usage"]["output_tokens"].as_u64().unwrap_or(0),
    };
    let call = response["output"]
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .find(|i| i["type"] == "function_call" && i["name"] == TOOL_NAME)
        })
        .map(|c| tool_call(&c["call_id"], &c["arguments"]));
    Reply { call, usage }
}

pub fn parse_chat(response: &Value) -> Reply {
    let usage = Usage {
        input_tokens: response["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
        output_tokens: response["usage"]["completion_tokens"].as_u64().unwrap_or(0),
    };
    let call = response["choices"][0]["message"]["tool_calls"]
        .as_array()
        .and_then(|calls| calls.iter().find(|c| c["function"]["name"] == TOOL_NAME))
        .map(|c| tool_call(&c["id"], &c["function"]["arguments"]));
    Reply { call, usage }
}

fn tool_call(id: &Value, arguments: &Value) -> ToolCall {
    let raw = arguments.as_str().unwrap_or_default();
    ToolCall {
        id: id.as_str().unwrap_or("call_0").to_string(),
        arguments: serde_json::from_str(raw).map_err(|_| raw.to_string()),
    }
}
