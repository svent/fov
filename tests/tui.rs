//! TUI snapshots of the main states in both layouts, on ratatui's TestBackend (§13).

use std::path::Path;
use std::time::Duration;

use fov::exec::{Outcome, Run};
use fov::input::{Input, Scope};
use fov::session::{EventKind, TurnResult};
use fov::tui::app::App;
use fov::tui::view;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

const COMMAND: &str = r#"awk -F, '$6=="completed"{s[$3]+=$7*$8} END{for(r in s) printf "%s %.2f\n",r,s[r]}' < sales.csv | sort -k2 -rn"#;

fn app(sandboxed: bool) -> App {
    let mut app = App::new(
        String::from("openrouter · test-model"),
        sandboxed,
        Vec::new(),
        Duration::from_secs(3),
        1 << 20,
    );
    let input = Input::file(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/sales.csv"),
        "sales.csv",
    )
    .unwrap();
    while !input.progress().counted {
        std::thread::sleep(Duration::from_millis(5));
    }
    app.tick(&input);
    app.spinner = 0;
    app
}

fn typed(app: &mut App, text: &str) {
    app.query = tui_input::Input::new(text.to_string());
}

fn run(outcome: Outcome, stdout: &str, stderr: &str) -> Run {
    Run {
        scope: Scope::Full,
        outcome,
        stdout: stdout.as_bytes().to_vec(),
        stdout_lines: stdout.lines().count() as u64,
        stderr: stderr.as_bytes().to_vec(),
        duration: Duration::from_millis(40),
    }
}

fn to_working(app: &mut App) {
    typed(app, "revenue of completed orders per region, highest first");
    app.start_turn("revenue of completed orders per region, highest first", 1);
    app.on_event(EventKind::Thinking, false);
}

fn to_review_ok(app: &mut App) {
    to_working(app);
    app.on_event(
        EventKind::Proposed {
            command: COMMAND.into(),
            explanation:
                "sums quantity × unit_price of completed orders per region, sorted descending"
                    .into(),
        },
        false,
    );
    let out =
        "North 295430.31\nSouth 240817.31\nCentral 174306.15\nEast 172422.70\nWest 158424.69\n";
    app.on_event(
        EventKind::Ran {
            run: run(Outcome::Ok, out, ""),
        },
        false,
    );
    app.on_event(
        EventKind::Done {
            result: TurnResult::Ok,
        },
        true,
    );
}

fn to_review_failed(app: &mut App) {
    to_working(app);
    app.on_event(
        EventKind::Proposed {
            command: "sort -k2 -X < sales.csv".into(),
            explanation: "sorts".into(),
        },
        false,
    );
    let failed = run(
        Outcome::Failed { stage: 0, code: 2 },
        "",
        "sort: invalid option -- X\n",
    );
    app.on_event(EventKind::Ran { run: failed }, false);
    let reason = String::from("`sort` (command 1) exited with status 2: sort: invalid option -- X");
    app.on_event(
        EventKind::Done {
            result: TurnResult::Failed { reason },
        },
        true,
    );
}

fn render(app: &App, side_by_side: bool) -> String {
    let (w, h) = if side_by_side { (100, 24) } else { (60, 40) };
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| view::draw(f, app, side_by_side)).unwrap();
    let buf = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..h {
        let line: String = (0..w).map(|x| buf[(x, y)].symbol()).collect();
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

macro_rules! snapshots {
    ($($name:ident: $setup:expr, $sandboxed:expr;)*) => {$(
        #[test]
        fn $name() {
            let mut a = app($sandboxed);
            $setup(&mut a);
            insta::assert_snapshot!(concat!(stringify!($name), "_side_by_side"), render(&a, true));
            insta::assert_snapshot!(concat!(stringify!($name), "_stacked"), render(&a, false));
        }
    )*};
}

snapshots! {
    compose: |a: &mut App| typed(a, "revenue of completed orders per region, highest first"), true;
    working: to_working, true;
    review_ok: to_review_ok, true;
    review_failed: to_review_failed, true;
    unsandboxed: |_: &mut App| {}, false;
}

/// A pipeline that prints nothing has run: the pane must not look as if it hadn't.
#[test]
fn empty_output_is_not_a_missing_preview() {
    let mut a = app(true);
    to_working(&mut a);
    a.on_event(
        EventKind::Ran {
            run: run(Outcome::Ok, "", ""),
        },
        false,
    );
    let screen = render(&a, true);
    assert!(screen.contains("(no output)"), "{screen}");
    assert!(!screen.contains("no preview yet"), "{screen}");
}
