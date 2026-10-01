//! Pure helpers for the Windows ConPTY relay ([`super::conpty`]): console
//! mode translation, the window size clamp, UTF-16 key input to UTF-8 bytes,
//! the activation decision, and the argv and report protocol between csm and
//! its `__conpty-leader` helper. No Windows API is called here, so all of it
//! is unit-tested on every platform (the module builds on Windows and under
//! `cfg(test)`).

#![cfg_attr(not(windows), allow(dead_code))]

use std::ffi::{OsStr, OsString};

use crate::config::IdleCompactMode;
use crate::platform::launcher::ChildEnv;

// ─── console modes ───────────────────────────────────────────────────────────
//
// The values are the documented `SetConsoleMode` flags (wincon.h), spelled
// out here so this module builds without windows-sys.

pub const ENABLE_PROCESSED_INPUT: u32 = 0x0001;
pub const ENABLE_LINE_INPUT: u32 = 0x0002;
pub const ENABLE_ECHO_INPUT: u32 = 0x0004;
pub const ENABLE_WINDOW_INPUT: u32 = 0x0008;
pub const ENABLE_MOUSE_INPUT: u32 = 0x0010;
pub const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;

pub const ENABLE_PROCESSED_OUTPUT: u32 = 0x0001;
pub const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
pub const DISABLE_NEWLINE_AUTO_RETURN: u32 = 0x0008;

/// The outer console's input mode while the relay runs: keys arrive as VT
/// sequences, nothing is echoed or line-buffered, and Ctrl-C is an ordinary
/// 0x03 key rather than a console control event. Window size events are
/// reported. Mouse input is off (a click would otherwise arrive as a
/// `MOUSE_EVENT` record the relay has no VT encoding for). Every other bit
/// (quick edit, insert mode, extended flags) is kept as it was.
pub fn raw_input_mode(original: u32) -> u32 {
    (original
        & !(ENABLE_PROCESSED_INPUT | ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_MOUSE_INPUT))
        | ENABLE_VIRTUAL_TERMINAL_INPUT
        | ENABLE_WINDOW_INPUT
}

/// The outer console's output mode while the relay runs: VT sequences are
/// interpreted, and a line feed does not also return the carriage (ConPTY's
/// output already spells out every CR, as a real terminal expects).
pub fn vt_output_mode(original: u32) -> u32 {
    original
        | ENABLE_PROCESSED_OUTPUT
        | ENABLE_VIRTUAL_TERMINAL_PROCESSING
        | DISABLE_NEWLINE_AUTO_RETURN
}

/// [`vt_output_mode`] without `DISABLE_NEWLINE_AUTO_RETURN`, for a console
/// that rejects that flag.
pub fn vt_output_mode_fallback(original: u32) -> u32 {
    original | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING
}

// ─── window size ─────────────────────────────────────────────────────────────

/// The size (rows, cols) of the visible console window given its `srWindow`
/// rectangle (inclusive bounds), clamped the way the POSIX relay's screen
/// model is ([`crate::idle_compact::supervisor::MIN_ROWS`] x
/// [`crate::idle_compact::supervisor::MIN_COLS`] at least). The ConPTY gets
/// the same clamped size: `CreatePseudoConsole` rejects a zero size, and an
/// unclamped tiny pseudoconsole would disagree with the screen model.
pub fn window_size(left: i16, top: i16, right: i16, bottom: i16) -> (u16, u16) {
    let span = |lo: i16, hi: i16| -> u16 {
        let n = i32::from(hi) - i32::from(lo) + 1;
        n.clamp(0, i32::from(i16::MAX)) as u16
    };
    clamp_size(span(top, bottom), span(left, right))
}

/// Clamp (rows, cols) to the screen model's floor and to what a `COORD`
/// can carry.
pub fn clamp_size(rows: u16, cols: u16) -> (u16, u16) {
    use crate::idle_compact::supervisor::{MIN_COLS, MIN_ROWS};
    let max = i16::MAX as u16;
    (rows.clamp(MIN_ROWS, max), cols.clamp(MIN_COLS, max))
}

// ─── activation ──────────────────────────────────────────────────────────────

/// Windows activation: [`super::should_activate`] with the foreground check
/// always true. A Windows console has no foreground process group; a process
/// that owns a console handle on both stdin and stdout is the one the user
/// is typing at.
pub fn should_activate(
    mode: IdleCompactMode,
    stdin_is_console: bool,
    stdout_is_console: bool,
    csm_relay_env: Option<&str>,
) -> bool {
    super::should_activate(
        mode,
        stdin_is_console,
        stdout_is_console,
        true,
        csm_relay_env,
    )
}

// ─── key input ───────────────────────────────────────────────────────────────

/// UTF-16 code units from `KEY_EVENT` records to UTF-8 bytes. A surrogate
/// pair may arrive split across two records (or two reads), so a high
/// surrogate waits here for its partner; an unpaired surrogate becomes
/// U+FFFD.
#[derive(Debug, Default)]
pub struct Utf16Decoder {
    high: Option<u16>,
}

impl Utf16Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one code unit, appending whatever it completes to `out`.
    pub fn push(&mut self, unit: u16, out: &mut Vec<u8>) {
        let mut put = |c: char| {
            let mut b = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
        };
        match unit {
            0xD800..=0xDBFF => {
                if self.high.replace(unit).is_some() {
                    put(char::REPLACEMENT_CHARACTER);
                }
            }
            0xDC00..=0xDFFF => match self.high.take() {
                Some(high) => {
                    let c =
                        0x10000 + ((u32::from(high) - 0xD800) << 10) + (u32::from(unit) - 0xDC00);
                    put(char::from_u32(c).unwrap_or(char::REPLACEMENT_CHARACTER));
                }
                None => put(char::REPLACEMENT_CHARACTER),
            },
            _ => {
                if self.high.take().is_some() {
                    put(char::REPLACEMENT_CHARACTER);
                }
                put(char::from_u32(u32::from(unit)).unwrap_or(char::REPLACEMENT_CHARACTER));
            }
        }
    }
}

/// UTF-8 output bytes to UTF-16 for `WriteConsoleW`. A multi-byte character
/// may be split across two output chunks, so an incomplete tail is carried
/// to the next call; an invalid byte becomes U+FFFD.
#[derive(Debug, Default)]
pub struct Utf8Carry {
    tail: Vec<u8>,
}

impl Utf8Carry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode `bytes` (after any carried tail) to UTF-16 code units.
    pub fn decode(&mut self, bytes: &[u8]) -> Vec<u16> {
        self.tail.extend_from_slice(bytes);
        let mut wide = Vec::with_capacity(self.tail.len());
        let mut i = 0;
        while i < self.tail.len() {
            match std::str::from_utf8(&self.tail[i..]) {
                Ok(s) => {
                    wide.extend(s.encode_utf16());
                    i = self.tail.len();
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    let s = std::str::from_utf8(&self.tail[i..i + valid]).unwrap_or_default();
                    wide.extend(s.encode_utf16());
                    i += valid;
                    match e.error_len() {
                        Some(n) => {
                            wide.push(0xFFFD);
                            i += n;
                        }
                        None => break,
                    }
                }
            }
        }
        self.tail.drain(..i);
        wide
    }
}

// ─── command line ────────────────────────────────────────────────────────────

/// Append `arg` (UTF-16) to a `CreateProcessW` command line, quoted so the
/// MSVC runtime's argv parser (and Rust's `std::env::args_os`, which follows
/// the same rules) reads it back unchanged. A leading space separates it
/// from what is already there.
pub fn append_arg(cmdline: &mut Vec<u16>, arg: &[u16]) {
    const SPACE: u16 = b' ' as u16;
    const TAB: u16 = b'\t' as u16;
    const NL: u16 = b'\n' as u16;
    const QUOTE: u16 = b'"' as u16;
    const BACKSLASH: u16 = b'\\' as u16;

    if !cmdline.is_empty() {
        cmdline.push(SPACE);
    }
    let needs_quotes = arg.is_empty() || arg.iter().any(|&c| matches!(c, SPACE | TAB | NL | QUOTE));
    if !needs_quotes {
        cmdline.extend_from_slice(arg);
        return;
    }
    cmdline.push(QUOTE);
    let mut backslashes = 0usize;
    for &c in arg {
        if c == BACKSLASH {
            backslashes += 1;
        } else {
            if c == QUOTE {
                // Backslashes before a quote are doubled, plus one to escape it.
                cmdline.extend(std::iter::repeat_n(BACKSLASH, backslashes + 1));
            }
            backslashes = 0;
        }
        cmdline.push(c);
    }
    // Backslashes before the closing quote are doubled.
    cmdline.extend(std::iter::repeat_n(BACKSLASH, backslashes));
    cmdline.push(QUOTE);
}

// ─── the helper's argv ───────────────────────────────────────────────────────

/// The hidden subcommand csm re-executes itself with inside the ConPTY.
pub const LEADER_WORD: &str = "__conpty-leader";

/// What csm tells its `__conpty-leader` helper: the two inherited pipe
/// handles (as numbers), the session id (for the `<sid>.stop` flag), the
/// environment changes for claude, and claude's full argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaderArgs {
    pub report: usize,
    pub control: usize,
    pub sid: String,
    pub env: ChildEnv,
    pub argv: Vec<OsString>,
}

impl LeaderArgs {
    /// The argv after `csm __conpty-leader`. `--set` pairs are sorted so the
    /// command line is the same for the same inputs.
    pub fn to_args(&self) -> Vec<OsString> {
        let mut out: Vec<OsString> = vec![
            self.report.to_string().into(),
            self.control.to_string().into(),
            self.sid.clone().into(),
        ];
        let mut set: Vec<_> = self.env.set.iter().collect();
        set.sort();
        for (k, v) in set {
            out.extend(["--set".into(), k.clone(), v.clone()]);
        }
        for k in &self.env.remove {
            out.extend(["--unset".into(), k.clone()]);
        }
        out.push("--".into());
        out.extend(self.argv.iter().cloned());
        out
    }

    /// Parse what [`to_args`](Self::to_args) produced.
    pub fn parse(args: &[OsString]) -> Result<Self, String> {
        let num = |a: Option<&OsString>, what: &str| -> Result<usize, String> {
            a.and_then(|s| s.to_str())
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| format!("missing or bad {what}"))
        };
        let report = num(args.first(), "report handle")?;
        let control = num(args.get(1), "control handle")?;
        let sid = args
            .get(2)
            .and_then(|s| s.to_str())
            .ok_or("missing session id")?
            .to_owned();
        let mut env = ChildEnv::default();
        let mut i = 3;
        loop {
            match args.get(i).map(OsString::as_os_str) {
                Some(w) if w == OsStr::new("--set") => {
                    let (Some(k), Some(v)) = (args.get(i + 1), args.get(i + 2)) else {
                        return Err("--set needs a name and a value".into());
                    };
                    env.set.insert(k.clone(), v.clone());
                    i += 3;
                }
                Some(w) if w == OsStr::new("--unset") => {
                    let Some(k) = args.get(i + 1) else {
                        return Err("--unset needs a name".into());
                    };
                    env.remove.push(k.clone());
                    i += 2;
                }
                Some(w) if w == OsStr::new("--") => break,
                _ => return Err("expected --set, --unset or --".into()),
            }
        }
        let argv = args[i + 1..].to_vec();
        if argv.is_empty() {
            return Err("no program to run".into());
        }
        Ok(LeaderArgs {
            report,
            control,
            sid,
            env,
            argv,
        })
    }
}

// ─── report / control lines ──────────────────────────────────────────────────

/// One line the helper writes to csm on the report pipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    /// claude is running with this pid.
    Pid(u32),
    /// claude could not be started.
    Fail(String),
}

impl Report {
    pub fn format(&self) -> String {
        match self {
            Report::Pid(p) => format!("pid {p}"),
            Report::Fail(m) => format!("fail {}", m.replace(['\r', '\n'], " ")),
        }
    }

    pub fn parse(line: &str) -> Option<Self> {
        let line = line.trim_end_matches(['\r', '\n']);
        if let Some(p) = line.strip_prefix("pid ") {
            return p.trim().parse().ok().map(Report::Pid);
        }
        line.strip_prefix("fail ")
            .map(|m| Report::Fail(m.to_owned()))
    }
}

/// The one line csm writes to the helper on the control pipe: forward a
/// Ctrl-Break to claude's process group.
pub const CONTROL_BREAK: &str = "break";

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn cmdline(args: &[&str]) -> String {
        let mut out = Vec::new();
        for a in args {
            append_arg(&mut out, &utf16(a));
        }
        String::from_utf16(&out).unwrap()
    }

    /// The MSVC argv rules (what CommandLineToArgvW and Rust's args_os do),
    /// written out here to check `append_arg` round-trips.
    fn parse_cmdline(s: &str) -> Vec<String> {
        let mut args = Vec::new();
        let chars: Vec<char> = s.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            while i < chars.len() && (chars[i] == ' ' || chars[i] == '\t') {
                i += 1;
            }
            if i >= chars.len() {
                break;
            }
            let mut cur = String::new();
            let mut in_quotes = false;
            while i < chars.len() {
                let c = chars[i];
                if c == '\\' {
                    let mut n = 0;
                    while i < chars.len() && chars[i] == '\\' {
                        n += 1;
                        i += 1;
                    }
                    if i < chars.len() && chars[i] == '"' {
                        cur.extend(std::iter::repeat_n('\\', n / 2));
                        if n % 2 == 1 {
                            cur.push('"');
                            i += 1;
                        }
                    } else {
                        cur.extend(std::iter::repeat_n('\\', n));
                    }
                    continue;
                }
                if c == '"' {
                    if in_quotes && i + 1 < chars.len() && chars[i + 1] == '"' {
                        cur.push('"');
                        i += 2;
                        continue;
                    }
                    in_quotes = !in_quotes;
                    i += 1;
                    continue;
                }
                if !in_quotes && (c == ' ' || c == '\t') {
                    break;
                }
                cur.push(c);
                i += 1;
            }
            args.push(cur);
        }
        args
    }

    #[test]
    fn raw_input_mode_turns_off_line_echo_processed_and_mouse() {
        let original = ENABLE_PROCESSED_INPUT
            | ENABLE_LINE_INPUT
            | ENABLE_ECHO_INPUT
            | ENABLE_MOUSE_INPUT
            | 0x0040 // quick edit
            | 0x0080; // extended flags
        let m = raw_input_mode(original);
        assert_eq!(m & ENABLE_PROCESSED_INPUT, 0, "Ctrl-C must be input");
        assert_eq!(m & ENABLE_LINE_INPUT, 0);
        assert_eq!(m & ENABLE_ECHO_INPUT, 0);
        assert_eq!(m & ENABLE_MOUSE_INPUT, 0);
        assert_ne!(m & ENABLE_VIRTUAL_TERMINAL_INPUT, 0);
        assert_ne!(m & ENABLE_WINDOW_INPUT, 0);
        assert_eq!(m & 0x00C0, 0x00C0, "other bits are kept");
    }

    #[test]
    fn raw_input_mode_from_zero() {
        assert_eq!(
            raw_input_mode(0),
            ENABLE_VIRTUAL_TERMINAL_INPUT | ENABLE_WINDOW_INPUT
        );
    }

    #[test]
    fn vt_output_mode_adds_vt_and_keeps_wrap() {
        let wrap = 0x0002;
        let m = vt_output_mode(ENABLE_PROCESSED_OUTPUT | wrap);
        assert_ne!(m & ENABLE_VIRTUAL_TERMINAL_PROCESSING, 0);
        assert_ne!(m & DISABLE_NEWLINE_AUTO_RETURN, 0);
        assert_ne!(m & wrap, 0);
        let f = vt_output_mode_fallback(wrap);
        assert_ne!(f & ENABLE_VIRTUAL_TERMINAL_PROCESSING, 0);
        assert_eq!(f & DISABLE_NEWLINE_AUTO_RETURN, 0);
    }

    #[test]
    fn window_size_is_inclusive_and_clamped() {
        assert_eq!(window_size(0, 0, 119, 39), (40, 120));
        assert_eq!(window_size(0, 100, 79, 123), (24, 80), "scrolled window");
        assert_eq!(window_size(0, 0, 0, 0), (5, 20), "1x1 clamps up");
        assert_eq!(window_size(0, 0, -1, -1), (5, 20), "empty clamps up");
        assert_eq!(window_size(10, 10, 0, 0), (5, 20), "inverted clamps up");
    }

    #[test]
    fn clamp_size_floor_and_ceiling() {
        assert_eq!(clamp_size(0, 0), (5, 20));
        assert_eq!(clamp_size(4, 19), (5, 20));
        assert_eq!(clamp_size(5, 20), (5, 20));
        assert_eq!(clamp_size(50, 200), (50, 200));
        assert_eq!(clamp_size(u16::MAX, u16::MAX), (32767, 32767));
    }

    #[test]
    fn activation_on_windows() {
        use IdleCompactMode::*;
        assert!(should_activate(On, true, true, None));
        assert!(should_activate(DryRun, true, true, Some("1")));
        assert!(!should_activate(Off, true, true, None));
        assert!(!should_activate(On, true, true, Some("0")));
        assert!(!should_activate(On, false, true, None), "stdin redirected");
        assert!(!should_activate(On, true, false, None), "stdout redirected");
    }

    #[test]
    fn utf16_decoder_handles_ascii_bmp_and_pairs() {
        let mut d = Utf16Decoder::new();
        let mut out = Vec::new();
        for u in "a\u{3}é한".encode_utf16() {
            d.push(u, &mut out);
        }
        assert_eq!(out, "a\u{3}é한".as_bytes());

        // A pair split across two pushes (two records).
        let mut out = Vec::new();
        let units: Vec<u16> = "😀".encode_utf16().collect();
        d.push(units[0], &mut out);
        assert!(out.is_empty(), "a lone high surrogate waits");
        d.push(units[1], &mut out);
        assert_eq!(out, "😀".as_bytes());
    }

    #[test]
    fn utf16_decoder_replaces_unpaired_surrogates() {
        let mut d = Utf16Decoder::new();
        let mut out = Vec::new();
        d.push(0xDC00, &mut out);
        d.push(0xD800, &mut out);
        d.push(b'x' as u16, &mut out);
        assert_eq!(String::from_utf8(out).unwrap(), "\u{FFFD}\u{FFFD}x");
    }

    #[test]
    fn utf8_carry_joins_split_characters() {
        let mut c = Utf8Carry::new();
        let s = "a한😀b";
        let bytes = s.as_bytes();
        let mut wide = Vec::new();
        for b in bytes {
            wide.extend(c.decode(std::slice::from_ref(b)));
        }
        assert_eq!(String::from_utf16(&wide).unwrap(), s);
    }

    #[test]
    fn utf8_carry_replaces_invalid_bytes() {
        let mut c = Utf8Carry::new();
        let wide = c.decode(b"a\xffb\xe2\x28c");
        assert_eq!(String::from_utf16(&wide).unwrap(), "a\u{FFFD}b\u{FFFD}(c");
    }

    #[test]
    fn append_arg_round_trips() {
        let cases: &[&[&str]] = &[
            &["C:\\Program Files\\csm.exe", "__conpty-leader", "12", "--"],
            &["a", "", "b c", "tab\there"],
            &["say \"hi\"", "back\\slash", "trail\\", "trail space\\"],
            &["\\\\server\\share\\x y", "q\\\"", "\\\"", "\"\""],
            &["--model", "sonnet", "-p", "hello & world | ^%PATH%"],
            &["한글 인자", "😀"],
        ];
        for args in cases {
            let line = cmdline(args);
            let back = parse_cmdline(&line);
            let want: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            assert_eq!(back, want, "{line}");
        }
    }

    #[test]
    fn append_arg_leaves_plain_words_bare() {
        assert_eq!(cmdline(&["a", "b-c", "--x=1"]), "a b-c --x=1");
        assert_eq!(cmdline(&["", "a b"]), "\"\" \"a b\"");
    }

    #[test]
    fn leader_args_round_trip() {
        let mut env = ChildEnv::default();
        env.set.insert("CSM_SUPERVISOR_PID".into(), "42".into());
        env.set
            .insert("CLAUDE_CONFIG_DIR".into(), "C:\\cfg dir".into());
        env.remove.push("ANTHROPIC_API_KEY".into());
        let a = LeaderArgs {
            report: 1234,
            control: 5678,
            sid: "abc-123".into(),
            env,
            argv: vec!["claude".into(), "--".into(), "--set".into(), "x".into()],
        };
        let args = a.to_args();
        assert_eq!(LeaderArgs::parse(&args).unwrap(), a);
    }

    #[test]
    fn leader_args_rejects_garbage() {
        let os = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(LeaderArgs::parse(&os(&[])).is_err());
        assert!(LeaderArgs::parse(&os(&["x", "1", "sid", "--", "claude"])).is_err());
        assert!(LeaderArgs::parse(&os(&["1", "2", "sid", "--"])).is_err());
        assert!(LeaderArgs::parse(&os(&["1", "2", "sid", "--set", "K"])).is_err());
        assert!(LeaderArgs::parse(&os(&["1", "2", "sid", "claude"])).is_err());
        assert!(LeaderArgs::parse(&os(&["1", "2", "sid", "--", "claude"])).is_ok());
    }

    #[test]
    fn report_lines_round_trip() {
        for r in [Report::Pid(4242), Report::Fail("no such file".into())] {
            assert_eq!(Report::parse(&format!("{}\r\n", r.format())), Some(r));
        }
        assert_eq!(Report::Fail("a\nb".into()).format(), "fail a b", "one line");
        assert_eq!(Report::parse("pid x"), None);
        assert_eq!(Report::parse("garbage"), None);
    }
}
