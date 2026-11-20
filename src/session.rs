//! The engine both front-ends use: submit → propose → validate → preview → repair.
//! Neither front-end knows about duct, HTTP or Landlock (§11).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::config::Settings;
use crate::exec::{CancelToken, Executor, Outcome, Run};
use crate::input::{Input, Scope};
use crate::llm::{Attempt, Conversation, Provider, Turn, Usage};
use crate::pipeline::{self, Proposal, ResolvedPipeline};
use crate::prompt;
use crate::tools::Tools;

pub const MAX_REPAIRS: usize = 2;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// An event of one turn. `turn` identifies the submit it belongs to.
#[derive(Clone, Debug)]
pub struct Event {
    pub turn: u64,
    pub kind: EventKind,
}

#[derive(Clone, Debug)]
pub enum EventKind {
    /// Asking the model for the first proposal.
    Thinking,
    /// Asking again after a failure: repair `attempt` of `MAX_REPAIRS`.
    Repairing {
        attempt: usize,
        reason: String,
    },
    /// A proposal that passed validation.
    Proposed {
        command: String,
        explanation: String,
    },
    /// A proposal that failed validation.
    Rejected {
        command: Option<String>,
        explanation: Option<String>,
        reason: String,
    },
    /// The model declined: no commands.
    Declined {
        explanation: String,
    },
    /// Stdin must end before this input can be previewed (§8.3).
    WaitingForInput,
    Running {
        scope: Scope,
    },
    Ran {
        run: Run,
    },
    Done {
        result: TurnResult,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnResult {
    Ok,
    Declined,
    /// The last failure, after the repairs were used up or when a repair wouldn't help.
    Failed {
        reason: String,
    },
    /// Nothing usable came back (transport error, or no parseable reply). The turn was discarded.
    Error {
        message: String,
    },
}

/// The latest proposal of the last completed turn.
#[derive(Clone, Debug)]
pub struct Current {
    pub proposal: Proposal,
    pub command: String,
    /// Some when it passed validation, so it can be accepted.
    pub pipeline: Option<ResolvedPipeline>,
}

struct Shared {
    provider: Provider,
    exec: Executor,
    tools: Tools,
    input: Input,
    settings: Settings,
    nonce: String,
    os: String,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    conversation: Conversation,
    current: Option<Current>,
    usage: Usage,
    /// Whether a prefix of the input is still valid input; known once the head is cut.
    cuttable: Option<bool>,
}

pub struct Session {
    shared: Arc<Shared>,
    events: mpsc::UnboundedSender<Event>,
    task: Option<(JoinHandle<()>, CancelToken)>,
    turn: u64,
}

impl Session {
    pub fn new(
        provider: Provider,
        exec: Executor,
        tools: Tools,
        input: Input,
        settings: Settings,
    ) -> (Session, mpsc::UnboundedReceiver<Event>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let shared = Shared {
            provider,
            exec,
            tools,
            input,
            settings,
            nonce: prompt::nonce(),
            os: crate::tools::os_description(),
            state: Mutex::default(),
        };
        (
            Session {
                shared: Arc::new(shared),
                events: tx,
                task: None,
                turn: 0,
            },
            rx,
        )
    }

    /// Start a turn for `query`. A running turn is cancelled first. Returns the turn id.
    pub fn submit(&mut self, query: &str) -> u64 {
        self.cancel();
        self.turn += 1;
        let cancel = CancelToken::default();
        let (shared, query, tx, c, id) = (
            self.shared.clone(),
            query.to_string(),
            self.events.clone(),
            cancel.clone(),
            self.turn,
        );
        let handle = tokio::spawn(async move {
            let emit = |kind| drop(tx.send(Event { turn: id, kind }));
            let result = run_turn(&shared, &query, &emit, &c).await;
            emit(EventKind::Done { result });
        });
        self.task = Some((handle, cancel));
        id
    }

    /// Stop the running turn, if any. The turn is discarded (§7.3).
    pub fn cancel(&mut self) {
        if let Some((handle, cancel)) = self.task.take() {
            cancel.cancel();
            handle.abort();
        }
    }

    pub fn turn(&self) -> u64 {
        self.turn
    }

    pub fn current(&self) -> Option<Current> {
        self.shared.state.lock().unwrap().current.clone()
    }

    pub fn usage(&self) -> Usage {
        self.shared.state.lock().unwrap().usage
    }

    pub fn conversation(&self) -> Conversation {
        self.shared.state.lock().unwrap().conversation.clone()
    }

    pub fn provider(&self) -> &Provider {
        &self.shared.provider
    }

    pub fn input(&self) -> &Input {
        &self.shared.input
    }

    pub fn executor(&self) -> &Executor {
        &self.shared.exec
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.cancel();
    }
}

async fn run_turn(
    s: &Arc<Shared>,
    query: &str,
    emit: &(dyn Fn(EventKind) + Sync),
    cancel: &CancelToken,
) -> TurnResult {
    emit(EventKind::Thinking);
    let mut conv = s.state.lock().unwrap().conversation.clone();
    let user = if conv.turns.is_empty() {
        let head = match s
            .input
            .head(s.settings.head_bytes, s.settings.head_max_bytes)
            .await
        {
            Ok(h) => h,
            Err(e) => {
                return TurnResult::Error {
                    message: format!("reading the input: {e}"),
                };
            }
        };
        s.state.lock().unwrap().cuttable = Some(s.input.cuttable(&head, s.settings.sample_bytes));
        conv.system = prompt::system_prompt(&s.os, &s.tools, &s.nonce);
        prompt::first_message(query, s.input.name(), &s.input.progress(), &head, &s.nonce)
    } else {
        prompt::follow_up(query)
    };
    conv.turns.push(Turn {
        user,
        attempts: Vec::new(),
    });

    let mut failure = String::new();
    let mut current: Option<Current> = None;
    for attempt in 0..=MAX_REPAIRS {
        if attempt > 0 {
            emit(EventKind::Repairing {
                attempt,
                reason: failure.clone(),
            });
        }
        let reply = match tokio::time::timeout(REQUEST_TIMEOUT, s.provider.propose(&conv)).await {
            Err(_) => {
                return TurnResult::Error {
                    message: String::from("the model didn't answer within 60 s"),
                };
            }
            Ok(Err(e)) => {
                return TurnResult::Error {
                    message: format!("{e:#}"),
                };
            }
            Ok(Ok(r)) => r,
        };
        {
            let mut st = s.state.lock().unwrap();
            st.usage.input_tokens += reply.usage.input_tokens;
            st.usage.output_tokens += reply.usage.output_tokens;
        }
        // Unparseable replies are never recorded; the same request is repeated.
        let Some(call) = reply.call else {
            failure = format!(
                "`{}` replied without calling propose_pipeline",
                s.provider.model()
            );
            continue;
        };
        let args = match call.arguments {
            Ok(v) => v,
            Err(_) => {
                failure = String::from("the tool call's arguments weren't valid JSON");
                continue;
            }
        };
        let record = |conv: &mut Conversation, result: Value| {
            let attempts = &mut conv.turns.last_mut().expect("a turn").attempts;
            attempts.push(Attempt {
                call_id: call.id.clone(),
                arguments: args.clone(),
                result,
            });
        };

        let proposal: Proposal = match serde_json::from_value(args.clone()) {
            Ok(p) => p,
            Err(e) => {
                failure = format!("the arguments don't match the schema: {e}");
                record(&mut conv, prompt::rejected(&failure));
                emit(EventKind::Rejected {
                    command: None,
                    explanation: None,
                    reason: failure.clone(),
                });
                current = None;
                continue;
            }
        };
        if proposal.commands.is_empty() {
            record(&mut conv, prompt::declined());
            emit(EventKind::Declined {
                explanation: proposal.explanation.clone(),
            });
            commit(s, conv, None);
            return TurnResult::Declined;
        }
        let command = pipeline::render(&proposal.commands, s.input.redirect());
        let resolved = match pipeline::validate(&proposal.commands, &s.tools) {
            Ok(r) => r,
            Err(reason) => {
                record(&mut conv, prompt::rejected(&reason));
                emit(EventKind::Rejected {
                    command: Some(command.clone()),
                    explanation: Some(proposal.explanation.clone()),
                    reason: reason.clone(),
                });
                current = Some(Current {
                    proposal,
                    command,
                    pipeline: None,
                });
                failure = reason;
                continue;
            }
        };
        emit(EventKind::Proposed {
            command: command.clone(),
            explanation: proposal.explanation.clone(),
        });
        let run = preview(s, &resolved, emit, cancel).await;
        emit(EventKind::Ran { run: run.clone() });
        record(
            &mut conv,
            prompt::run_result(&run, &resolved, s.settings.timeout, &s.nonce),
        );
        current = Some(Current {
            proposal,
            command,
            pipeline: Some(resolved.clone()),
        });
        let cuttable = s.state.lock().unwrap().cuttable.unwrap_or(false);
        match judge(&run, &resolved, s.settings.timeout, cuttable) {
            Verdict::Ok => {
                commit(s, conv, current);
                return TurnResult::Ok;
            }
            Verdict::Repair(reason) => failure = reason,
            Verdict::Final(reason) => {
                commit(s, conv, current);
                return TurnResult::Failed { reason };
            }
        }
    }
    // A turn without a single recorded attempt would leave two user messages in a row.
    if conv.turns.last().is_some_and(|t| t.attempts.is_empty()) {
        return TurnResult::Error { message: failure };
    }
    commit(s, conv, current);
    TurnResult::Failed { reason: failure }
}

fn commit(s: &Shared, conversation: Conversation, current: Option<Current>) {
    let mut st = s.state.lock().unwrap();
    st.conversation = conversation;
    st.current = current;
}

async fn preview(
    s: &Arc<Shared>,
    p: &ResolvedPipeline,
    emit: &(dyn Fn(EventKind) + Sync),
    cancel: &CancelToken,
) -> Run {
    let cuttable = s.state.lock().unwrap().cuttable.unwrap_or(false);
    loop {
        let plan = match s.input.preview_plan(cuttable, s.settings.sample_bytes) {
            Ok(plan) => plan,
            Err(e) => {
                return Run {
                    scope: Scope::Full,
                    outcome: Outcome::SpawnError(format!("reading the input: {e}")),
                    stdout: Vec::new(),
                    stdout_lines: 0,
                    stderr: Vec::new(),
                    duration: Duration::ZERO,
                };
            }
        };
        let Some((primary, sample)) = plan else {
            emit(EventKind::WaitingForInput);
            s.input.wait_complete().await;
            continue;
        };
        emit(EventKind::Running {
            scope: primary.scope,
        });
        let (s2, p2, c2, timeout) = (s.clone(), p.clone(), cancel.clone(), s.settings.timeout);
        let job = tokio::task::spawn_blocking(move || {
            s2.exec
                .run_preview(&p2, &primary, sample.as_ref(), timeout, &c2)
        });
        return job.await.unwrap_or_else(|e| Run {
            scope: Scope::Full,
            outcome: Outcome::SpawnError(e.to_string()),
            stdout: Vec::new(),
            stdout_lines: 0,
            stderr: Vec::new(),
            duration: Duration::ZERO,
        });
    }
}

enum Verdict {
    Ok,
    Repair(String),
    Final(String),
}

/// §7.5: what a run means for the repair loop.
fn judge(run: &Run, p: &ResolvedPipeline, timeout: Duration, cuttable: bool) -> Verdict {
    match &run.outcome {
        Outcome::Ok => Verdict::Ok,
        Outcome::Failed { stage, code } => {
            let stderr = String::from_utf8_lossy(&run.stderr);
            let first = stderr
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim();
            let name = &p.stages[*stage].tool.name;
            let mut reason = format!("`{name}` (command {}) exited with status {code}", stage + 1);
            if !first.is_empty() {
                reason.push_str(": ");
                reason.push_str(first);
            }
            Verdict::Repair(reason)
        }
        // A timeout on input that can't be sampled isn't a wrong pipeline.
        Outcome::TimedOut if !cuttable => Verdict::Final(format!(
            "the preview didn't finish within {} ms, and this input can't be sampled; raise [preview].timeout_ms or --preview-timeout-ms",
            timeout.as_millis()
        )),
        Outcome::TimedOut => Verdict::Repair(format!(
            "the pipeline didn't finish within {} ms, even on a sample",
            timeout.as_millis()
        )),
        Outcome::Cancelled => Verdict::Final(String::from("cancelled")),
        Outcome::SpawnError(e) => Verdict::Final(format!("couldn't start the pipeline: {e}")),
    }
}
