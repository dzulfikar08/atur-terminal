//! The images of a watched herdr terminal.
//!
//! herdr stores the Kitty images a pane's application sends, but its terminal
//! stream (`terminal session control`) is only ANSI text: images never reach it, so
//! a mirror built from that stream shows none. They reach herdr's own client
//! shells, see `shell`. While a session is watched this connects as one, asks for
//! the tab of the session's pane, and turns the scene it receives into the
//! `Scene` the session owner shows next to the mirror. The owner then handles
//! placement, scrolling, deletion, resizing and new viewers like those of any
//! image.
//!
//! The shell is a viewer for herdr, which has side effects this keeps small:
//! * herdr's session-wide focus follows the shell: it is the pane's tab while a
//!   session is watched (herdr's own windows keep the tab they show);
//! * the shell lays the tab out at its own size. It starts at herdr's own layout
//!   size, which changes nothing, and once on the pane's tab it is the size of the
//!   session (one column more: herdr keeps one for a scrollbar) when a control
//!   stream sizes the terminal, which keeps the terminal at the session's size, or
//!   herdr's layout size otherwise, which leaves herdr's size alone.

use std::collections::{HashMap, HashSet};
use std::io;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use muxy_protocol::Size;
use muxy_terminal::{
    CellSize, GraphicImage, GraphicPlacement, MAX_GRAPHICS_BYTES, MAX_GRAPHICS_PLACEMENTS,
};
use serde_json::{Value, json};

use super::api::Api;
use super::panes::Panes;
use super::shell::{self, AssetKey, Format, Message, Pane, Source, Surface};
use super::target::Target;
use crate::session::OwnerEvent;
use crate::session::backing::Scene;

/// How long before a lost connection is opened again.
const RETRY: Duration = Duration::from_millis(500);
/// The longest wait between attempts.
const MAX_RETRY: Duration = Duration::from_secs(4);
/// How long herdr may stay unreachable before its images are given up.
const GIVE_UP: Duration = Duration::from_secs(60);
/// How long a write to herdr may block.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// How often the shell asks again for the pane's tab when herdr shows another.
const REFOCUS: Duration = Duration::from_secs(1);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the session wants of the shell; it can change at any time.
#[derive(Debug)]
struct State {
    size: Size,
    cell: CellSize,
    /// Whether a control stream sizes the terminal at `size`.
    takeover: bool,
    /// herdr's layout size of the pane's tab, read when the shell connected.
    area: Option<(u16, u16)>,
    writer: Option<UnixStream>,
    /// The surface and cell size herdr was last told.
    announced: Option<(u16, u16, CellSize)>,
    /// Whether the shell shows the pane's tab, so that its size may follow the session.
    viewing: bool,
}

/// The surface that gets the session's terminal at its size.
fn surface_size(size: Size, takeover: bool, area: Option<(u16, u16)>) -> (u16, u16) {
    let own = (size.cols.saturating_add(1), size.rows);
    match (takeover, area) {
        (true, Some(area)) => (area.0.max(own.0), area.1.max(own.1)),
        (false, Some(area)) => area,
        (_, None) => own,
    }
}

#[derive(Debug)]
struct Shared {
    api: Arc<Api>,
    target: Arc<Target>,
    socket: PathBuf,
    state: Mutex<State>,
    stopped: AtomicBool,
    /// The shell showed the pane since this was last read.
    served: AtomicBool,
}

/// The shell connection of one watched session.
#[derive(Debug)]
pub(super) struct Images {
    shared: Arc<Shared>,
}

/// herdr's client socket: the API socket's name with `-client` before `.sock`.
fn client_socket(api: &std::path::Path) -> PathBuf {
    let stem = api
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("herdr");
    api.with_file_name(format!("{stem}-client.sock"))
}

impl Images {
    /// Starts connecting; `sink` receives the scene whenever it changes.
    pub(super) fn start(
        api: Arc<Api>,
        target: Arc<Target>,
        size: Size,
        cell: CellSize,
        takeover: bool,
        sink: Sender<OwnerEvent>,
    ) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            socket: client_socket(api.socket()),
            api,
            target,
            state: Mutex::new(State {
                size,
                cell,
                takeover,
                area: None,
                writer: None,
                announced: None,
                viewing: false,
            }),
            stopped: AtomicBool::new(false),
            served: AtomicBool::new(false),
        });
        let worker = Arc::clone(&shared);
        thread::Builder::new()
            .name(format!("herdr-images-{}", shared.target.terminal()))
            .spawn(move || worker.run(&sink))?;
        Ok(Self { shared })
    }

    /// The session's size, cell size and whether a control stream sizes it.
    pub(super) fn configure(&self, size: Size, cell: CellSize, takeover: bool) {
        let mut state = lock(&self.shared.state);
        state.size = size;
        state.cell = cell;
        state.takeover = takeover;
        announce(&mut state);
    }

    /// Ends the connection; its thread follows.
    pub(super) fn stop(&self) {
        self.shared.stopped.store(true, Ordering::Release);
        let mut state = lock(&self.shared.state);
        if let Some(mut writer) = state.writer.take() {
            let _ = shell::write_frame(&mut writer, &shell::detach());
            let _ = writer.shutdown(std::net::Shutdown::Both);
        }
    }
}

impl Drop for Images {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Tells herdr the size and cell size the session now needs, once the shell shows its tab.
fn announce(state: &mut State) {
    let (cols, rows) = surface_size(state.size, state.takeover, state.area);
    let wanted = (cols, rows, state.cell);
    if !state.viewing || state.announced == Some(wanted) {
        return;
    }
    let cell = state.cell;
    let Some(writer) = state.writer.as_mut() else {
        return;
    };
    if shell::write_frame(writer, &shell::resize(cols, rows, cell)).is_ok() {
        state.announced = Some(wanted);
    }
}

/// Where the session's pane is.
struct Place {
    pane: String,
    tab: String,
    area: Option<(u16, u16)>,
}

enum Outcome {
    /// The connection ended; it may be opened again.
    Lost,
    /// herdr does not serve this client, or the owner is gone.
    Final(String),
}

impl Shared {
    fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    fn run(&self, sink: &Sender<OwnerEvent>) {
        let mut down_since: Option<Instant> = None;
        let mut attempts = 0_u32;
        while !self.stopped() {
            match self.session(sink) {
                Ok(Outcome::Final(reason)) => {
                    log::info!("images of {}: {reason}", self.target.terminal());
                    return;
                }
                Ok(Outcome::Lost) => {}
                Err(error) => log::debug!("images of {}: {error}", self.target.terminal()),
            }
            if self.stopped() {
                return;
            }
            if self.served.swap(false, Ordering::AcqRel) {
                down_since = None;
                attempts = 0;
            }
            let since = *down_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= GIVE_UP {
                log::info!("images of {}: herdr did not answer", self.target.terminal());
                let _ = sink.send(OwnerEvent::Graphics(Scene::default()));
                return;
            }
            attempts += 1;
            thread::sleep((RETRY * attempts).min(MAX_RETRY));
        }
    }

    /// The pane, its tab and the layout size of that tab.
    fn locate(&self) -> io::Result<Place> {
        let panes =
            Panes::fetch(&self.api).ok_or_else(|| io::Error::other("herdr does not answer"))?;
        let pane = self
            .target
            .locate(&panes)
            .map(|pane| pane.pane.clone())
            .ok_or_else(|| io::Error::other("herdr no longer lists the terminal"))?;
        let layout = self
            .api
            .call("pane.layout", &json!({"pane_id": pane}))
            .map_err(io::Error::other)?;
        let layout = &layout["layout"];
        let tab = layout["tab_id"]
            .as_str()
            .ok_or_else(|| io::Error::other("herdr sent a layout without a tab"))?
            .to_owned();
        let side = |value: &Value| value.as_u64().and_then(|side| u16::try_from(side).ok());
        let area = side(&layout["area"]["width"])
            .zip(side(&layout["area"]["height"]))
            .filter(|(cols, rows)| *cols > 0 && *rows > 0);
        Ok(Place { pane, tab, area })
    }

    fn send(&self, payload: &[u8]) -> io::Result<()> {
        let mut state = lock(&self.state);
        let writer = state
            .writer
            .as_mut()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotConnected))?;
        shell::write_frame(writer, payload)
    }

    /// One connection, until it ends.
    #[allow(
        clippy::too_many_lines,
        reason = "One connection's handshake and message loop read in order"
    )]
    fn session(&self, sink: &Sender<OwnerEvent>) -> io::Result<Outcome> {
        let mut place = self.locate()?;
        let stream = UnixStream::connect(&self.socket)?;
        stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
        let mut reader = stream.try_clone()?;
        {
            let mut state = lock(&self.state);
            state.area = place.area;
            state.viewing = false;
            // herdr's own layout size changes nothing on the tabs the shell passes by.
            let (cols, rows) = place
                .area
                .unwrap_or_else(|| surface_size(state.size, state.takeover, None));
            let cell = state.cell;
            state.announced = Some((cols, rows, cell));
            if self.stopped() {
                return Ok(Outcome::Lost);
            }
            let mut writer = stream;
            shell::write_frame(&mut writer, &shell::hello(cols, rows, cell))?;
            state.writer = Some(writer);
        }
        let mut frame = Vec::new();
        let mut boot: Option<String> = None;
        let mut focused_at: Option<Instant> = None;
        let mut requests = 0_u64;
        let mut cache = Cache::default();
        let mut shown = Scene::default();
        loop {
            shell::read_frame(&mut reader, &mut frame)?;
            if self.stopped() {
                return Ok(Outcome::Lost);
            }
            match shell::parse(&frame) {
                Message::Welcome(data) => {
                    if let Some(reason) = refusal(data) {
                        return Ok(Outcome::Final(format!(
                            "herdr refused the client: {reason}"
                        )));
                    }
                }
                Message::Snapshot(data) => {
                    if boot.is_none() {
                        boot = boot_id(data);
                    }
                    if focused_at.is_none()
                        && let Some(boot) = &boot
                    {
                        requests += 1;
                        self.send(&shell::focus_tab(boot, &place.tab, requests))?;
                        focused_at = Some(Instant::now());
                    }
                }
                Message::Response(data) => {
                    if let Some(reason) = refusal_of_request(data) {
                        log::debug!("images of {}: {reason}", self.target.terminal());
                    }
                }
                Message::Closed(reason) => {
                    // herdr stopping or handing over: it answers again later.
                    log::debug!(
                        "images of {}: herdr closed: {reason}",
                        self.target.terminal()
                    );
                    return Ok(Outcome::Lost);
                }
                Message::Surface(surface) => {
                    let Some(pane) = surface.panes.iter().find(|pane| pane.id == place.pane) else {
                        // A surface of the tab herdr focused, or herdr moved the shell off the
                        // pane's tab: ask for the pane's tab again.
                        if let Some(boot) = &boot
                            && focused_at.is_none_or(|at| at.elapsed() >= REFOCUS)
                        {
                            if let Ok(found) = self.locate() {
                                place = found;
                            }
                            requests += 1;
                            self.send(&shell::focus_tab(boot, &place.tab, requests))?;
                            focused_at = Some(Instant::now());
                        }
                        continue;
                    };
                    let cell = {
                        let mut state = lock(&self.state);
                        if !state.viewing {
                            state.viewing = true;
                            announce(&mut state);
                        }
                        state.cell
                    };
                    self.served.store(true, Ordering::Release);
                    let scene = cache.scene(&surface, pane, cell);
                    if scene != shown {
                        shown = scene.clone();
                        if sink.send(OwnerEvent::Graphics(scene)).is_err() {
                            return Ok(Outcome::Final("the session ended".into()));
                        }
                    }
                }
                Message::Other => {}
            }
        }
    }
}

/// Why herdr refused the endpoint hello, from the welcome.
fn refusal(welcome: &str) -> Option<String> {
    let welcome: Value = serde_json::from_str(welcome).ok()?;
    let error = welcome.get("error").filter(|error| !error.is_null())?;
    Some(
        error["message"]
            .as_str()
            .unwrap_or("no reason given")
            .to_owned(),
    )
}

fn refusal_of_request(response: &[u8]) -> Option<String> {
    let response: Value = serde_json::from_slice(response).ok()?;
    let error = response.get("error").filter(|error| !error.is_null())?;
    Some(format!(
        "{}: {}",
        error["code"].as_str().unwrap_or("error"),
        error["message"].as_str().unwrap_or_default()
    ))
}

fn boot_id(snapshot: &str) -> Option<String> {
    let snapshot: Value = serde_json::from_str(snapshot).ok()?;
    snapshot["boot_id"].as_str().map(str::to_owned)
}

#[derive(Debug)]
struct Cached {
    id: u32,
    rgba: Arc<[u8]>,
}

/// The images this connection holds: herdr sends each once and then lists only
/// placements. An image no placement shows any more is let go, as herdr does.
#[derive(Debug, Default)]
struct Cache {
    images: HashMap<AssetKey, Cached>,
    bytes: usize,
    last_id: u32,
}

/// Expands what herdr stores to the RGBA the protocol carries. `None` for a format
/// or length this server cannot show.
fn rgba(key: &AssetKey, data: &[u8]) -> Option<Arc<[u8]>> {
    let pixels = usize::try_from(u64::from(key.width).checked_mul(u64::from(key.height))?).ok()?;
    let rgba_len = pixels.checked_mul(4)?;
    let rgb_len = pixels.checked_mul(3)?;
    match key.format {
        Format::Rgba if data.len() == rgba_len => Some(Arc::from(data)),
        Format::Rgb if data.len() == rgb_len => Some(
            data.chunks_exact(3)
                .flat_map(|rgb| [rgb[0], rgb[1], rgb[2], 255])
                .collect(),
        ),
        _ => None,
    }
}

impl Cache {
    fn next_id(&mut self) -> u32 {
        self.last_id = self.last_id.wrapping_add(1).max(1);
        self.last_id
    }

    /// The scene `surface` shows for `pane`, with the pane's terminal at the origin.
    fn scene(&mut self, surface: &Surface, pane: &Pane, cell: CellSize) -> Scene {
        let ours = |key: &AssetKey| matches!(&key.source, Source::Pane { pane: owner, .. } if *owner == pane.id);
        for (key, data) in &surface.assets {
            if !ours(key) || self.images.contains_key(key) {
                continue;
            }
            let Some(rgba) = rgba(key, data) else {
                log::debug!(
                    "an image of {}x{} pixels is not shown",
                    key.width,
                    key.height
                );
                continue;
            };
            if self.bytes + rgba.len() > MAX_GRAPHICS_BYTES {
                log::debug!("an image of {} bytes does not fit the limit", rgba.len());
                continue;
            }
            self.bytes += rgba.len();
            let id = self.next_id();
            self.images.insert(key.clone(), Cached { id, rgba });
        }
        let wanted: HashSet<&AssetKey> = surface
            .placements
            .iter()
            .map(|placement| &placement.asset)
            .chain(&surface.retained)
            .collect();
        self.images.retain(|key, image| {
            let keep = wanted.contains(key);
            if !keep {
                self.bytes -= image.rgba.len();
            }
            keep
        });

        let mut scene = Scene::default();
        for placement in surface
            .placements
            .iter()
            .filter(|placement| ours(&placement.asset))
        {
            let Some(image) = self.images.get(&placement.asset) else {
                continue;
            };
            let key = &placement.asset;
            let [x, y, width, height] = placement.source;
            if width == 0 || height == 0 || x >= key.width || y >= key.height {
                continue;
            }
            let width = width.min(key.width - x);
            let height = height.min(key.height - y);
            let size = [
                placement.cols.saturating_mul(u32::from(cell.width)),
                placement.rows.saturating_mul(u32::from(cell.height)),
            ];
            if size.contains(&0) {
                continue;
            }
            scene.placements.push(GraphicPlacement {
                image: image.id,
                id: 0,
                column: i32::from(placement.x) - i32::from(pane.inner.x),
                row: i32::from(placement.y) - i32::from(pane.inner.y),
                offset: placement.offset,
                size,
                source: [x, y, width, height],
                z: placement.z,
            });
        }
        scene.placements.sort_by_key(|placement| {
            (
                placement.z,
                placement.image,
                placement.row,
                placement.column,
            )
        });
        scene.placements.truncate(MAX_GRAPHICS_PLACEMENTS);
        for (index, placement) in scene.placements.iter_mut().enumerate() {
            placement.id = u32::try_from(index + 1).unwrap_or(u32::MAX);
        }
        let used: HashSet<u32> = scene
            .placements
            .iter()
            .map(|placement| placement.image)
            .collect();
        for (key, image) in &self.images {
            if used.contains(&image.id) {
                scene.images.push(GraphicImage {
                    id: image.id,
                    generation: key.fingerprint,
                    width: key.width,
                    height: key.height,
                    rgba: Arc::clone(&image.rgba),
                });
            }
        }
        scene.images.sort_by_key(|image| image.id);
        scene
    }
}

#[cfg(test)]
mod tests {
    use super::shell::{Placement, Rect};
    use super::*;

    const CELL: CellSize = CellSize {
        width: 9,
        height: 18,
    };

    fn key(pane: &str, image: u32, width: u32, height: u32, format: Format) -> AssetKey {
        let bytes = u64::from(width) * u64::from(height);
        AssetKey {
            source: Source::Pane {
                pane: pane.into(),
                image,
            },
            width,
            height,
            format,
            len: bytes * if format == Format::Rgb { 3 } else { 4 },
            fingerprint: u64::from(image) * 1000 + 7,
        }
    }

    fn placement(asset: &AssetKey, x: u16, y: u16, z: i32) -> Placement {
        Placement {
            asset: asset.clone(),
            id: 0,
            x,
            y,
            cols: 10,
            rows: 4,
            source: [0, 0, asset.width, asset.height],
            offset: [2, 3],
            z,
        }
    }

    fn pane() -> Pane {
        Pane {
            id: "w1:p1".into(),
            inner: Rect {
                x: 1,
                y: 2,
                width: 80,
                height: 24,
            },
        }
    }

    fn surface(assets: &[(&AssetKey, Vec<u8>)], placements: Vec<Placement>) -> Surface {
        Surface {
            panes: vec![pane()],
            assets: assets
                .iter()
                .map(|(key, data)| ((*key).clone(), data.clone()))
                .collect(),
            placements,
            retained: Vec::new(),
        }
    }

    #[test]
    fn a_placement_is_shown_at_the_terminal_cell_with_the_pixel_size_of_its_cells() {
        let image = key("w1:p1", 7, 2, 1, Format::Rgba);
        let mut cache = Cache::default();
        let scene = cache.scene(
            &surface(
                &[(&image, vec![1, 2, 3, 4, 5, 6, 7, 8])],
                vec![placement(&image, 5, 9, 0)],
            ),
            &pane(),
            CELL,
        );
        assert_eq!(scene.images.len(), 1);
        assert_eq!(
            (
                scene.images[0].width,
                scene.images[0].height,
                scene.images[0].generation
            ),
            (2, 1, 7007)
        );
        assert_eq!(&*scene.images[0].rgba, &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(
            scene.placements,
            [GraphicPlacement {
                image: scene.images[0].id,
                id: 1,
                column: 4,
                row: 7,
                offset: [2, 3],
                size: [90, 72],
                source: [0, 0, 2, 1],
                z: 0,
            }]
        );
    }

    #[test]
    fn an_image_is_held_until_no_placement_shows_it_and_sent_once() {
        let image = key("w1:p1", 7, 1, 1, Format::Rgba);
        let mut cache = Cache::default();
        let first = cache.scene(
            &surface(&[(&image, vec![9; 4])], vec![placement(&image, 1, 2, 0)]),
            &pane(),
            CELL,
        );
        // herdr lists the placement again but not the bytes.
        let moved = cache.scene(
            &surface(&[], vec![placement(&image, 1, 5, 0)]),
            &pane(),
            CELL,
        );
        assert_eq!(moved.images, first.images);
        assert!(Arc::ptr_eq(&moved.images[0].rgba, &first.images[0].rgba));
        assert_eq!(moved.placements[0].row, 3);
        let gone = cache.scene(&surface(&[], Vec::new()), &pane(), CELL);
        assert_eq!(gone, Scene::default());
        assert_eq!(cache.bytes, 0);
        // Shown again later, herdr sends the bytes again.
        let back = cache.scene(
            &surface(&[(&image, vec![9; 4])], vec![placement(&image, 1, 2, 0)]),
            &pane(),
            CELL,
        );
        assert_eq!(back.images.len(), 1);
        assert_ne!(back.images[0].id, first.images[0].id);
    }

    #[test]
    fn rgb_is_expanded_and_other_panes_and_bad_lengths_are_ignored() {
        let rgb = key("w1:p1", 1, 2, 1, Format::Rgb);
        let other = key("w1:p2", 2, 1, 1, Format::Rgba);
        let short = key("w1:p1", 3, 4, 4, Format::Rgba);
        let png = key("w1:p1", 4, 1, 1, Format::Png);
        let mut cache = Cache::default();
        let scene = cache.scene(
            &surface(
                &[
                    (&rgb, vec![1, 2, 3, 4, 5, 6]),
                    (&other, vec![0; 4]),
                    (&short, vec![0; 3]),
                    (&png, vec![0; 4]),
                ],
                vec![
                    placement(&rgb, 0, 2, 0),
                    placement(&other, 0, 2, 0),
                    placement(&short, 0, 2, 0),
                    placement(&png, 0, 2, 0),
                ],
            ),
            &pane(),
            CELL,
        );
        assert_eq!(scene.placements.len(), 1);
        assert_eq!(&*scene.images[0].rgba, &[1, 2, 3, 255, 4, 5, 6, 255]);
    }

    #[test]
    fn placements_are_ordered_by_depth_and_source_rectangles_are_kept_inside_the_image() {
        let image = key("w1:p1", 7, 10, 10, Format::Rgba);
        let mut under = placement(&image, 0, 2, -5);
        under.source = [4, 4, 100, 100];
        let mut empty = placement(&image, 0, 2, 0);
        empty.source = [10, 0, 1, 1];
        let over = placement(&image, 3, 2, 1);
        let mut cache = Cache::default();
        let scene = cache.scene(
            &surface(&[(&image, vec![0; 400])], vec![over, empty, under]),
            &pane(),
            CELL,
        );
        assert_eq!(
            scene
                .placements
                .iter()
                .map(|placement| (placement.id, placement.z, placement.source))
                .collect::<Vec<_>>(),
            [(1, -5, [4, 4, 6, 6]), (2, 1, [0, 0, 10, 10])]
        );
    }

    #[test]
    fn images_beyond_the_limit_are_not_held() {
        // 2048 x 1024 x 4 = 8 MiB: one fills the limit, so the second does not fit.
        let first = key("w1:p1", 1, 2048, 1024, Format::Rgba);
        let second = key("w1:p1", 2, 1, 1, Format::Rgba);
        let mut cache = Cache::default();
        let scene = cache.scene(
            &surface(
                &[(&first, vec![0; 2048 * 1024 * 4]), (&second, vec![0; 4])],
                vec![placement(&first, 0, 2, 0), placement(&second, 0, 2, 1)],
            ),
            &pane(),
            CELL,
        );
        assert_eq!(scene.images.len(), 1);
        assert_eq!(scene.placements.len(), 1);
        assert!(cache.bytes <= MAX_GRAPHICS_BYTES);
    }

    #[test]
    fn the_surface_follows_the_session_only_where_a_control_stream_sizes_the_terminal() {
        let size = Size {
            cols: 200,
            rows: 50,
        };
        assert_eq!(surface_size(size, true, Some((120, 40))), (201, 50));
        assert_eq!(surface_size(size, true, Some((300, 80))), (300, 80));
        assert_eq!(surface_size(size, false, Some((120, 40))), (120, 40));
        assert_eq!(surface_size(size, false, None), (201, 50));
    }

    #[test]
    fn the_client_socket_is_named_after_the_api_socket() {
        assert_eq!(
            client_socket(std::path::Path::new("/run/u/herdr.sock")),
            PathBuf::from("/run/u/herdr-client.sock")
        );
        assert_eq!(
            client_socket(std::path::Path::new("/tmp/x/custom.sock")),
            PathBuf::from("/tmp/x/custom-client.sock")
        );
    }

    #[test]
    fn refusals_and_boots_are_read_from_json() {
        assert_eq!(refusal("{\"generation\":1}"), None);
        assert_eq!(
            refusal("{\"error\":{\"code\":\"old\",\"message\":\"update herdr\"}}").as_deref(),
            Some("update herdr")
        );
        assert_eq!(
            boot_id("{\"boot_id\":\"b-1\",\"x\":[]}").as_deref(),
            Some("b-1")
        );
        assert_eq!(
            refusal_of_request(
                b"{\"id\":\"a\",\"error\":{\"code\":\"stale_boot\",\"message\":\"m\"}}"
            )
            .as_deref(),
            Some("stale_boot: m")
        );
        assert_eq!(refusal_of_request(b"{\"id\":\"a\",\"result\":{}}"), None);
    }
}
