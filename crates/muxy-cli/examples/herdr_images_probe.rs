//! Drives a herdr-backed server to check that Kitty images show, move, go away and
//! survive a restart of the server.
//!
//! ```text
//! herdr_images_probe SOCKET SCRIPT seed    Create a session and let `SCRIPT` (kitty_image.py)
//!                                          print images in it: one sent in chunks and
//!                                          placed at once, one sent and placed by two
//!                                          commands, one placed by Unicode placeholders.
//!                                          Require each on the client's screen, with the
//!                                          pixels and the cells they should have, then
//!                                          scrolled, resized, re-sized in cells and
//!                                          deleted; finally leave one image on the screen.
//! herdr_images_probe SOCKET SCRIPT check   Attach to the adopted session and require that
//!                                          image again.
//! ```
//!
//! The CI job `herdr-images` runs it against an isolated herdr and `atur-term-server`,
//! restarting the server in between.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use muxy_client::{Client, ClientEvent, RunGrid};
use muxy_protocol::{CellSize, ChannelId, Graphics, SessionId, Size};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

const SIZE: Size = Size { cols: 80, rows: 24 };
const CELL: CellSize = CellSize {
    width: 9,
    height: 18,
};
const WAIT: Duration = Duration::from_secs(30);
/// What `kitty_image.py` prints: a picture this many pixels wide and high, the left
/// half red and the right half green.
const WIDTH: u32 = 64;
const HEIGHT: u32 = 32;
const LEFT: [u8; 4] = [255, 0, 128, 255];
const RIGHT: [u8; 4] = [0, 255, 128, 255];

fn main() -> Result {
    let mut arguments = std::env::args().skip(1);
    let socket = PathBuf::from(
        arguments
            .next()
            .ok_or("usage: SOCKET SCRIPT (seed|check)")?,
    );
    let script = PathBuf::from(arguments.next().ok_or("the image script is required")?);
    let command = arguments.next().ok_or("a command is required")?;
    let client = Client::connect(&socket)?;
    let events = client
        .events()
        .ok_or("the connection has no event stream")?;
    match command.as_str() {
        "seed" => seed(&client, events, &script),
        "check" => check(&client, events),
        other => Err(format!("unknown command {other}").into()),
    }
}

struct Live<'a> {
    client: &'a Client,
    session: SessionId,
    channel: ChannelId,
    grid: RunGrid,
    events: Receiver<ClientEvent>,
}

impl<'a> Live<'a> {
    fn attach(
        client: &'a Client,
        session: SessionId,
        events: Receiver<ClientEvent>,
    ) -> Result<Self> {
        let attachment = client.attach(session, SIZE)?;
        Ok(Self {
            client,
            session,
            channel: attachment.channel,
            grid: attachment.grid,
            events,
        })
    }

    fn type_line(&self, line: &str) -> Result {
        self.client
            .send_input(self.channel, format!("{line}\r").as_bytes())?;
        Ok(())
    }

    /// Applies what the server sends until the images on the screen satisfy `test`.
    fn wait(&mut self, what: &str, test: impl Fn(&Graphics) -> bool) -> Result<Graphics> {
        let deadline = Instant::now() + WAIT;
        loop {
            if test(&self.grid.graphics) {
                return Ok(self.grid.graphics.clone());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!(
                    "timed out waiting for {what}; the screen shows {}",
                    describe(&self.grid.graphics)
                )
                .into());
            }
            match self
                .events
                .recv_timeout(left.min(Duration::from_millis(250)))
            {
                Ok(ClientEvent::Frame { channel, frame }) if channel == self.channel => {
                    self.grid.apply(&frame);
                    self.client.ack(channel, frame.seq)?;
                }
                Ok(ClientEvent::Disconnected) | Err(RecvTimeoutError::Disconnected) => {
                    return Err(format!("the server hung up while waiting for {what}").into());
                }
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }

    /// What a client that attaches now is told, whatever it was sent before.
    fn fresh_view(&self) -> Result<Graphics> {
        let deadline = Instant::now() + WAIT;
        loop {
            let attachment = self.client.attach(self.session, SIZE)?;
            let graphics = attachment.grid.graphics;
            self.client.detach(attachment.channel)?;
            if !graphics.images.is_empty() || Instant::now() >= deadline {
                return Ok(graphics);
            }
            thread::sleep(Duration::from_millis(300));
        }
    }
}

fn describe(graphics: &Graphics) -> String {
    let images: Vec<_> = graphics
        .images
        .iter()
        .map(|image| (image.id, image.width, image.height))
        .collect();
    let placements: Vec<_> = graphics
        .placements
        .iter()
        .map(|placement| {
            (
                placement.column,
                placement.row,
                placement.size,
                placement.source,
            )
        })
        .collect();
    format!("images {images:?}, placements (column, row, size, source) {placements:?}")
}

/// One image of the picture's size, placed at once in `cols` by `rows` cells of `cell`.
fn one_picture(graphics: &Graphics, cols: u32, rows: u32, cell: CellSize) -> bool {
    let [image] = graphics.images.as_slice() else {
        return false;
    };
    let [placement] = graphics.placements.as_slice() else {
        return false;
    };
    let pixel = |x: usize| {
        let at = x * 4;
        image.rgba.get(at..at + 4)
    };
    (image.width, image.height) == (WIDTH, HEIGHT)
        && pixel(0) == Some(&LEFT[..])
        && pixel(usize::try_from(WIDTH).unwrap_or(0) - 1) == Some(&RIGHT[..])
        && placement.image == image.id
        && placement.size == [cols * u32::from(cell.width), rows * u32::from(cell.height)]
        && placement.source == [0, 0, WIDTH, HEIGHT]
        && placement.column == 0
}

fn report(step: &str, graphics: &Graphics) -> Result {
    writeln!(
        io::stdout(),
        "{}",
        serde_json::json!({"step": step, "shows": describe(graphics)})
    )?;
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "One scenario, read from top to bottom"
)]
fn seed(client: &Client, events: Receiver<ClientEvent>, script: &Path) -> Result {
    let session = client.create_session(Path::new("/tmp"), SIZE)?;
    let mut live = Live::attach(client, session.id, events)?;
    client.send_cell_size(live.channel, CELL)?;
    // Let the shell print its first prompt, then type like a user would.
    thread::sleep(Duration::from_secs(2));
    let script = script.display();

    // An image sent in chunks and placed by the command that sent it, a few rows down.
    live.type_line(&format!(
        "clear; printf '\\n\\n\\n\\n\\n'; python3 {script} direct --id 7 --cols 10 --rows 4"
    ))?;
    let direct = live.wait("the chunked image", |g| one_picture(g, 10, 4, CELL))?;
    report("direct", &direct)?;
    let first_row = direct.placements[0].row;
    if !(0..24).contains(&first_row) {
        return Err(format!("the image is placed outside the screen at row {first_row}").into());
    }
    // A client that attaches now is told of it too.
    let fresh = live.fresh_view()?;
    if !one_picture(&fresh, 10, 4, CELL) || fresh.placements[0].row != first_row {
        return Err(format!("a new viewer is shown {}", describe(&fresh)).into());
    }

    // Output scrolls it up with the text.
    live.type_line("i=0; while [ $i -lt 16 ]; do echo line$i; i=$((i+1)); done")?;
    let scrolled = live.wait("the image to scroll up", |g| {
        g.placements.len() == 1 && g.placements[0].row < first_row
    })?;
    report("scrolled", &scrolled)?;
    // More output scrolls it off the screen: it goes away.
    live.type_line("i=0; while [ $i -lt 40 ]; do echo more$i; i=$((i+1)); done")?;
    let off = live.wait("the image to scroll off", |g| g.placements.is_empty())?;
    report("scrolled off", &off)?;

    // The application deletes its image.
    live.type_line(&format!(
        "clear; python3 {script} direct --id 8 --cols 10 --rows 4"
    ))?;
    live.wait("an image to delete", |g| one_picture(g, 10, 4, CELL))?;
    live.type_line(&format!("python3 {script} delete"))?;
    let deleted = live.wait("the deletion", |g| {
        g.images.is_empty() && g.placements.is_empty()
    })?;
    report("deleted", &deleted)?;

    // An image sent by one command and placed by another.
    live.type_line(&format!(
        "clear; python3 {script} put --id 9 --cols 8 --rows 3"
    ))?;
    let put = live.wait("the image sent and placed apart", |g| {
        one_picture(g, 8, 3, CELL)
    })?;
    report("transmit then place", &put)?;

    // Unicode placeholders: one placement for each row of cells.
    live.type_line(&format!(
        "clear; python3 {script} unicode --id 11 --cols 6 --rows 3"
    ))?;
    let placeholders = live.wait("the placeholder image", |g| {
        g.images.len() == 1
            && g.placements.len() == 3
            && g.placements.iter().all(|placement| {
                placement.size == [6 * u32::from(CELL.width), u32::from(CELL.height)]
            })
    })?;
    report("unicode placeholders", &placeholders)?;

    // The cells of the client change size: the placements follow.
    let bigger = CellSize {
        width: 10,
        height: 20,
    };
    client.send_cell_size(live.channel, bigger)?;
    let resized_cells = live.wait("the placements to follow the cell size", |g| {
        g.placements.len() == 3
            && g.placements
                .iter()
                .all(|placement| placement.size == [60, 20])
    })?;
    report("cell size", &resized_cells)?;
    client.send_cell_size(live.channel, CELL)?;

    // The session is resized and the image stays.
    live.type_line(&format!(
        "clear; python3 {script} direct --id 12 --cols 10 --rows 4"
    ))?;
    live.wait("the image to place again", |g| one_picture(g, 10, 4, CELL))?;
    let wide = Size {
        cols: 100,
        rows: 30,
    };
    client.resize(live.channel, wide)?;
    let resized = live.wait("the image after a resize", |g| one_picture(g, 10, 4, CELL))?;
    report("resized", &resized)?;
    client.resize(live.channel, SIZE)?;
    live.wait("the image after resizing back", |g| {
        one_picture(g, 10, 4, CELL)
    })?;

    // What stays for the check after the server restarts.
    live.type_line(&format!(
        "clear; printf '\\n\\n'; python3 {script} direct --id 13 --cols 10 --rows 4"
    ))?;
    let last = live.wait("the last image", |g| one_picture(g, 10, 4, CELL))?;
    report("left on the screen", &last)?;
    writeln!(
        io::stdout(),
        "{}",
        serde_json::json!({"session": session.id.get(), "row": last.placements[0].row})
    )?;
    Ok(())
}

fn check(client: &Client, events: Receiver<ClientEvent>) -> Result {
    let sessions = client.list_sessions()?;
    let [session] = sessions.as_slice() else {
        return Err(format!("expected one adopted session, found {}", sessions.len()).into());
    };
    let mut live = Live::attach(client, session.id, events)?;
    client.send_cell_size(live.channel, CELL)?;
    let shown = live.wait("the image after the restart", |g| {
        one_picture(g, 10, 4, CELL)
    })?;
    report("after the restart", &shown)?;
    let fresh = live.fresh_view()?;
    if fresh.images.len() != 1 {
        return Err(format!("a new viewer is shown {}", describe(&fresh)).into());
    }
    writeln!(
        io::stdout(),
        "{}",
        serde_json::json!({"session": session.id.get(), "row": shown.placements[0].row})
    )?;
    Ok(())
}
