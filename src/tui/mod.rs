//! The interactive front-end. It renders session events and turns keys into session calls.

pub mod app;
pub mod keys;
pub mod view;

use std::io;
use std::time::Duration;

use crossterm::event::{Event as TermEvent, EventStream};
use futures_util::StreamExt;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::pipeline::ResolvedPipeline;
use crate::session::{Event, EventKind, Session};
use crate::terminal::{Screen, current_side_by_side};
use app::App;
use keys::Action;

pub enum Outcome {
    Accept {
        pipeline: ResolvedPipeline,
        command: String,
    },
    /// The user quit. After `q`, the shown command, to be printed.
    Quit(Option<String>),
    /// The input can't be used (e.g. the spool limit was exceeded).
    Fatal(String),
}

pub async fn run(
    screen: &mut Screen,
    session: &mut Session,
    rx: &mut UnboundedReceiver<Event>,
    mut app: App,
) -> io::Result<Outcome> {
    let mut keys = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut term = signal(SignalKind::terminate())?;
    let mut hup = signal(SignalKind::hangup())?;
    let mut side = current_side_by_side();
    app.tick(session.input());
    loop {
        screen.terminal.draw(|f| view::draw(f, &app, side))?;
        tokio::select! {
            ev = keys.next() => match ev {
                Some(Ok(TermEvent::Key(key))) => match keys::handle(&mut app, key) {
                    Action::Submit(q) => {
                        let turn = session.submit(&q);
                        app.start_turn(&q, turn);
                    }
                    Action::Cancel => {
                        session.cancel();
                        app.cancel_turn();
                    }
                    Action::Accept => {
                        if let Some(c) = session.current()
                            && let Some(pipeline) = c.pipeline
                        {
                            return Ok(Outcome::Accept { pipeline, command: c.command });
                        }
                    }
                    Action::Copy(command) => {
                        app.notice = Some(match crate::clipboard::copy(&command, screen.terminal.backend_mut()) {
                            Ok(done) => done.to_string(),
                            Err(e) => format!("copying failed: {e}"),
                        });
                    }
                    Action::Quit(command) => return Ok(Outcome::Quit(command)),
                    Action::None => {}
                },
                Some(Ok(TermEvent::Paste(text))) => app.paste(&text),
                Some(Ok(TermEvent::Resize(..))) => side = current_side_by_side(),
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e),
                None => return Ok(Outcome::Quit(None)),
            },
            Some(ev) = rx.recv() => {
                if ev.turn == app.turn {
                    let acceptable = matches!(ev.kind, EventKind::Done { .. })
                        && session.current().is_some_and(|c| c.pipeline.is_some());
                    app.on_event(ev.kind, acceptable);
                }
            }
            _ = tick.tick() => {
                app.tick(session.input());
                if let Some(e) = session.input().progress().error {
                    return Ok(Outcome::Fatal(e));
                }
            }
            _ = term.recv() => return Ok(Outcome::Quit(None)),
            _ = hup.recv() => return Ok(Outcome::Quit(None)),
        }
    }
}
