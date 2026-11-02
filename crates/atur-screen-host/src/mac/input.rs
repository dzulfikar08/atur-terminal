//! Mouse and keyboard input injected as `CGEvent`s, and the Accessibility permission check.
//!
//! macOS only lets a process post synthetic input when the user allowed it under Privacy &
//! Security → Accessibility, so [`accessibility_trusted`] is the gate for everything here.
//!
//! The decisions (which events a pointer message becomes, double-click counting, which
//! modifiers are held) are small pure types at the top of the file with unit tests; the
//! `CGEvent` plumbing is a thin layer of unsafe code below them. Every Core Foundation
//! object created here is owned by an `Event` or an `InputSource` right away, so the
//! "Create" rule reference is released exactly once.
#![allow(
    unsafe_code,
    reason = "CoreGraphics, ApplicationServices and Carbon have no safe bindings in the dependency tree"
)]
#![allow(
    non_snake_case,
    non_upper_case_globals,
    non_camel_case_types,
    reason = "declarations mirror the C headers"
)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use core_foundation::base::{CFRelease, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::data::CFDataGetBytePtr;
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::string::{CFString, CFStringRef};

use crate::geometry::Geometry;
use crate::keymap::{Flags, KeyStyle, Layout, Modifier, Resolved, resolve};

// ---------------------------------------------------------------------------------------
// Pure logic
// ---------------------------------------------------------------------------------------

/// A mouse button of the RFB button mask (bits 0 to 2).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Button {
    Left,
    Middle,
    Right,
}

impl Button {
    /// In the order presses and releases are posted, and the order that decides which button a
    /// drag belongs to when several are held.
    const ALL: [Self; 3] = [Self::Left, Self::Right, Self::Middle];

    /// The bit of the RFB button mask.
    const fn mask(self) -> u8 {
        match self {
            Self::Left => 1,
            Self::Middle => 1 << 1,
            Self::Right => 1 << 2,
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Left => 0,
            Self::Middle => 1,
            Self::Right => 2,
        }
    }

    /// The button a drag belongs to: the first held one.
    fn held(buttons: u8) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|button| buttons & button.mask() != 0)
    }

    /// The `CGMouseButton` number.
    const fn cg_button(self) -> u32 {
        match self {
            Self::Left => 0,
            Self::Right => 1,
            Self::Middle => 2,
        }
    }

    const fn pressed(self) -> u32 {
        match self {
            Self::Left => EVENT_LEFT_MOUSE_DOWN,
            Self::Right => EVENT_RIGHT_MOUSE_DOWN,
            Self::Middle => EVENT_OTHER_MOUSE_DOWN,
        }
    }

    const fn released(self) -> u32 {
        match self {
            Self::Left => EVENT_LEFT_MOUSE_UP,
            Self::Right => EVENT_RIGHT_MOUSE_UP,
            Self::Middle => EVENT_OTHER_MOUSE_UP,
        }
    }

    const fn dragged(self) -> u32 {
        match self {
            Self::Left => EVENT_LEFT_MOUSE_DRAGGED,
            Self::Right => EVENT_RIGHT_MOUSE_DRAGGED,
            Self::Middle => EVENT_OTHER_MOUSE_DRAGGED,
        }
    }
}

/// Bits 3 to 6 of the RFB button mask: the wheel, one notch per press.
const WHEEL_UP: u8 = 1 << 3;
const WHEEL_DOWN: u8 = 1 << 4;
const WHEEL_LEFT: u8 = 1 << 5;
const WHEEL_RIGHT: u8 = 1 << 6;
/// Lines scrolled by one wheel notch.
const WHEEL_LINES: i32 = 3;
/// Per wheel bit: the vertical and horizontal line counts of its event. Positive scrolls up
/// or left.
const WHEEL_STEPS: [(u8, i32, i32); 4] = [
    (WHEEL_UP, WHEEL_LINES, 0),
    (WHEEL_DOWN, -WHEEL_LINES, 0),
    (WHEEL_LEFT, 0, WHEEL_LINES),
    (WHEEL_RIGHT, 0, -WHEEL_LINES),
];

/// Clicks of the same button closer than this in time and space count as a double click.
const MULTI_CLICK_WINDOW: Duration = Duration::from_millis(500);
const MULTI_CLICK_SLOP: f64 = 4.0;

#[derive(Clone, Copy, Debug)]
struct LastClick {
    button: Button,
    at: Instant,
    point: (f64, f64),
    count: u32,
}

/// Numbers consecutive clicks: 1 for a single click, 2 for a double click, and so on.
#[derive(Debug, Default)]
struct ClickTracker {
    last: Option<LastClick>,
}

impl ClickTracker {
    /// The click count of a press of `button` at `point` and `now`.
    fn press(&mut self, button: Button, point: (f64, f64), now: Instant) -> u32 {
        let count = match self.last {
            Some(last)
                if last.button == button
                    && now.saturating_duration_since(last.at) <= MULTI_CLICK_WINDOW
                    && (last.point.0 - point.0).abs() <= MULTI_CLICK_SLOP
                    && (last.point.1 - point.1).abs() <= MULTI_CLICK_SLOP =>
            {
                last.count.saturating_add(1)
            }
            _ => 1,
        };
        self.last = Some(LastClick {
            button,
            at: now,
            point,
            count,
        });
        count
    }
}

/// One mouse event to post.
#[derive(Clone, Copy, Debug, PartialEq)]
enum PointerEvent {
    /// The pointer moved; `drag` is the held button and its click count, if any.
    Moved {
        point: (f64, f64),
        drag: Option<(Button, u32)>,
    },
    Button {
        point: (f64, f64),
        button: Button,
        down: bool,
        click_state: u32,
    },
    /// One wheel notch: line counts, positive scrolls up or left.
    Wheel { vertical: i32, horizontal: i32 },
}

/// What the viewer's pointer did so far: the button mask and position of its last message.
#[derive(Debug, Default)]
struct PointerState {
    buttons: u8,
    position: Option<(f64, f64)>,
    clicks: ClickTracker,
    /// The click count of the latest press of each button, for its drags and its release.
    counts: [u32; 3],
}

impl PointerState {
    /// Hands `emit` the events for a pointer message: the move first, then the releases, the
    /// presses, and one scroll per wheel bit that just went down.
    fn step(
        &mut self,
        buttons: u8,
        point: (f64, f64),
        now: Instant,
        mut emit: impl FnMut(PointerEvent),
    ) {
        let previous = self.buttons;
        if self.position != Some(point) {
            let drag = Button::held(previous).map(|button| (button, self.counts[button.index()]));
            emit(PointerEvent::Moved { point, drag });
        }
        for button in Button::ALL {
            if previous & button.mask() != 0 && buttons & button.mask() == 0 {
                emit(PointerEvent::Button {
                    point,
                    button,
                    down: false,
                    click_state: self.counts[button.index()],
                });
            }
        }
        for button in Button::ALL {
            if previous & button.mask() == 0 && buttons & button.mask() != 0 {
                let count = self.clicks.press(button, point, now);
                self.counts[button.index()] = count;
                emit(PointerEvent::Button {
                    point,
                    button,
                    down: true,
                    click_state: count,
                });
            }
        }
        let rising = buttons & !previous;
        for (bit, vertical, horizontal) in WHEEL_STEPS {
            if rising & bit != 0 {
                emit(PointerEvent::Wheel {
                    vertical,
                    horizontal,
                });
            }
        }
        self.buttons = buttons;
        self.position = Some(point);
    }

    /// Hands `emit` the release of every held button, at the last position.
    fn release_all(&mut self, mut emit: impl FnMut(PointerEvent)) {
        if let Some(point) = self.position {
            for button in Button::ALL {
                if self.buttons & button.mask() != 0 {
                    emit(PointerEvent::Button {
                        point,
                        button,
                        down: false,
                        click_state: self.counts[button.index()],
                    });
                }
            }
        }
        self.buttons = 0;
    }
}

/// `CGEventFlags` of the generic modifier bits.
const FLAG_BITS: [(Flags, u64); 4] = [
    (Flags::SHIFT, 0x0002_0000),
    (Flags::CONTROL, 0x0004_0000),
    (Flags::OPTION, 0x0008_0000),
    (Flags::COMMAND, 0x0010_0000),
];

/// The `CGEventFlags` for a set of modifiers.
fn flag_bits(flags: Flags) -> u64 {
    FLAG_BITS
        .iter()
        .filter(|(flag, _)| flags.contains(*flag))
        .fold(0, |bits, (_, bit)| bits | *bit)
}

/// The device-dependent `CGEventFlags` bit of one physical modifier key (`NX_DEVICE*KEYMASK`);
/// applications that tell the left key from the right one read these.
const fn device_bit(modifier: Modifier, right: bool) -> u64 {
    match (modifier, right) {
        (Modifier::Control, false) => 0x0000_0001,
        (Modifier::Shift, false) => 0x0000_0002,
        (Modifier::Shift, true) => 0x0000_0004,
        (Modifier::Command, false) => 0x0000_0008,
        (Modifier::Command, true) => 0x0000_0010,
        (Modifier::Option, false) => 0x0000_0020,
        (Modifier::Option, true) => 0x0000_0040,
        (Modifier::Control, true) => 0x0000_2000,
        (Modifier::CapsLock, _) => 0,
    }
}

/// The physical modifier keys held down, left and right apart.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Held(u8);

impl Held {
    const ALL: [(Modifier, bool); 8] = [
        (Modifier::Shift, false),
        (Modifier::Shift, true),
        (Modifier::Control, false),
        (Modifier::Control, true),
        (Modifier::Option, false),
        (Modifier::Option, true),
        (Modifier::Command, false),
        (Modifier::Command, true),
    ];

    /// Caps Lock is not a held key: it has no bit.
    fn bit(modifier: Modifier, right: bool) -> u8 {
        let index: u8 = match modifier {
            Modifier::Shift => 0,
            Modifier::Control => 1,
            Modifier::Option => 2,
            Modifier::Command => 3,
            Modifier::CapsLock => return 0,
        };
        1 << (index * 2 + u8::from(right))
    }

    fn contains(self, modifier: Modifier, right: bool) -> bool {
        self.0 & Self::bit(modifier, right) != 0
    }

    fn set(&mut self, modifier: Modifier, right: bool, down: bool) {
        if down {
            self.0 |= Self::bit(modifier, right);
        } else {
            self.0 &= !Self::bit(modifier, right);
        }
    }

    /// The modifiers with at least one key held.
    fn flags(self) -> Flags {
        let mut flags = Flags::NONE;
        for (modifier, right) in Self::ALL {
            if self.contains(modifier, right) {
                flags |= modifier.flag();
            }
        }
        flags
    }

    /// The device-dependent flag bits of the keys held.
    fn device_bits(self) -> u64 {
        Self::ALL
            .into_iter()
            .filter(|&(modifier, right)| self.contains(modifier, right))
            .fold(0, |bits, (modifier, right)| {
                bits | device_bit(modifier, right)
            })
    }

    /// The `CGEventFlags` of an event posted while these keys are held.
    fn cg_flags(self) -> u64 {
        flag_bits(self.flags()) | self.device_bits()
    }
}

/// A key press remembered until its release.
#[derive(Clone, Copy, Debug)]
struct Pressed {
    keycode: u16,
    flags: Flags,
}

// ---------------------------------------------------------------------------------------
// Foreign functions
// ---------------------------------------------------------------------------------------

/// `CGPoint` (a `CGFloat` is a double on the 64-bit targets).
#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

const EVENT_SOURCE_STATE_HID_SYSTEM: i32 = 1;
const HID_EVENT_TAP: u32 = 0;
const EVENT_LEFT_MOUSE_DOWN: u32 = 1;
const EVENT_LEFT_MOUSE_UP: u32 = 2;
const EVENT_RIGHT_MOUSE_DOWN: u32 = 3;
const EVENT_RIGHT_MOUSE_UP: u32 = 4;
const EVENT_MOUSE_MOVED: u32 = 5;
const EVENT_LEFT_MOUSE_DRAGGED: u32 = 6;
const EVENT_RIGHT_MOUSE_DRAGGED: u32 = 7;
const EVENT_FLAGS_CHANGED: u32 = 12;
const EVENT_OTHER_MOUSE_DOWN: u32 = 25;
const EVENT_OTHER_MOUSE_UP: u32 = 26;
const EVENT_OTHER_MOUSE_DRAGGED: u32 = 27;
const MOUSE_BUTTON_LEFT: u32 = 0;
const FIELD_MOUSE_CLICK_STATE: u32 = 1;
const FIELD_KEYBOARD_AUTOREPEAT: u32 = 8;
const SCROLL_UNIT_LINE: u32 = 1;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> u8;
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> u8;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceCreate(state_id: i32) -> *mut c_void;
    fn CGEventCreateKeyboardEvent(
        source: *mut c_void,
        virtual_key: u16,
        key_down: bool,
    ) -> *mut c_void;
    fn CGEventCreateMouseEvent(
        source: *mut c_void,
        mouse_type: u32,
        position: CGPoint,
        button: u32,
    ) -> *mut c_void;
    fn CGEventCreateScrollWheelEvent2(
        source: *mut c_void,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> *mut c_void;
    fn CGEventSetType(event: *mut c_void, event_type: u32);
    fn CGEventSetFlags(event: *mut c_void, flags: u64);
    fn CGEventSetIntegerValueField(event: *mut c_void, field: u32, value: i64);
    fn CGEventKeyboardSetUnicodeString(event: *mut c_void, length: usize, string: *const u16);
    fn CGEventPost(tap: u32, event: *mut c_void);
}

#[allow(clippy::too_many_arguments, reason = "mirrors the C declaration")]
unsafe extern "C" {
    fn TISCopyCurrentKeyboardLayoutInputSource() -> *mut c_void;
    fn TISCopyCurrentASCIICapableKeyboardLayoutInputSource() -> *mut c_void;
    fn TISGetInputSourceProperty(
        input_source: *mut c_void,
        property_key: CFStringRef,
    ) -> *const c_void;
    static kTISPropertyUnicodeKeyLayoutData: CFStringRef;
    fn LMGetKbdType() -> u8;
    fn UCKeyTranslate(
        key_layout: *const c_void,
        virtual_key_code: u16,
        key_action: u16,
        modifier_key_state: u32,
        keyboard_type: u32,
        key_translate_options: u32,
        dead_key_state: *mut u32,
        max_string_length: usize,
        actual_string_length: *mut usize,
        unicode_string: *mut u16,
    ) -> i32;
}

/// The key of the options dictionary of `AXIsProcessTrustedWithOptions` that asks the system
/// to show its permission prompt (`kAXTrustedCheckOptionPrompt`).
const AX_TRUSTED_CHECK_OPTION_PROMPT: &str = "AXTrustedCheckOptionPrompt";

/// Whether this process may post input events and watch the keyboard (the Accessibility
/// permission). With `prompt` the system shows its permission dialog when it is missing.
pub fn accessibility_trusted(prompt: bool) -> bool {
    if !prompt {
        // SAFETY: a plain query without arguments.
        return unsafe { AXIsProcessTrusted() } != 0;
    }
    let key = CFString::from_static_string(AX_TRUSTED_CHECK_OPTION_PROMPT);
    let options = CFDictionary::from_CFType_pairs(&[(key, CFBoolean::true_value())]);
    // SAFETY: `options` is a valid dictionary that outlives the call.
    unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef()) != 0 }
}

/// An owned `CGEvent`, released when dropped.
struct Event(NonNull<c_void>);

impl Event {
    fn keyboard(source: *mut c_void, keycode: u16, down: bool) -> Option<Self> {
        // SAFETY: a null source is allowed; the event is owned by the returned value.
        NonNull::new(unsafe { CGEventCreateKeyboardEvent(source, keycode, down) }).map(Self)
    }

    fn mouse(source: *mut c_void, kind: u32, point: (f64, f64), button: u32) -> Option<Self> {
        let position = CGPoint {
            x: point.0,
            y: point.1,
        };
        // SAFETY: as in `keyboard`.
        NonNull::new(unsafe { CGEventCreateMouseEvent(source, kind, position, button) }).map(Self)
    }

    fn scroll(source: *mut c_void, vertical: i32, horizontal: i32) -> Option<Self> {
        // SAFETY: as in `keyboard`.
        NonNull::new(unsafe {
            CGEventCreateScrollWheelEvent2(source, SCROLL_UNIT_LINE, 2, vertical, horizontal, 0)
        })
        .map(Self)
    }

    fn set_type(&self, event_type: u32) {
        // SAFETY: the pointer is a live event.
        unsafe { CGEventSetType(self.0.as_ptr(), event_type) };
    }

    fn set_flags(&self, flags: u64) {
        // SAFETY: the pointer is a live event.
        unsafe { CGEventSetFlags(self.0.as_ptr(), flags) };
    }

    fn set_field(&self, field: u32, value: i64) {
        // SAFETY: the pointer is a live event.
        unsafe { CGEventSetIntegerValueField(self.0.as_ptr(), field, value) };
    }

    fn set_unicode(&self, units: &[u16]) {
        // SAFETY: the pointer is a live event; the function copies `units.len()` code units.
        unsafe { CGEventKeyboardSetUnicodeString(self.0.as_ptr(), units.len(), units.as_ptr()) };
    }

    fn post(&self) {
        // SAFETY: the pointer is a live event.
        unsafe { CGEventPost(HID_EVENT_TAP, self.0.as_ptr()) };
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: the event came from a Create function and is released once, here.
        unsafe { CFRelease(self.0.as_ptr().cast_const()) };
    }
}

fn post_key(source: *mut c_void, keycode: u16, down: bool, flags: u64, repeat: bool) {
    let Some(event) = Event::keyboard(source, keycode, down) else {
        return;
    };
    event.set_flags(flags);
    if repeat {
        event.set_field(FIELD_KEYBOARD_AUTOREPEAT, 1);
    }
    event.post();
}

/// Posts a modifier key the way hardware reports it: a flags-changed event carrying the key
/// code and the flags after the change.
fn post_modifier(source: *mut c_void, keycode: u16, down: bool, flags: u64) {
    let Some(event) = Event::keyboard(source, keycode, down) else {
        return;
    };
    event.set_type(EVENT_FLAGS_CHANGED);
    event.set_flags(flags);
    event.post();
}

/// Types a character that no key produces: a press and a release carrying the character as
/// text. The modifiers are cleared so the character comes out as it is.
fn post_text(source: *mut c_void, c: char) {
    let mut buffer = [0_u16; 2];
    let units: &[u16] = c.encode_utf16(&mut buffer);
    for down in [true, false] {
        let Some(event) = Event::keyboard(source, 0, down) else {
            continue;
        };
        event.set_flags(0);
        event.set_unicode(units);
        event.post();
    }
}

fn post_mouse(
    source: *mut c_void,
    kind: u32,
    point: (f64, f64),
    button: u32,
    click_state: u32,
    flags: u64,
) {
    let Some(event) = Event::mouse(source, kind, point, button) else {
        return;
    };
    if click_state > 0 {
        event.set_field(FIELD_MOUSE_CLICK_STATE, i64::from(click_state));
    }
    event.set_flags(flags);
    event.post();
}

fn post_pointer(source: *mut c_void, flags: u64, event: PointerEvent) {
    match event {
        PointerEvent::Moved { point, drag } => {
            let (kind, button, click_state) = match drag {
                Some((button, count)) => (button.dragged(), button.cg_button(), count),
                None => (EVENT_MOUSE_MOVED, MOUSE_BUTTON_LEFT, 0),
            };
            post_mouse(source, kind, point, button, click_state, flags);
        }
        PointerEvent::Button {
            point,
            button,
            down,
            click_state,
        } => {
            let kind = if down {
                button.pressed()
            } else {
                button.released()
            };
            post_mouse(source, kind, point, button.cg_button(), click_state, flags);
        }
        PointerEvent::Wheel {
            vertical,
            horizontal,
        } => {
            let Some(scroll) = Event::scroll(source, vertical, horizontal) else {
                return;
            };
            scroll.set_flags(flags);
            scroll.post();
        }
    }
}

// ---------------------------------------------------------------------------------------
// Keyboard layout
// ---------------------------------------------------------------------------------------

/// An owned `TISInputSourceRef`, released when dropped.
struct InputSource(NonNull<c_void>);

impl InputSource {
    fn current() -> Option<Self> {
        // SAFETY: Copy rule: the result is owned by the returned value.
        NonNull::new(unsafe { TISCopyCurrentKeyboardLayoutInputSource() }).map(Self)
    }

    /// The latest ASCII-capable layout, for when the current source is an input method.
    fn ascii_capable() -> Option<Self> {
        // SAFETY: as in `current`.
        NonNull::new(unsafe { TISCopyCurrentASCIICapableKeyboardLayoutInputSource() }).map(Self)
    }

    /// The `UCKeyboardLayout` of the source; valid while `self` lives.
    fn layout_data(&self) -> Option<*const c_void> {
        // SAFETY: the source is live; the property is a `CFData` owned by it (Get rule), or null
        // for sources without a layout.
        let data =
            unsafe { TISGetInputSourceProperty(self.0.as_ptr(), kTISPropertyUnicodeKeyLayoutData) };
        if data.is_null() {
            return None;
        }
        // SAFETY: `data` is a live `CFData`.
        let bytes = unsafe { CFDataGetBytePtr(data.cast()) };
        (!bytes.is_null()).then_some(bytes.cast::<c_void>())
    }
}

impl Drop for InputSource {
    fn drop(&mut self) {
        // SAFETY: the source came from a Copy function and is released once, here.
        unsafe { CFRelease(self.0.as_ptr().cast_const()) };
    }
}

/// `UCKeyTranslate` modifier state bits: `shiftKey >> 8` and `optionKey >> 8`.
const UC_SHIFT: u32 = 1 << 1;
const UC_OPTION: u32 = 1 << 3;
/// `kUCKeyActionDown`.
const UC_KEY_ACTION_DOWN: u16 = 0;
/// `kUCKeyTranslateNoDeadKeysMask`: a dead key gives its own character.
const UC_NO_DEAD_KEYS: u32 = 1;

/// The character that key `keycode` types with Shift and/or Option on the layout `layout`.
fn translate(
    layout: *const c_void,
    keycode: u16,
    flags: Flags,
    keyboard_type: u32,
) -> Option<char> {
    let mut modifiers = 0;
    if flags.contains(Flags::SHIFT) {
        modifiers |= UC_SHIFT;
    }
    if flags.contains(Flags::OPTION) {
        modifiers |= UC_OPTION;
    }
    let mut dead_keys = 0_u32;
    let mut length = 0_usize;
    let mut units = [0_u16; 4];
    // SAFETY: `layout` is the `UCKeyboardLayout` of a live input source and the output
    // parameters are valid for the length given.
    let status = unsafe {
        UCKeyTranslate(
            layout,
            keycode,
            UC_KEY_ACTION_DOWN,
            modifiers,
            keyboard_type,
            UC_NO_DEAD_KEYS,
            &mut dead_keys,
            units.len(),
            &mut length,
            units.as_mut_ptr(),
        )
    };
    if status != 0 || length != 1 {
        return None;
    }
    char::from_u32(u32::from(units[0]))
}

/// Reads the keyboard layout in use. `None` (the caller falls back to the US layout) when the
/// text input system cannot be asked from this thread or gives nothing useful.
fn probe_layout() -> Option<Layout> {
    // The text input system asserts that it is called on the main thread.
    // SAFETY: a plain query without arguments.
    if unsafe { libc::pthread_main_np() } == 0 {
        log::debug!("not on the main thread: using the US keyboard layout");
        return None;
    }
    let found = [InputSource::current(), InputSource::ascii_capable()]
        .into_iter()
        .flatten()
        .find_map(|source| {
            let data = source.layout_data()?;
            Some((source, data))
        });
    let Some((source, data)) = found else {
        log::debug!("no keyboard layout data: using the US keyboard layout");
        return None;
    };
    // SAFETY: a plain query without arguments.
    let keyboard_type = u32::from(unsafe { LMGetKbdType() });
    let layout = Layout::probe(|keycode, flags| translate(data, keycode, flags, keyboard_type));
    if layout.is_none() {
        log::debug!("keyboard layout gave too few characters: using the US keyboard layout");
    }
    // `data` points into the source: keep it alive until the probe is done.
    drop(source);
    layout
}

// ---------------------------------------------------------------------------------------
// The injector
// ---------------------------------------------------------------------------------------

/// Posts the viewer's pointer and key events to the Mac as `CGEvent`s. Needs the Accessibility
/// permission (see [`accessibility_trusted`]); without it the system drops the events.
#[derive(Debug)]
pub struct Injector {
    /// The event source; null when the system gave none (events are posted without one).
    source: *mut c_void,
    style: KeyStyle,
    layout: Layout,
    /// The US layout, kept to find the keys of shortcuts when `layout` has no Latin letters.
    shortcut_layout: Option<Layout>,
    held: Held,
    /// Keys pressed, by keysym, so their release posts the same key code.
    pressed: HashMap<u32, Pressed>,
    mouse: PointerState,
}

// SAFETY: the event source is only used through `&mut self` or `&self` and Core Foundation
// objects may be used from any thread.
unsafe impl Send for Injector {}

impl Injector {
    /// An injector with `style` as the modifier mapping. Reads the keyboard layout in use when
    /// called on the main thread (the text input system insists on it); elsewhere, or when
    /// that fails, it assumes a US keyboard.
    pub fn new(style: KeyStyle) -> Self {
        let probed = probe_layout();
        let shortcut_layout = probed.is_some().then(Layout::ansi_us);
        let layout = probed.unwrap_or_else(Layout::ansi_us);
        // SAFETY: a plain constructor; the result is owned by the injector.
        let source = unsafe { CGEventSourceCreate(EVENT_SOURCE_STATE_HID_SYSTEM) };
        if source.is_null() {
            log::debug!("no event source: posting events without one");
        }
        Self {
            source,
            style,
            layout,
            shortcut_layout,
            held: Held::default(),
            pressed: HashMap::new(),
            mouse: PointerState::default(),
        }
    }

    /// The pointer at picture pixel `(x, y)` with the RFB button mask `buttons`: bits 0 to 2 are
    /// left, middle and right, bits 3 to 6 wheel up, down, left and right.
    pub fn pointer(&mut self, geometry: &Geometry, buttons: u8, x: u16, y: u16) {
        let point = geometry.point(x, y);
        let source = self.source;
        let flags = self.held.cg_flags();
        self.mouse.step(buttons, point, Instant::now(), |event| {
            post_pointer(source, flags, event);
        });
    }

    /// A key press (`down`) or release of the X11 keysym `keysym`. A press of a key that is
    /// already down is an auto-repeat.
    pub fn key(&mut self, keysym: u32, down: bool) {
        // A release undoes whatever the press did, whatever the keysym means now.
        let pressed = if down {
            None
        } else {
            self.pressed.remove(&keysym)
        };
        if let Some(pressed) = pressed {
            let flags = flag_bits(self.held.flags() | pressed.flags) | self.held.device_bits();
            post_key(self.source, pressed.keycode, false, flags, false);
            return;
        }
        let Some(resolved) = resolve(keysym, self.style, &self.layout) else {
            log::debug!("no Mac key for keysym {keysym:#x}");
            return;
        };
        match resolved {
            Resolved::Modifier { modifier, right } => self.modifier(modifier, right, down),
            Resolved::Key { keycode, flags } if down => self.press(keysym, keycode, flags),
            Resolved::Text(c) if down => self.type_char(keysym, c),
            // The release of a key that was never pressed.
            Resolved::Key { .. } | Resolved::Text(_) => {}
        }
    }

    /// Releases every key, modifier and mouse button that is held.
    pub fn release_all(&mut self) {
        let source = self.source;
        let held_flags = self.held.flags();
        let device_bits = self.held.device_bits();
        for (_, pressed) in self.pressed.drain() {
            let flags = flag_bits(held_flags | pressed.flags) | device_bits;
            post_key(source, pressed.keycode, false, flags, false);
        }
        for (modifier, right) in Held::ALL {
            if self.held.contains(modifier, right) {
                self.held.set(modifier, right, false);
                post_modifier(source, modifier.keycode(right), false, self.held.cg_flags());
            }
        }
        let flags = self.held.cg_flags();
        self.mouse.release_all(|event| {
            post_pointer(source, flags, event);
        });
    }

    fn modifier(&mut self, modifier: Modifier, right: bool, down: bool) {
        // Caps Lock toggles state on the Mac; a viewer's key events would leave it out of step.
        if modifier == Modifier::CapsLock {
            return;
        }
        if down && self.held.contains(modifier, right) {
            return;
        }
        self.held.set(modifier, right, down);
        post_modifier(
            self.source,
            modifier.keycode(right),
            down,
            self.held.cg_flags(),
        );
    }

    fn press(&mut self, keysym: u32, keycode: u16, flags: Flags) {
        let repeat = self
            .pressed
            .insert(keysym, Pressed { keycode, flags })
            .is_some();
        let flags = flag_bits(self.held.flags() | flags) | self.held.device_bits();
        post_key(self.source, keycode, true, flags, repeat);
    }

    fn type_char(&mut self, keysym: u32, c: char) {
        if let Some((keycode, flags)) = self.shortcut_key(c) {
            self.press(keysym, keycode, flags);
        } else {
            post_text(self.source, c);
        }
    }

    /// The US key of `c` while Control or Command is held: shortcuts follow the key positions
    /// of a Latin layout even when the layout in use has no Latin letters.
    fn shortcut_key(&self, c: char) -> Option<(u16, Flags)> {
        if !self
            .held
            .flags()
            .intersects(Flags::CONTROL | Flags::COMMAND)
        {
            return None;
        }
        self.shortcut_layout.as_ref()?.lookup(c)
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        self.release_all();
        if !self.source.is_null() {
            // SAFETY: the source came from a Create function and is released once, here.
            unsafe { CFRelease(self.source.cast_const()) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Instant {
        Instant::now()
    }

    fn later(base: Instant, millis: u64) -> Instant {
        base + Duration::from_millis(millis)
    }

    fn run(
        state: &mut PointerState,
        buttons: u8,
        point: (f64, f64),
        now: Instant,
    ) -> Vec<PointerEvent> {
        let mut events = Vec::new();
        state.step(buttons, point, now, |event| events.push(event));
        events
    }

    fn button(point: (f64, f64), button: Button, down: bool, click_state: u32) -> PointerEvent {
        PointerEvent::Button {
            point,
            button,
            down,
            click_state,
        }
    }

    fn moved(point: (f64, f64), drag: Option<(Button, u32)>) -> PointerEvent {
        PointerEvent::Moved { point, drag }
    }

    #[test]
    fn clicks_close_in_time_and_space_are_counted() {
        let t = base();
        let mut clicks = ClickTracker::default();
        assert_eq!(clicks.press(Button::Left, (10.0, 10.0), t), 1);
        assert_eq!(clicks.press(Button::Left, (12.0, 11.0), later(t, 200)), 2);
        assert_eq!(clicks.press(Button::Left, (12.0, 11.0), later(t, 400)), 3);
        // 501 ms after the previous press.
        assert_eq!(clicks.press(Button::Left, (12.0, 11.0), later(t, 901)), 1);
        // The window is measured from the previous press, not from the first.
        assert_eq!(clicks.press(Button::Left, (12.0, 11.0), later(t, 1400)), 2);
    }

    #[test]
    fn another_button_or_a_distant_click_starts_over() {
        let t = base();
        let mut clicks = ClickTracker::default();
        assert_eq!(clicks.press(Button::Left, (0.0, 0.0), t), 1);
        assert_eq!(clicks.press(Button::Right, (0.0, 0.0), later(t, 100)), 1);
        assert_eq!(clicks.press(Button::Left, (0.0, 0.0), later(t, 200)), 1);
        // 4 points away still counts, 5 does not.
        assert_eq!(clicks.press(Button::Left, (4.0, 4.0), later(t, 300)), 2);
        assert_eq!(clicks.press(Button::Left, (9.0, 4.0), later(t, 400)), 1);
        assert_eq!(clicks.press(Button::Left, (9.0, 10.0), later(t, 500)), 1);
    }

    #[test]
    fn a_press_drag_and_release_become_events_in_order() {
        let t = base();
        let mut state = PointerState::default();
        let start = (5.0, 6.0);
        let end = (9.0, 6.0);
        assert_eq!(run(&mut state, 0, start, t), [moved(start, None)]);
        assert!(run(&mut state, 0, start, t).is_empty());
        assert_eq!(
            run(&mut state, 1, start, t),
            [button(start, Button::Left, true, 1)]
        );
        assert_eq!(
            run(&mut state, 1, end, t),
            [moved(end, Some((Button::Left, 1)))]
        );
        assert_eq!(
            run(&mut state, 0, end, t),
            [button(end, Button::Left, false, 1)]
        );
        // The second click of a double click, 100 ms later.
        assert_eq!(
            run(&mut state, 1, end, later(t, 100)),
            [button(end, Button::Left, true, 2)]
        );
        assert_eq!(
            run(&mut state, 0, end, later(t, 150)),
            [button(end, Button::Left, false, 2)]
        );
    }

    #[test]
    fn moving_while_releasing_drags_first_then_lets_go() {
        let t = base();
        let mut state = PointerState::default();
        let start = (1.0, 1.0);
        let end = (50.0, 60.0);
        run(&mut state, 4, start, t);
        assert_eq!(
            run(&mut state, 0, end, t),
            [
                moved(end, Some((Button::Right, 1))),
                button(end, Button::Right, false, 1)
            ]
        );
    }

    #[test]
    fn the_first_message_moves_before_it_presses() {
        let mut state = PointerState::default();
        let point = (3.0, 4.0);
        assert_eq!(
            run(&mut state, 2, point, base()),
            [moved(point, None), button(point, Button::Middle, true, 1)]
        );
    }

    #[test]
    fn releases_come_before_presses_and_drags_follow_the_first_held_button() {
        let t = base();
        let mut state = PointerState::default();
        let point = (3.0, 4.0);
        run(&mut state, 1, point, t);
        // Left goes up while right goes down.
        assert_eq!(
            run(&mut state, 4, point, t),
            [
                button(point, Button::Left, false, 1),
                button(point, Button::Right, true, 1)
            ]
        );
        assert_eq!(Button::held(0b110), Some(Button::Right));
        assert_eq!(Button::held(0b010), Some(Button::Middle));
        assert_eq!(Button::held(0b101), Some(Button::Left));
        assert_eq!(Button::held(0b1000), None);
    }

    #[test]
    fn each_wheel_notch_scrolls_once() {
        let t = base();
        let mut state = PointerState::default();
        let point = (3.0, 4.0);
        run(&mut state, 0, point, t);
        let wheel = |vertical, horizontal| PointerEvent::Wheel {
            vertical,
            horizontal,
        };
        assert_eq!(run(&mut state, WHEEL_UP, point, t), [wheel(3, 0)]);
        // Still down: no second notch.
        assert!(run(&mut state, WHEEL_UP, point, t).is_empty());
        assert!(run(&mut state, 0, point, t).is_empty());
        assert_eq!(run(&mut state, WHEEL_DOWN, point, t), [wheel(-3, 0)]);
        assert!(run(&mut state, 0, point, t).is_empty());
        assert_eq!(run(&mut state, WHEEL_LEFT, point, t), [wheel(0, 3)]);
        assert!(run(&mut state, 0, point, t).is_empty());
        assert_eq!(run(&mut state, WHEEL_RIGHT, point, t), [wheel(0, -3)]);
    }

    #[test]
    fn a_wheel_notch_while_a_button_is_held_leaves_the_button_alone() {
        let t = base();
        let mut state = PointerState::default();
        let point = (3.0, 4.0);
        run(&mut state, 1, point, t);
        assert_eq!(
            run(&mut state, 1 | WHEEL_UP, point, t),
            [PointerEvent::Wheel {
                vertical: 3,
                horizontal: 0
            }]
        );
    }

    #[test]
    fn release_all_lets_go_of_held_buttons_once() {
        let t = base();
        let mut state = PointerState::default();
        let point = (3.0, 4.0);
        run(&mut state, 1 | 4 | WHEEL_UP, point, t);
        let mut events = Vec::new();
        state.release_all(|event| events.push(event));
        assert_eq!(
            events,
            [
                button(point, Button::Left, false, 1),
                button(point, Button::Right, false, 1)
            ]
        );
        let mut again = Vec::new();
        state.release_all(|event| again.push(event));
        assert!(again.is_empty());
        // The next message sees no held buttons.
        assert_eq!(
            run(&mut state, 1, point, later(t, 5000)),
            [button(point, Button::Left, true, 1)]
        );
    }

    #[test]
    fn held_modifiers_keep_left_and_right_apart() {
        let mut held = Held::default();
        held.set(Modifier::Shift, false, true);
        held.set(Modifier::Shift, true, true);
        held.set(Modifier::Command, false, true);
        assert_eq!(held.flags(), Flags::SHIFT | Flags::COMMAND);
        assert_eq!(held.cg_flags(), 0x0002_0000 | 0x0010_0000 | 0x2 | 0x4 | 0x8);
        // The right Shift keeps the flag up.
        held.set(Modifier::Shift, false, false);
        assert_eq!(held.flags(), Flags::SHIFT | Flags::COMMAND);
        assert!(!held.contains(Modifier::Shift, false));
        assert!(held.contains(Modifier::Shift, true));
        held.set(Modifier::Shift, true, false);
        assert_eq!(held.flags(), Flags::COMMAND);
        assert_eq!(held.cg_flags(), 0x0010_0000 | 0x8);
    }

    #[test]
    fn caps_lock_is_never_held() {
        let mut held = Held::default();
        held.set(Modifier::CapsLock, false, true);
        assert_eq!(held, Held::default());
        assert_eq!(held.cg_flags(), 0);
    }

    #[test]
    fn modifier_flags_are_the_cg_event_flags() {
        assert_eq!(flag_bits(Flags::NONE), 0);
        assert_eq!(flag_bits(Flags::SHIFT), 0x0002_0000);
        assert_eq!(flag_bits(Flags::CONTROL), 0x0004_0000);
        assert_eq!(flag_bits(Flags::OPTION), 0x0008_0000);
        assert_eq!(flag_bits(Flags::COMMAND), 0x0010_0000);
        assert_eq!(
            flag_bits(Flags::SHIFT | Flags::CONTROL | Flags::OPTION | Flags::COMMAND),
            0x001E_0000
        );
    }

    #[test]
    fn every_modifier_key_has_its_own_device_bit() {
        let mut seen = 0_u64;
        for (modifier, right) in Held::ALL {
            let bit = device_bit(modifier, right);
            assert_ne!(bit, 0);
            assert_eq!(seen & bit, 0, "{modifier:?} right={right}");
            seen |= bit;
        }
        assert_eq!(device_bit(Modifier::Control, true), 0x2000);
        assert_eq!(device_bit(Modifier::CapsLock, false), 0);
    }
}
