//! Drives a herdr-backed server to check that prompt marks survive its restart.
//!
//! ```text
//! herdr_marks_probe SOCKET seed N    Create a session, submit N `echo atur-mark-NN`
//!                                    commands, and require N marks on them.
//! herdr_marks_probe SOCKET check N   Attach to the adopted session and require the
//!                                    same N marks, on the same commands, in order.
//! ```
//!
//! Prints the marked rows as JSON. The CI job `herdr-marks` runs it against an
//! isolated herdr and `atur-term-server`, restarting the server in between.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use muxy_client::Client;
use muxy_protocol::Size;

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

const SIZE: Size = Size { cols: 80, rows: 24 };
const WAIT: Duration = Duration::from_secs(20);

fn main() -> Result {
    let mut arguments = std::env::args().skip(1);
    let socket = PathBuf::from(arguments.next().ok_or("usage: SOCKET (seed|check) N")?);
    let command = arguments.next().ok_or("a command is required")?;
    let count: usize = arguments.next().ok_or("a count is required")?.parse()?;
    let client = Client::connect(&socket)?;
    match command.as_str() {
        "seed" => seed(&client, count),
        "check" => check(&client, count),
        other => Err(format!("unknown command {other}").into()),
    }
}

fn seed(client: &Client, count: usize) -> Result {
    let session = client.create_session(Path::new("/tmp"), SIZE)?;
    let attachment = client.attach(session.id, SIZE)?;
    // Let the shell print its first prompt, then type like a user would.
    thread::sleep(Duration::from_secs(2));
    for index in 1..=count {
        client.send_input(
            attachment.channel,
            format!("echo atur-mark-{index:02}\r").as_bytes(),
        )?;
        // The owner finds the row a moment after the screen shows the command.
        thread::sleep(Duration::from_millis(1200));
    }
    let marked = wait_for_marks(client, session.id, count)?;
    writeln!(
        io::stdout(),
        "{}",
        serde_json::json!({"session": session.id.get(), "marks": marked})
    )?;
    Ok(())
}

fn check(client: &Client, count: usize) -> Result {
    let sessions = client.list_sessions()?;
    let [session] = sessions.as_slice() else {
        return Err(format!("expected one adopted session, found {}", sessions.len()).into());
    };
    let marked = wait_for_marks(client, session.id, count)?;
    writeln!(
        io::stdout(),
        "{}",
        serde_json::json!({"session": session.id.get(), "marks": marked})
    )?;
    Ok(())
}

/// Attaches afresh until exactly the `count` marked rows are the echo commands, in order.
fn wait_for_marks(
    client: &Client,
    session: muxy_protocol::SessionId,
    count: usize,
) -> Result<Vec<String>> {
    let deadline = Instant::now() + WAIT;
    loop {
        let attachment = client.attach(session, SIZE)?;
        let page = client.history_page(attachment.channel, muxy_protocol::HistoryCursor(0), 500)?;
        client.detach(attachment.channel)?;
        let grid = attachment.grid;
        let marked: Vec<String> = grid
            .prompts
            .iter()
            .filter_map(|row| grid.content_row(*row))
            .map(|runs| runs.iter().map(|run| run.text.as_str()).collect::<String>())
            .collect();
        let wanted: Vec<String> = (1..=count)
            .map(|index| format!("echo atur-mark-{index:02}"))
            .collect();
        let found: Vec<&str> = marked
            .iter()
            .map(|text| {
                wanted
                    .iter()
                    .find(|command| text.contains(command.as_str()))
                    .map_or("", String::as_str)
            })
            .collect();
        if found == wanted {
            return Ok(marked);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "expected marks on {wanted:?}; the snapshot has {} marks on {marked:?} (history page: {} prompts)",
                grid.prompts.len(),
                page.prompts.len()
            )
            .into());
        }
        thread::sleep(Duration::from_millis(500));
    }
}
