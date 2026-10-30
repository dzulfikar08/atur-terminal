//! The terminal's own scrollbar, for panes the system gives none.

use std::{collections::BTreeSet, time::Instant};

use gpui::{Bounds, Context, Pixels, Task, point, px, size};
use muxy_ui::scrollbar::{
    MINIMUM_THUMB_LENGTH, REVEAL_DURATION, ScrollbarRevealState, ThumbGeometry, WIDTH,
};

use super::pane::{PaneState, TerminalPane};

/// The gap between the thumb and the pane's right edge.
const EDGE_GAP: f32 = 2.0;
/// Prompt ticks are a little narrower than the thumb and centred on it.
const PROMPT_TICK_WIDTH: f32 = 5.0;
const PROMPT_TICK_HEIGHT: f32 = 2.0;
/// More prompts than this are thinned to one tick per `PROMPT_TICK_SPACING`.
const MAX_PROMPT_TICKS: usize = 200;
const PROMPT_TICK_SPACING: f32 = 4.0;

#[derive(Debug)]
pub(crate) struct Scrollbar {
    reveal: ScrollbarRevealState,
    /// Reveal times count from here.
    epoch: Instant,
    hide: Option<Task<()>>,
    /// Where a drag holds the thumb, in pixels below its top.
    grab: Option<f32>,
    /// Where the pane was last drawn.
    track: Option<Bounds<Pixels>>,
}

struct Geometry {
    track: Bounds<Pixels>,
    thumb: ThumbGeometry,
    /// The rows the pane can scroll through.
    scrollable: f64,
    /// The rows the thumb's track stands for: those scrolled through and
    /// those on screen.
    total: f64,
}

impl Geometry {
    fn thumb_bounds(&self) -> Bounds<Pixels> {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "Thumb geometry is bounded by the track's f32 pixels"
        )]
        let (origin, length) = (self.thumb.origin as f32, self.thumb.length as f32);
        Bounds::new(
            point(
                self.track.right() - px(WIDTH + EDGE_GAP),
                self.track.top() + px(origin),
            ),
            size(px(WIDTH), px(length)),
        )
    }

    /// A tick for each of `prompts`, whose rows index the `loaded_rows` the
    /// pane holds at the bottom of the history. Prompts above the loaded
    /// window are unknown to the client and get no tick.
    fn prompt_ticks(&self, prompts: &BTreeSet<usize>, loaded_rows: usize) -> Vec<Bounds<Pixels>> {
        let left = self.track.right() - px(EDGE_GAP + f32::midpoint(WIDTH, PROMPT_TICK_WIDTH));
        let mut ticks = Vec::with_capacity(prompts.len().min(MAX_PROMPT_TICKS));
        prompt_offsets(
            prompts,
            loaded_rows,
            self.total,
            f32::from(self.track.size.height),
            |offset| {
                ticks.push(Bounds::new(
                    point(left, self.track.top() + px(offset)),
                    size(px(PROMPT_TICK_WIDTH), px(PROMPT_TICK_HEIGHT)),
                ));
            },
        );
        ticks
    }

    /// How far above the bottom the view is when the thumb's top sits `origin`
    /// pixels down the track.
    fn rows_from_bottom(&self, origin: f32) -> f64 {
        let travel = f64::from(f32::from(self.track.size.height)) - self.thumb.length;
        if travel <= 0.0 {
            return 0.0;
        }
        self.scrollable * (1.0 - (f64::from(origin) / travel).clamp(0.0, 1.0))
    }
}

impl Scrollbar {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            reveal: ScrollbarRevealState::default(),
            epoch: now,
            hide: None,
            grab: None,
            track: None,
        }
    }

    pub(super) fn place(&mut self, track: Bounds<Pixels>) {
        self.track = Some(track);
    }

    fn shown(&self, now: Instant) -> bool {
        self.reveal.allows_hit(now.duration_since(self.epoch))
    }
}

impl TerminalPane {
    fn scrollbar_geometry(&self) -> Option<Geometry> {
        #[cfg(target_os = "macos")]
        if self.native_scroll.is_some() {
            return None;
        }
        if !self.native_visible
            || self.reports_wheel()
            || matches!(self.state, PaneState::Connecting | PaneState::Disconnected)
        {
            return None;
        }
        let track = self.scrollbar.track?;
        let rows = self
            .viewport()
            .or(self.grid.as_ref().map(|grid| grid.size))?
            .rows;
        let scrollable = self.scrollable_rows(rows);
        let thumb = ThumbGeometry::from_lengths(
            scrollable + f64::from(rows),
            f64::from(rows),
            scrollable - self.scroll.requested_rows(),
            f64::from(f32::from(track.size.height)),
            MINIMUM_THUMB_LENGTH,
        )?;
        Some(Geometry {
            track,
            thumb,
            scrollable,
            total: scrollable + f64::from(rows),
        })
    }

    /// The thumb, while the scrollbar shows.
    pub(super) fn scrollbar_thumb(&self, now: Instant) -> Option<Bounds<Pixels>> {
        self.scrollbar
            .shown(now)
            .then(|| self.scrollbar_geometry())
            .flatten()
            .map(|geometry| geometry.thumb_bounds())
    }

    /// The prompt ticks, while the scrollbar shows. Each of the displayed
    /// grid's prompts that the client has loaded gets one.
    pub(super) fn scrollbar_prompt_ticks(&self, now: Instant) -> Vec<Bounds<Pixels>> {
        let Some(geometry) = self
            .scrollbar
            .shown(now)
            .then(|| self.scrollbar_geometry())
            .flatten()
        else {
            return Vec::new();
        };
        let Some(grid) = self.displayed_grid() else {
            return Vec::new();
        };
        geometry.prompt_ticks(&grid.prompts, grid.history.len() + grid.rows.len())
    }

    pub(super) fn scrollbar_dragging(&self) -> bool {
        self.scrollbar.grab.is_some()
    }

    /// Shows the scrollbar for a moment, as scrolling and pointing at it do.
    pub(super) fn reveal_scrollbar(&mut self, cx: &mut Context<Self>) {
        if self.scrollbar_geometry().is_none() {
            return;
        }
        let executor = cx.background_executor().clone();
        let now = executor.now();
        let shown = self.scrollbar.shown(now);
        self.scrollbar
            .reveal
            .reveal(now.duration_since(self.scrollbar.epoch));
        self.scrollbar.hide = Some(cx.spawn(async move |pane, cx| {
            executor.timer(REVEAL_DURATION).await;
            let _ = pane.update(cx, |pane, cx| {
                pane.scrollbar.hide = None;
                cx.notify();
            });
        }));
        if !shown {
            cx.notify();
        }
    }

    pub(super) fn hover_scrollbar(
        &mut self,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if self
            .scrollbar
            .track
            .is_some_and(|track| near_thumb(track, position))
        {
            self.reveal_scrollbar(cx);
        }
    }

    /// Starts dragging the thumb when the press lands on the shown scrollbar.
    pub(super) fn press_scrollbar(
        &mut self,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.scrollbar.shown(cx.background_executor().now()) {
            return false;
        }
        let Some(geometry) = self.scrollbar_geometry() else {
            return false;
        };
        if !near_thumb(geometry.track, position) {
            return false;
        }
        let thumb = geometry.thumb_bounds();
        let grab = if thumb.top() <= position.y && position.y <= thumb.bottom() {
            position.y - thumb.top()
        } else {
            thumb.size.height / 2.0
        };
        self.scrollbar.grab = Some(f32::from(grab));
        self.scrollbar.reveal.begin_drag();
        self.prepare_saved_history(cx);
        self.drag_scrollbar(position, cx);
        true
    }

    pub(super) fn drag_scrollbar(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let (Some(grab), Some(geometry)) = (self.scrollbar.grab, self.scrollbar_geometry()) else {
            return;
        };
        let origin = f32::from(position.y - geometry.track.top()) - grab;
        self.scroll_to(geometry.rows_from_bottom(origin), cx);
    }

    /// Ends a drag of the thumb, if one was under way.
    pub(super) fn release_scrollbar(&mut self, cx: &mut Context<Self>) -> bool {
        if self.scrollbar.grab.take().is_none() {
            return false;
        }
        let now = cx.background_executor().now();
        self.scrollbar
            .reveal
            .end_drag(now.duration_since(self.scrollbar.epoch));
        self.reveal_scrollbar(cx);
        cx.notify();
        true
    }
}

/// Whether `position` is over the strip of `track` the thumb moves in.
fn near_thumb(track: Bounds<Pixels>, position: gpui::Point<Pixels>) -> bool {
    track.contains(&position) && position.x >= track.right() - px(WIDTH * 2.0)
}

/// Calls `emit` with each prompt's pixel offset below the top of a track
/// `track_length` pixels tall, in ascending order. `prompts` index the
/// `loaded_rows` rows ending the `total_rows` the track stands for. Rows past
/// the loaded ones are skipped. Past `MAX_PROMPT_TICKS` prompts, only the first
/// in each `PROMPT_TICK_SPACING` pixels gets an offset, so the output stays
/// proportional to the track however many prompts there are.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "Row counts and pixel offsets are far inside f64 and f32 precision"
)]
pub(super) fn prompt_offsets(
    prompts: &BTreeSet<usize>,
    loaded_rows: usize,
    total_rows: f64,
    track_length: f32,
    mut emit: impl FnMut(f32),
) {
    if !total_rows.is_finite() || total_rows <= 0.0 || track_length <= PROMPT_TICK_HEIGHT {
        return;
    }
    let missing_above = (total_rows - loaded_rows as f64).max(0.0);
    let furthest = track_length - PROMPT_TICK_HEIGHT;
    let thin = prompts.len() > MAX_PROMPT_TICKS;
    let mut last_bucket = -1.0_f32;
    for &row in prompts.range(..loaded_rows) {
        let fraction = ((missing_above + row as f64) / total_rows).clamp(0.0, 1.0);
        let offset = ((fraction * f64::from(track_length)) as f32).min(furthest);
        if thin {
            let bucket = (offset / PROMPT_TICK_SPACING).floor();
            if bucket <= last_bucket {
                continue;
            }
            last_bucket = bucket;
        }
        emit(offset);
    }
}
