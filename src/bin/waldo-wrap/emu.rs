//! The terminal emulator, behind one module (DESIGN.md section 9.3).
//!
//! Everything that touches libghostty-vt lives here: the hand-written
//! declarations for the C functions used, and a safe wrapper. The rest of the
//! host sees rows of text, a cursor, modes and encoded input, so the core can
//! be replaced without touching it.

use std::collections::hash_map::DefaultHasher;
use std::ffi::c_void;
use std::hash::Hasher;
use std::os::raw::c_int;
use std::ptr;

mod ffi {
    use std::ffi::c_void;
    use std::os::raw::c_int;

    pub type Terminal = *mut c_void;
    pub type TrackedRef = *mut c_void;
    pub type KeyEncoder = *mut c_void;
    pub type KeyEvent = *mut c_void;

    pub const SUCCESS: c_int = 0;
    pub const OUT_OF_SPACE: c_int = -3;

    // GhosttyTerminalOption
    pub const OPT_USERDATA: c_int = 1;
    pub const OPT_WRITE_PTY: c_int = 2;
    pub const OPT_SCROLLBACK_MAX_BYTES: c_int = 28;
    pub const OPT_UNKNOWN_SEQUENCE: c_int = 36;
    pub const OPT_UNKNOWN_MAX_BYTES: c_int = 37;
    pub const OPT_RENDER_HOLD: c_int = 42;
    pub const OPT_SEMANTIC_PROMPT: c_int = 43;

    // GhosttyTerminalData
    pub const DATA_CURSOR_X: c_int = 3;
    pub const DATA_CURSOR_Y: c_int = 4;
    pub const DATA_ACTIVE_SCREEN: c_int = 6;
    pub const DATA_CURSOR_VISIBLE: c_int = 7;
    pub const DATA_KITTY_KEYBOARD_FLAGS: c_int = 8;
    pub const DATA_TITLE: c_int = 12;
    pub const DATA_SCROLLBACK_ROWS: c_int = 15;
    pub const DATA_MODE: c_int = 37;

    pub const SCREEN_ALTERNATE: c_int = 1;

    pub const BOUNDARY_OSC_END: u32 = 1 << 6;

    pub const UNKNOWN_SEQUENCE_OSC: c_int = 1;
    pub const SEMANTIC_PROMPT_PROMPT_START: c_int = 1;

    pub const CELL_DATA_CONTENT_TAG: c_int = 2;
    pub const CELL_DATA_WIDE: c_int = 3;
    pub const CELL_DATA_HAS_STYLING: c_int = 5;
    pub const CELL_DATA_COLOR_PALETTE: c_int = 10;
    pub const CELL_DATA_COLOR_RGB: c_int = 11;
    pub const CELL_CONTENT_BG_COLOR_PALETTE: c_int = 2;
    pub const CELL_CONTENT_BG_COLOR_RGB: c_int = 3;
    pub const CELL_DATA_HAS_TEXT: c_int = 4;
    pub const CELL_WIDE_SPACER_TAIL: c_int = 2;
    pub const CELL_WIDE_SPACER_HEAD: c_int = 3;

    pub const ROW_DATA_WRAP: c_int = 1;

    pub const KEY_ACTION_PRESS: c_int = 1;

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct GString {
        pub ptr: *const u8,
        pub len: usize,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct PointCoordinate {
        pub x: u16,
        pub y: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub union PointValue {
        pub coordinate: PointCoordinate,
        pub _padding: [u64; 2],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Point {
        pub tag: c_int,
        pub value: PointValue,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct StyleColor {
        pub tag: u8,
        pub value: [u8; 3],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct Style {
        pub fg_color: StyleColor,
        pub bg_color: StyleColor,
        pub underline_color: StyleColor,
        pub flags: u16,
        pub reserved: u16,
    }

    #[repr(C)]
    pub struct GridRef {
        pub node: *mut c_void,
        pub x: u16,
        pub y: u16,
    }

    #[repr(C)]
    pub struct ModeConfig {
        pub mode: u16,
        pub value: bool,
    }

    #[repr(C)]
    pub struct WriteStop {
        pub size: usize,
        pub consumed: usize,
        pub reached: u32,
        pub kind: c_int,
        pub introducer: c_int,
        pub end: c_int,
        pub byte: u8,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct UnknownString {
        pub truncated: bool,
        pub content: GString,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct UnknownOsc {
        pub truncated: bool,
        pub content: GString,
        pub terminator: c_int,
    }

    #[repr(C)]
    pub union UnknownValue {
        pub apc: UnknownString,
        pub osc: UnknownOsc,
        pub _padding: [u64; 16],
    }

    #[repr(C)]
    pub struct UnknownSequence {
        pub tag: c_int,
        pub value: UnknownValue,
    }

    #[repr(C)]
    pub struct SemanticPrompt {
        pub kind: c_int,
        pub prompt_kind: c_int,
        pub has_exit_code: bool,
        pub exit_code: i32,
        pub command: GString,
        pub error: GString,
    }

    #[link(name = "ghostty-vt")]
    extern "C" {
        pub fn ghostty_terminal_new(
            allocator: *const c_void,
            terminal: *mut Terminal,
            cols: u16,
            rows: u16,
        ) -> c_int;
        pub fn ghostty_terminal_free(terminal: Terminal);
        pub fn ghostty_terminal_resize(
            terminal: Terminal,
            cols: u16,
            rows: u16,
            cell_width_px: u32,
            cell_height_px: u32,
        ) -> c_int;
        pub fn ghostty_terminal_set(terminal: Terminal, option: c_int, value: *const c_void)
            -> c_int;
        pub fn ghostty_terminal_get(terminal: Terminal, data: c_int, out: *mut c_void) -> c_int;
        pub fn ghostty_terminal_vt_write(terminal: Terminal, data: *const u8, len: usize);
        pub fn ghostty_terminal_vt_write_until(
            terminal: Terminal,
            data: *const u8,
            len: usize,
            boundaries: u32,
            out: *mut WriteStop,
        ) -> c_int;
        pub fn ghostty_terminal_grid_ref(
            terminal: Terminal,
            point: Point,
            out_ref: *mut GridRef,
        ) -> c_int;
        pub fn ghostty_terminal_grid_ref_track(
            terminal: Terminal,
            point: Point,
            out_ref: *mut TrackedRef,
        ) -> c_int;

        pub fn ghostty_tracked_grid_ref_free(r: TrackedRef);
        pub fn ghostty_tracked_grid_ref_has_value(r: TrackedRef) -> bool;
        pub fn ghostty_tracked_grid_ref_point(
            r: TrackedRef,
            tag: c_int,
            out_point: *mut PointCoordinate,
        ) -> c_int;
        pub fn ghostty_tracked_grid_ref_set(r: TrackedRef, terminal: Terminal, point: Point)
            -> c_int;

        pub fn ghostty_grid_ref_cell(r: *const GridRef, out_cell: *mut u64) -> c_int;
        pub fn ghostty_grid_ref_style(r: *const GridRef, out_style: *mut Style) -> c_int;
        pub fn ghostty_grid_ref_row(r: *const GridRef, out_row: *mut u64) -> c_int;
        pub fn ghostty_grid_ref_graphemes(
            r: *const GridRef,
            buf: *mut u32,
            buf_len: usize,
            out_written: *mut usize,
        ) -> c_int;
        pub fn ghostty_cell_get(cell: u64, data: c_int, out: *mut c_void) -> c_int;
        pub fn ghostty_row_get(row: u64, data: c_int, out: *mut c_void) -> c_int;

        pub fn ghostty_key_encoder_new(allocator: *const c_void, encoder: *mut KeyEncoder)
            -> c_int;
        pub fn ghostty_key_encoder_free(encoder: KeyEncoder);
        pub fn ghostty_key_encoder_set_from_terminal(
            encoder: KeyEncoder,
            terminal: Terminal,
        ) -> c_int;
        pub fn ghostty_key_encoder_encode(
            encoder: KeyEncoder,
            event: KeyEvent,
            buf: *mut u8,
            buf_len: usize,
            out_written: *mut usize,
        ) -> c_int;
        pub fn ghostty_key_event_new(allocator: *const c_void, event: *mut KeyEvent) -> c_int;
        pub fn ghostty_key_event_free(event: KeyEvent);
        pub fn ghostty_key_event_set_action(event: KeyEvent, action: c_int) -> c_int;
        pub fn ghostty_key_event_set_key(event: KeyEvent, key: c_int) -> c_int;
        pub fn ghostty_key_event_set_mods(event: KeyEvent, mods: u16) -> c_int;
        pub fn ghostty_key_event_set_utf8(event: KeyEvent, utf8: GString) -> c_int;
        pub fn ghostty_key_event_set_unshifted_codepoint(event: KeyEvent, cp: u32) -> c_int;

        pub fn ghostty_paste_encode(
            data: *mut u8,
            data_len: usize,
            bracketed: bool,
            buf: *mut u8,
            buf_len: usize,
            out_written: *mut usize,
        ) -> c_int;
    }
}

/// Which coordinate space a row number is in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Space {
    /// The visible screen: row 0 is its top line.
    Active,
    /// History and screen together: row 0 is the oldest line still held.
    Screen,
}

impl Space {
    fn tag(self) -> c_int {
        match self {
            Space::Active => 0,
            Space::Screen => 2,
        }
    }
}

pub const MOD_SHIFT: u16 = 1 << 0;
pub const MOD_CTRL: u16 = 1 << 1;
pub const MOD_ALT: u16 = 1 << 2;

/// GhosttyKey values for the keys waldo can name.
pub mod key {
    pub const BACKQUOTE: i32 = 1;
    pub const BACKSLASH: i32 = 2;
    pub const BRACKET_LEFT: i32 = 3;
    pub const BRACKET_RIGHT: i32 = 4;
    pub const COMMA: i32 = 5;
    pub const DIGIT_0: i32 = 6;
    pub const EQUAL: i32 = 16;
    pub const A: i32 = 20;
    pub const MINUS: i32 = 46;
    pub const PERIOD: i32 = 47;
    pub const QUOTE: i32 = 48;
    pub const SEMICOLON: i32 = 49;
    pub const SLASH: i32 = 50;
    pub const BACKSPACE: i32 = 53;
    pub const ENTER: i32 = 58;
    pub const SPACE: i32 = 63;
    pub const TAB: i32 = 64;
    pub const DELETE: i32 = 68;
    pub const END: i32 = 69;
    pub const HOME: i32 = 71;
    pub const INSERT: i32 = 72;
    pub const PAGE_DOWN: i32 = 73;
    pub const PAGE_UP: i32 = 74;
    pub const ARROW_DOWN: i32 = 75;
    pub const ARROW_LEFT: i32 = 76;
    pub const ARROW_RIGHT: i32 = 77;
    pub const ARROW_UP: i32 = 78;
    pub const ESCAPE: i32 = 120;
    pub const F1: i32 = 121;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Color {
    #[default]
    None,
    Palette(u8),
    Rgb(u8, u8, u8),
}

/// How a cell is drawn: colours and attributes.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub faint: bool,
    pub italic: bool,
    pub underline: bool,
    pub blink: bool,
    pub inverse: bool,
    pub invisible: bool,
    pub strikethrough: bool,
}

impl Style {
    fn from_raw(raw: &ffi::Style) -> Style {
        let color = |c: &ffi::StyleColor| match c.tag {
            1 => Color::Palette(c.value[0]),
            2 => Color::Rgb(c.value[0], c.value[1], c.value[2]),
            _ => Color::None,
        };
        let f = raw.flags;
        Style {
            fg: color(&raw.fg_color),
            bg: color(&raw.bg_color),
            bold: f & 1 != 0,
            italic: f & (1 << 1) != 0,
            faint: f & (1 << 2) != 0,
            blink: f & (1 << 3) != 0,
            inverse: f & (1 << 4) != 0,
            invisible: f & (1 << 5) != 0,
            strikethrough: f & (1 << 6) != 0,
            underline: (f >> 8) & 7 != 0,
        }
    }

    /// The SGR sequence that sets exactly this style, starting from a reset.
    pub fn sgr(&self) -> String {
        let mut p: Vec<String> = vec!["0".into()];
        for (on, code) in [
            (self.bold, "1"),
            (self.faint, "2"),
            (self.italic, "3"),
            (self.underline, "4"),
            (self.blink, "5"),
            (self.inverse, "7"),
            (self.invisible, "8"),
            (self.strikethrough, "9"),
        ] {
            if on {
                p.push(code.into());
            }
        }
        let color = |c: Color, base: u8| -> Option<String> {
            match c {
                Color::None => None,
                Color::Palette(i) if i < 8 => Some(format!("{}", base + i)),
                Color::Palette(i) if i < 16 => Some(format!("{}", base + 60 + i - 8)),
                Color::Palette(i) => Some(format!("{};5;{}", base + 8, i)),
                Color::Rgb(r, g, b) => Some(format!("{};2;{};{};{}", base + 8, r, g, b)),
            }
        };
        p.extend(color(self.fg, 30));
        p.extend(color(self.bg, 40));
        format!("\x1b[{}m", p.join(";"))
    }
}

/// One row of the grid as text.
#[derive(Clone, Debug, Default)]
pub struct Row {
    /// Text up to the last cell that holds a character. Spaces a program
    /// wrote are kept; cells it never wrote are not.
    pub text: String,
    /// This row continues on the next one (soft wrap).
    pub wrap: bool,
}

/// Things the emulator reported while it was fed bytes.
#[derive(Default)]
struct Events {
    replies: Vec<u8>,
    unknown_osc: Vec<Vec<u8>>,
    hold: bool,
    prompt_start: bool,
}

pub struct Emu {
    t: ffi::Terminal,
    events: *mut Events,
    encoder: ffi::KeyEncoder,
    event: ffi::KeyEvent,
    cols: u16,
    rows: u16,
}

/// A reference to a row that the emulator keeps pointing at the same line
/// while the grid scrolls and reflows.
pub struct Tracked(ffi::TrackedRef);

impl Drop for Tracked {
    fn drop(&mut self) {
        unsafe { ffi::ghostty_tracked_grid_ref_free(self.0) }
    }
}

unsafe extern "C" fn on_write_pty(_t: ffi::Terminal, ud: *mut c_void, data: *const u8, len: usize) {
    let ev = &mut *(ud as *mut Events);
    if !data.is_null() && len > 0 {
        ev.replies.extend_from_slice(std::slice::from_raw_parts(data, len));
    }
}

unsafe extern "C" fn on_unknown(
    _t: ffi::Terminal,
    ud: *mut c_void,
    seq: *const ffi::UnknownSequence,
) {
    let ev = &mut *(ud as *mut Events);
    if seq.is_null() || (*seq).tag != ffi::UNKNOWN_SEQUENCE_OSC {
        return;
    }
    let osc = (*seq).value.osc;
    if osc.truncated || osc.content.ptr.is_null() {
        return;
    }
    let bytes = std::slice::from_raw_parts(osc.content.ptr, osc.content.len);
    if ev.unknown_osc.len() < 64 {
        ev.unknown_osc.push(bytes.to_vec());
    }
}

unsafe extern "C" fn on_render_hold(_t: ffi::Terminal, ud: *mut c_void, held: bool) {
    let ev = &mut *(ud as *mut Events);
    ev.hold = held;
}

unsafe extern "C" fn on_semantic_prompt(
    _t: ffi::Terminal,
    ud: *mut c_void,
    event: *const ffi::SemanticPrompt,
) {
    let ev = &mut *(ud as *mut Events);
    if !event.is_null() && (*event).kind == ffi::SEMANTIC_PROMPT_PROMPT_START {
        ev.prompt_start = true;
    }
}

impl Emu {
    pub fn new(cols: u16, rows: u16, scrollback_bytes: usize) -> Result<Emu, String> {
        unsafe {
            let mut t: ffi::Terminal = ptr::null_mut();
            if ffi::ghostty_terminal_new(ptr::null(), &mut t, cols, rows) != ffi::SUCCESS {
                return Err("could not create the terminal emulator".into());
            }
            let events = Box::into_raw(Box::new(Events::default()));
            let set = |opt: c_int, value: *const c_void| -> Result<(), String> {
                if ffi::ghostty_terminal_set(t, opt, value) == ffi::SUCCESS {
                    Ok(())
                } else {
                    Err(format!("the terminal emulator refused option {opt}"))
                }
            };
            set(ffi::OPT_USERDATA, events as *const c_void)?;
            set(ffi::OPT_WRITE_PTY, on_write_pty as *const c_void)?;
            set(ffi::OPT_SCROLLBACK_MAX_BYTES, &scrollback_bytes as *const usize as *const c_void)?;
            let unknown_max: usize = 2048;
            set(ffi::OPT_UNKNOWN_MAX_BYTES, &unknown_max as *const usize as *const c_void)?;
            set(ffi::OPT_UNKNOWN_SEQUENCE, on_unknown as *const c_void)?;
            set(ffi::OPT_RENDER_HOLD, on_render_hold as *const c_void)?;
            set(ffi::OPT_SEMANTIC_PROMPT, on_semantic_prompt as *const c_void)?;

            let mut encoder: ffi::KeyEncoder = ptr::null_mut();
            let mut event: ffi::KeyEvent = ptr::null_mut();
            if ffi::ghostty_key_encoder_new(ptr::null(), &mut encoder) != ffi::SUCCESS
                || ffi::ghostty_key_event_new(ptr::null(), &mut event) != ffi::SUCCESS
            {
                return Err("could not create the key encoder".into());
            }
            Ok(Emu { t, events, encoder, event, cols, rows })
        }
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }

    pub fn rows(&self) -> u16 {
        self.rows
    }

    /// Feed program output to the emulator.
    pub fn feed(&mut self, data: &[u8]) {
        unsafe { ffi::ghostty_terminal_vt_write(self.t, data.as_ptr(), data.len()) }
    }

    /// Feed program output, stopping after the first byte that ends an OSC
    /// sequence. Returns how many bytes were consumed and whether it stopped
    /// at such an end.
    pub fn feed_until_osc_end(&mut self, data: &[u8]) -> (usize, bool) {
        if data.is_empty() {
            return (0, false);
        }
        let mut stop: ffi::WriteStop = unsafe { std::mem::zeroed() };
        stop.size = std::mem::size_of::<ffi::WriteStop>();
        let r = unsafe {
            ffi::ghostty_terminal_vt_write_until(
                self.t,
                data.as_ptr(),
                data.len(),
                ffi::BOUNDARY_OSC_END,
                &mut stop,
            )
        };
        if r == ffi::SUCCESS {
            (stop.consumed.max(1).min(data.len()), true)
        } else if stop.consumed == data.len() {
            (data.len(), false)
        } else {
            // Not expected. Fall back to a plain write so no byte is lost.
            self.feed(data);
            (data.len(), false)
        }
    }

    fn events(&mut self) -> &mut Events {
        unsafe { &mut *self.events }
    }

    /// Answers to terminal queries, to be written to the pty.
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.events().replies)
    }

    /// OSC sequences the emulator does not implement, without the leading
    /// `ESC ]` and the terminator.
    pub fn take_unknown_osc(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.events().unknown_osc)
    }

    /// Whether the program has a synchronized-output hold open (mode 2026).
    pub fn hold(&mut self) -> bool {
        self.events().hold
    }

    /// Whether a shell prompt-start mark (OSC 133) arrived since last asked.
    pub fn take_prompt_start(&mut self) -> bool {
        std::mem::take(&mut self.events().prompt_start)
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<(), String> {
        let r = unsafe { ffi::ghostty_terminal_resize(self.t, cols, rows, 0, 0) };
        if r != ffi::SUCCESS {
            return Err(format!("the terminal emulator refused the size {cols}x{rows}"));
        }
        self.cols = cols;
        self.rows = rows;
        Ok(())
    }

    fn get<T: Default>(&self, data: c_int) -> T {
        let mut out = T::default();
        unsafe { ffi::ghostty_terminal_get(self.t, data, &mut out as *mut T as *mut c_void) };
        out
    }

    /// Cursor column and row on the visible screen, 0-based.
    pub fn cursor(&self) -> (u16, u16) {
        (self.get::<u16>(ffi::DATA_CURSOR_X), self.get::<u16>(ffi::DATA_CURSOR_Y))
    }

    pub fn cursor_visible(&self) -> bool {
        self.get::<bool>(ffi::DATA_CURSOR_VISIBLE)
    }

    pub fn alt_screen(&self) -> bool {
        self.get::<c_int>(ffi::DATA_ACTIVE_SCREEN) == ffi::SCREEN_ALTERNATE
    }

    pub fn kitty_keyboard_flags(&self) -> u8 {
        self.get::<u8>(ffi::DATA_KITTY_KEYBOARD_FLAGS)
    }

    /// Rows of history above the visible screen.
    pub fn scrollback_rows(&self) -> usize {
        self.get::<usize>(ffi::DATA_SCROLLBACK_ROWS)
    }

    pub fn title(&self) -> String {
        let mut s = ffi::GString { ptr: ptr::null(), len: 0 };
        unsafe {
            ffi::ghostty_terminal_get(self.t, ffi::DATA_TITLE, &mut s as *mut _ as *mut c_void);
            if s.ptr.is_null() || s.len == 0 {
                return String::new();
            }
            String::from_utf8_lossy(std::slice::from_raw_parts(s.ptr, s.len)).into_owned()
        }
    }

    /// The value of a DEC private mode, such as 2004 for bracketed paste.
    pub fn dec_mode(&self, mode: u16) -> bool {
        let mut cfg = ffi::ModeConfig { mode: mode & 0x7fff, value: false };
        unsafe {
            ffi::ghostty_terminal_get(self.t, ffi::DATA_MODE, &mut cfg as *mut _ as *mut c_void);
        }
        cfg.value
    }

    fn point(space: Space, x: u16, y: u32) -> ffi::Point {
        let mut value = ffi::PointValue { _padding: [0; 2] };
        value.coordinate = ffi::PointCoordinate { x, y };
        ffi::Point { tag: space.tag(), value }
    }

    /// Read one row. `None` if the row does not exist. If `hasher` is given,
    /// everything that affects how the row looks is fed to it.
    pub fn row(&self, space: Space, y: u32, mut hasher: Option<&mut DefaultHasher>) -> Option<Row> {
        let mut text = String::new();
        let mut end = 0usize;
        let mut wrap = false;
        for x in 0..self.cols {
            let mut gref = ffi::GridRef { node: ptr::null_mut(), x: 0, y: 0 };
            let r = unsafe { ffi::ghostty_terminal_grid_ref(self.t, Self::point(space, x, y), &mut gref) };
            if r != ffi::SUCCESS || gref.node.is_null() {
                if x == 0 {
                    return None;
                }
                break;
            }
            unsafe {
                if x == 0 {
                    let mut row: u64 = 0;
                    if ffi::ghostty_grid_ref_row(&gref, &mut row) == ffi::SUCCESS {
                        ffi::ghostty_row_get(row, ffi::ROW_DATA_WRAP, &mut wrap as *mut bool as *mut c_void);
                    }
                }
                let mut cell: u64 = 0;
                if ffi::ghostty_grid_ref_cell(&gref, &mut cell) != ffi::SUCCESS {
                    continue;
                }
                if let Some(h) = hasher.as_deref_mut() {
                    h.write_u64(cell);
                }
                let mut wide: c_int = 0;
                ffi::ghostty_cell_get(cell, ffi::CELL_DATA_WIDE, &mut wide as *mut c_int as *mut c_void);
                if wide == ffi::CELL_WIDE_SPACER_TAIL || wide == ffi::CELL_WIDE_SPACER_HEAD {
                    continue;
                }
                let mut has_text = false;
                ffi::ghostty_cell_get(cell, ffi::CELL_DATA_HAS_TEXT, &mut has_text as *mut bool as *mut c_void);
                if !has_text {
                    text.push(' ');
                    continue;
                }
                let mut stack = [0u32; 8];
                let mut n: usize = 0;
                let r = ffi::ghostty_grid_ref_graphemes(&gref, stack.as_mut_ptr(), stack.len(), &mut n);
                let mut heap: Vec<u32> = Vec::new();
                let cps: &[u32] = if r == ffi::SUCCESS {
                    &stack[..n.min(stack.len())]
                } else if r == ffi::OUT_OF_SPACE {
                    heap.resize(n, 0);
                    let mut m: usize = 0;
                    if ffi::ghostty_grid_ref_graphemes(&gref, heap.as_mut_ptr(), heap.len(), &mut m) == ffi::SUCCESS {
                        &heap[..m.min(heap.len())]
                    } else {
                        &[]
                    }
                } else {
                    &[]
                };
                for &cp in cps {
                    if let Some(h) = hasher.as_deref_mut() {
                        h.write_u32(cp);
                    }
                    text.push(char::from_u32(cp).filter(|c| !c.is_control()).unwrap_or('\u{fffd}'));
                }
                end = text.len();
            }
        }
        text.truncate(end);
        Some(Row { text, wrap })
    }

    /// Read one row as text with SGR colour and attribute sequences where
    /// the style changes, and a reset at the end if any style was set. No
    /// other escape sequences are produced. `x0` and `max_cols` select a
    /// horizontal slice in cells. Trailing cells that are blank and unstyled
    /// are dropped.
    pub fn row_ansi(&self, space: Space, y: u32, x0: u16, max_cols: u16) -> Option<String> {
        let mut out = String::new();
        let mut end = 0usize;
        let mut current = Style::default();
        let last = self.cols.min(x0.saturating_add(max_cols));
        for x in x0..last {
            let mut gref = ffi::GridRef { node: ptr::null_mut(), x: 0, y: 0 };
            let r = unsafe { ffi::ghostty_terminal_grid_ref(self.t, Self::point(space, x, y), &mut gref) };
            if r != ffi::SUCCESS || gref.node.is_null() {
                if x == x0 {
                    return None;
                }
                break;
            }
            unsafe {
                let mut cell: u64 = 0;
                if ffi::ghostty_grid_ref_cell(&gref, &mut cell) != ffi::SUCCESS {
                    continue;
                }
                let mut wide: c_int = 0;
                ffi::ghostty_cell_get(cell, ffi::CELL_DATA_WIDE, &mut wide as *mut c_int as *mut c_void);
                if wide == ffi::CELL_WIDE_SPACER_TAIL || wide == ffi::CELL_WIDE_SPACER_HEAD {
                    continue;
                }
                let mut style = Style::default();
                let mut styled = false;
                ffi::ghostty_cell_get(cell, ffi::CELL_DATA_HAS_STYLING, &mut styled as *mut bool as *mut c_void);
                if styled {
                    let mut raw: ffi::Style = std::mem::zeroed();
                    if ffi::ghostty_grid_ref_style(&gref, &mut raw) == ffi::SUCCESS {
                        style = Style::from_raw(&raw);
                    }
                }
                // A cell can also be nothing but a background colour.
                let mut tag: c_int = 0;
                ffi::ghostty_cell_get(cell, ffi::CELL_DATA_CONTENT_TAG, &mut tag as *mut c_int as *mut c_void);
                if tag == ffi::CELL_CONTENT_BG_COLOR_PALETTE {
                    let mut idx: u8 = 0;
                    ffi::ghostty_cell_get(cell, ffi::CELL_DATA_COLOR_PALETTE, &mut idx as *mut u8 as *mut c_void);
                    style.bg = Color::Palette(idx);
                } else if tag == ffi::CELL_CONTENT_BG_COLOR_RGB {
                    let mut rgb = [0u8; 3];
                    ffi::ghostty_cell_get(cell, ffi::CELL_DATA_COLOR_RGB, rgb.as_mut_ptr() as *mut c_void);
                    style.bg = Color::Rgb(rgb[0], rgb[1], rgb[2]);
                }
                if style != current {
                    out.push_str(&style.sgr());
                    current = style;
                }
                let mut has_text = false;
                ffi::ghostty_cell_get(cell, ffi::CELL_DATA_HAS_TEXT, &mut has_text as *mut bool as *mut c_void);
                if !has_text {
                    out.push(' ');
                    if style != Style::default() {
                        end = out.len();
                    }
                    continue;
                }
                let mut stack = [0u32; 8];
                let mut n: usize = 0;
                let r = ffi::ghostty_grid_ref_graphemes(&gref, stack.as_mut_ptr(), stack.len(), &mut n);
                let mut heap: Vec<u32> = Vec::new();
                let cps: &[u32] = if r == ffi::SUCCESS {
                    &stack[..n.min(stack.len())]
                } else if r == ffi::OUT_OF_SPACE {
                    heap.resize(n, 0);
                    let mut m: usize = 0;
                    if ffi::ghostty_grid_ref_graphemes(&gref, heap.as_mut_ptr(), heap.len(), &mut m) == ffi::SUCCESS {
                        &heap[..m.min(heap.len())]
                    } else {
                        &[]
                    }
                } else {
                    &[]
                };
                for &cp in cps {
                    out.push(char::from_u32(cp).filter(|c| !c.is_control()).unwrap_or('\u{fffd}'));
                }
                // A written space in the default style is still trailing
                // blank; anything else is content.
                if style != Style::default() || cps.iter().any(|&cp| cp != b' ' as u32) {
                    end = out.len();
                }
            }
        }
        out.truncate(end);
        if out.contains('\x1b') && !out.ends_with("\x1b[0m") {
            out.push_str("\x1b[0m");
        }
        Some(out)
    }

    /// Start tracking a row.
    pub fn track(&self, space: Space, y: u32) -> Option<Tracked> {
        let mut r: ffi::TrackedRef = ptr::null_mut();
        let res = unsafe { ffi::ghostty_terminal_grid_ref_track(self.t, Self::point(space, 0, y), &mut r) };
        if res == ffi::SUCCESS && !r.is_null() {
            Some(Tracked(r))
        } else {
            None
        }
    }

    /// Where a tracked row is now, in `Space::Screen` rows. `None` once the
    /// row has been discarded from history.
    pub fn tracked_row(&self, tracked: &Tracked) -> Option<u32> {
        unsafe {
            if !ffi::ghostty_tracked_grid_ref_has_value(tracked.0) {
                return None;
            }
            let mut p = ffi::PointCoordinate::default();
            if ffi::ghostty_tracked_grid_ref_point(tracked.0, Space::Screen.tag(), &mut p) == ffi::SUCCESS {
                Some(p.y)
            } else {
                None
            }
        }
    }

    /// Point a tracked reference at another row.
    pub fn tracked_set(&self, tracked: &Tracked, space: Space, y: u32) -> bool {
        unsafe { ffi::ghostty_tracked_grid_ref_set(tracked.0, self.t, Self::point(space, 0, y)) == ffi::SUCCESS }
    }

    /// Encode a key press the way the program currently expects it. `text`
    /// is what the key produces with no modifier held, for keys that produce
    /// text. Returns no bytes if the emulator has no encoding for the key.
    pub fn encode_key(&mut self, key: i32, mods: u16, text: Option<char>) -> Vec<u8> {
        let mut utf8 = [0u8; 4];
        let mut out = [0u8; 128];
        let mut n: usize = 0;
        unsafe {
            ffi::ghostty_key_encoder_set_from_terminal(self.encoder, self.t);
            ffi::ghostty_key_event_set_action(self.event, ffi::KEY_ACTION_PRESS);
            ffi::ghostty_key_event_set_key(self.event, key);
            ffi::ghostty_key_event_set_mods(self.event, mods);
            match text {
                Some(c) => {
                    let s = c.encode_utf8(&mut utf8);
                    ffi::ghostty_key_event_set_utf8(self.event, ffi::GString { ptr: s.as_ptr(), len: s.len() });
                    ffi::ghostty_key_event_set_unshifted_codepoint(self.event, c as u32);
                }
                None => {
                    ffi::ghostty_key_event_set_utf8(self.event, ffi::GString { ptr: ptr::null(), len: 0 });
                    ffi::ghostty_key_event_set_unshifted_codepoint(self.event, 0);
                }
            }
            let r = ffi::ghostty_key_encoder_encode(self.encoder, self.event, out.as_mut_ptr(), out.len(), &mut n);
            // The event borrows `utf8`; clear it before the buffer goes away.
            ffi::ghostty_key_event_set_utf8(self.event, ffi::GString { ptr: ptr::null(), len: 0 });
            if r != ffi::SUCCESS {
                return Vec::new();
            }
        }
        out[..n.min(out.len())].to_vec()
    }

    /// Encode pasted text: bracketed if the program enabled mode 2004, and
    /// made safe so it cannot end the bracket early.
    pub fn encode_paste(&self, data: &[u8]) -> Vec<u8> {
        let bracketed = self.dec_mode(2004);
        let mut input = data.to_vec();
        let mut out = vec![0u8; input.len() + 16];
        let mut n: usize = 0;
        unsafe {
            let mut r = ffi::ghostty_paste_encode(input.as_mut_ptr(), input.len(), bracketed, out.as_mut_ptr(), out.len(), &mut n);
            if r == ffi::OUT_OF_SPACE {
                out.resize(n, 0);
                r = ffi::ghostty_paste_encode(input.as_mut_ptr(), input.len(), bracketed, out.as_mut_ptr(), out.len(), &mut n);
            }
            if r != ffi::SUCCESS {
                return Vec::new();
            }
        }
        out.truncate(n);
        out
    }
}

impl Drop for Emu {
    fn drop(&mut self) {
        unsafe {
            ffi::ghostty_key_event_free(self.event);
            ffi::ghostty_key_encoder_free(self.encoder);
            ffi::ghostty_terminal_free(self.t);
            drop(Box::from_raw(self.events));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emu() -> Emu {
        Emu::new(80, 24, 4 << 20).unwrap()
    }

    #[test]
    fn rows_and_cursor() {
        let mut e = emu();
        e.feed(b"hello\r\nPassword: ");
        assert_eq!(e.row(Space::Active, 0, None).unwrap().text, "hello");
        assert_eq!(e.row(Space::Active, 1, None).unwrap().text, "Password: ");
        assert_eq!(e.row(Space::Active, 2, None).unwrap().text, "");
        assert_eq!(e.cursor(), (10, 1));
        assert!(e.cursor_visible());
        assert!(!e.alt_screen());
        assert!(e.row(Space::Active, 24, None).is_none());
    }

    #[test]
    fn wide_and_combining_characters() {
        let mut e = emu();
        e.feed("a\u{4e16}b e\u{301}".as_bytes());
        assert_eq!(e.row(Space::Active, 0, None).unwrap().text, "a\u{4e16}b e\u{301}");
        assert_eq!(e.cursor().0, 6);
    }

    #[test]
    fn soft_wrap_flag() {
        let mut e = Emu::new(10, 5, 1 << 20).unwrap();
        e.feed(b"0123456789abc\r\nnext");
        let r0 = e.row(Space::Active, 0, None).unwrap();
        let r1 = e.row(Space::Active, 1, None).unwrap();
        assert!(r0.wrap);
        assert!(!r1.wrap);
        assert_eq!(format!("{}{}", r0.text, r1.text), "0123456789abc");
    }

    #[test]
    fn answers_queries() {
        let mut e = emu();
        e.feed(b"\x1b[c");
        let da1 = e.take_replies();
        assert!(da1.starts_with(b"\x1b[?"), "DA1 reply was {:?}", String::from_utf8_lossy(&da1));
        e.feed(b"\x1b[5n");
        assert_eq!(e.take_replies(), b"\x1b[0n");
        e.feed(b"ab\x1b[6n");
        assert_eq!(e.take_replies(), b"\x1b[1;3R");
    }

    #[test]
    fn private_osc_reaches_the_host() {
        let mut e = emu();
        e.feed(b"before\x1b]7770;waldo;abc123;2\x07after");
        assert_eq!(e.take_unknown_osc(), vec![b"7770;waldo;abc123;2".to_vec()]);
        // It draws nothing.
        assert_eq!(e.row(Space::Active, 0, None).unwrap().text, "beforeafter");
        // The ST terminator works too.
        e.feed(b"\x1b]7770;x\x1b\\");
        assert_eq!(e.take_unknown_osc(), vec![b"7770;x".to_vec()]);
    }

    #[test]
    fn feed_stops_at_osc_end() {
        let mut e = emu();
        let data = b"one\x1b]7770;n;0\x07two";
        let (n, stopped) = e.feed_until_osc_end(data);
        assert!(stopped);
        assert_eq!(&data[n..], b"two");
        assert_eq!(e.take_unknown_osc().len(), 1);
        assert_eq!(e.row(Space::Active, 0, None).unwrap().text, "one");
        let (n2, stopped2) = e.feed_until_osc_end(&data[n..]);
        assert_eq!((n2, stopped2), (3, false));
        assert_eq!(e.row(Space::Active, 0, None).unwrap().text, "onetwo");
    }

    #[test]
    fn synchronized_output_hold() {
        let mut e = emu();
        assert!(!e.hold());
        e.feed(b"\x1b[?2026h");
        assert!(e.hold());
        e.feed(b"\x1b[?2026l");
        assert!(!e.hold());
    }

    #[test]
    fn prompt_start_mark() {
        let mut e = emu();
        assert!(!e.take_prompt_start());
        e.feed(b"\x1b]133;A\x07$ ");
        assert!(e.take_prompt_start());
        assert!(!e.take_prompt_start());
    }

    #[test]
    fn title_and_alt_screen() {
        let mut e = emu();
        e.feed(b"\x1b]2;my title\x07\x1b[?1049h");
        assert_eq!(e.title(), "my title");
        assert!(e.alt_screen());
        e.feed(b"\x1b[?1049l");
        assert!(!e.alt_screen());
    }

    #[test]
    fn key_encoding() {
        let mut e = emu();
        assert_eq!(e.encode_key(key::ESCAPE, 0, None), b"\x1b");
        assert_eq!(e.encode_key(key::ENTER, 0, None), b"\r");
        assert_eq!(e.encode_key(key::TAB, 0, None), b"\t");
        assert_eq!(e.encode_key(key::TAB, MOD_SHIFT, None), b"\x1b[Z");
        assert_eq!(e.encode_key(key::BACKSPACE, 0, None), b"\x7f");
        assert_eq!(e.encode_key(key::ARROW_UP, 0, None), b"\x1b[A");
        assert_eq!(e.encode_key(key::ARROW_LEFT, MOD_CTRL | MOD_SHIFT, None), b"\x1b[1;6D");
        assert_eq!(e.encode_key(key::DELETE, 0, None), b"\x1b[3~");
        assert_eq!(e.encode_key(key::PAGE_DOWN, 0, None), b"\x1b[6~");
        assert_eq!(e.encode_key(key::F1, 0, None), b"\x1bOP");
        assert_eq!(e.encode_key(key::F1 + 4, 0, None), b"\x1b[15~");
        assert_eq!(e.encode_key(key::A + 2, MOD_CTRL, Some('c')), b"\x03");
        assert_eq!(e.encode_key(key::A + 23, MOD_ALT, Some('x')), b"\x1bx");
        // Application cursor keys change the arrows.
        e.feed(b"\x1b[?1h");
        assert_eq!(e.encode_key(key::ARROW_UP, 0, None), b"\x1bOA");
    }

    #[test]
    fn paste_encoding() {
        let mut e = emu();
        assert_eq!(e.encode_paste(b"a\nb"), b"a\rb");
        e.feed(b"\x1b[?2004h");
        assert!(e.dec_mode(2004));
        assert_eq!(e.encode_paste(b"a\nb"), b"\x1b[200~a\nb\x1b[201~");
        // A paste cannot close its own bracket.
        let evil = e.encode_paste(b"x\x1b[201~rm -rf");
        let inner = &evil[6..evil.len() - 6];
        assert!(!inner.windows(6).any(|w| w == b"\x1b[201~"), "{:?}", String::from_utf8_lossy(&evil));
    }

    #[test]
    fn styled_rows() {
        let mut e = emu();
        // Plain text has no escapes at all.
        e.feed(b"plain\r\n");
        assert_eq!(e.row_ansi(Space::Active, 0, 0, 80).unwrap(), "plain");
        // Colour and attributes, each run introduced by a full SGR.
        e.feed(b"a \x1b[1;31mred\x1b[0m b \x1b[7msel\x1b[0m\r\n");
        assert_eq!(e.row_ansi(Space::Active, 1, 0, 80).unwrap(), "a \x1b[0;1;31mred\x1b[0m b \x1b[0;7msel\x1b[0m");
        // 256-colour and true colour.
        e.feed(b"\x1b[38;5;208;48;2;1;2;3mx\x1b[0m\r\n");
        assert_eq!(e.row_ansi(Space::Active, 2, 0, 80).unwrap(), "\x1b[0;38;5;208;48;2;1;2;3mx\x1b[0m");
        // A highlighted bar: erased cells that carry only a background.
        e.feed(b"\x1b[44mbar\x1b[K\x1b[0m\r\n");
        let bar = e.row_ansi(Space::Active, 3, 0, 80).unwrap();
        assert!(bar.starts_with("\x1b[0;44mbar") && bar.ends_with("\x1b[0m"), "{bar:?}");
        assert_eq!(bar.matches(' ').count(), 77, "{bar:?}");
        // A slice of the row.
        assert_eq!(e.row_ansi(Space::Active, 1, 2, 3).unwrap(), "\x1b[0;1;31mred\x1b[0m");
        assert_eq!(e.row_ansi(Space::Active, 0, 2, 2).unwrap(), "ai");
        // The plain reader is unchanged by styles.
        assert_eq!(e.row(Space::Active, 1, None).unwrap().text, "a red b sel");
    }

    #[test]
    fn tracked_row_follows_scrolling() {
        let mut e = Emu::new(20, 5, 1 << 20).unwrap();
        let t = e.track(Space::Active, 0).unwrap();
        assert_eq!(e.tracked_row(&t), Some(0));
        for i in 0..30 {
            e.feed(format!("line {i}\r\n").as_bytes());
        }
        assert_eq!(e.tracked_row(&t), Some(0));
        assert_eq!(e.scrollback_rows(), 26);
        assert_eq!(e.row(Space::Screen, 0, None).unwrap().text, "line 0");
        assert!(e.tracked_set(&t, Space::Screen, 26));
        assert_eq!(e.tracked_row(&t), Some(26));
        e.feed(b"more\r\n");
        assert_eq!(e.tracked_row(&t), Some(26));
        assert_eq!(e.scrollback_rows(), 27);
    }
}
