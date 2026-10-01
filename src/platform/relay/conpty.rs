//! Windows ConPTY relay: the Windows half of [`super`].
//!
//! In direct mode ([`WindowsLauncher`]) claude shares csm's console. In relay
//! mode csm keeps the real console for itself and runs claude inside a
//! pseudoconsole (ConPTY) it creates, so every byte claude prints passes
//! through csm on its way out and every key passes through csm on its way in.
//!
//! claude is not started by csm directly. csm starts a re-exec of itself,
//! `csm __conpty-leader` ([`leader_main`]), attached to the pseudoconsole;
//! that helper starts claude with `std::process::Command` (so `PATH`,
//! `PATHEXT` and `.cmd` shims resolve exactly as in direct mode) and then
//! does what [`WindowsLauncher`] does for a direct launch, from inside the
//! pseudoconsole where the console control calls reach claude: claude gets
//! its own process group, a console Ctrl-C/Ctrl-Break is forwarded to it,
//! and the `<sid>.stop` flag the limit-switch hook writes gets a Ctrl-Break,
//! a grace period and then `TerminateProcess`. The helper exits with
//! claude's exit code. Two inherited anonymous pipes connect it to csm: the
//! report pipe (helper to csm: `pid <n>` once claude runs, or `fail <why>`)
//! and the control pipe (csm to helper: `break`).
//!
//! csm's side:
//!
//!   - The outer console goes into raw VT mode ([`conpty_logic::raw_input_mode`],
//!     [`conpty_logic::vt_output_mode`]); the original modes come back on every
//!     exit path: the guard's `Drop`, a panic hook, and the console control
//!     handler for a close, logoff or shutdown event.
//!   - **input** thread: waits on the console input handle and reads
//!     `INPUT_RECORD`s. Key-down characters (VT sequences, since the console is
//!     in VT input mode; Ctrl-C is a plain 0x03 because processed input is off)
//!     become UTF-8 and go to the ConPTY input pipe, or into the [`InputHold`]
//!     buffer while the supervisor is typing. The same loop polls the visible
//!     window size every 100 ms (and on `WINDOW_BUFFER_SIZE_EVENT`), and on a
//!     change calls `ResizePseudoConsole` and [`RelayObserver::on_resize`].
//!   - **output** thread: reads the ConPTY output pipe, writes it to the real
//!     console with `WriteConsoleW` and feeds [`RelayObserver::on_output`].
//!   - the launching thread waits for the helper, takes its exit code as
//!     claude's, closes the pseudoconsole and joins both threads.
//!
//! A console control event that does arrive (Ctrl-C sent by another process,
//! Ctrl-Break) never kills csm: Ctrl-C becomes a 0x03 key for claude, and
//! Ctrl-Break goes to the helper, which forwards it to claude's group. A
//! close event restores the console, closes the pseudoconsole (which sends
//! claude its own close event) and gives the helper a few seconds to exit.
//!
//! Any failure before the helper reports claude's pid falls back to
//! [`WindowsLauncher`] for that launch, logged once, as on unix.

use std::ffi::{OsStr, OsString, c_void};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::os::windows::process::{CommandExt, ExitStatusExt};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, FALSE, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation, TRUE, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Console::{
    CONSOLE_SCREEN_BUFFER_INFO, COORD, CTRL_BREAK_EVENT, CTRL_C_EVENT, ClosePseudoConsole,
    CreatePseudoConsole, GenerateConsoleCtrlEvent, GetConsoleMode, GetConsoleScreenBufferInfo,
    GetStdHandle, HPCON, INPUT_RECORD, KEY_EVENT, ReadConsoleInputW, ResizePseudoConsole,
    STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleCtrlHandler, SetConsoleMode,
    WINDOW_BUFFER_SIZE_EVENT, WriteConsoleW,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject,
};
use windows_sys::core::BOOL;

use super::classify::Classifier;
use super::conpty_logic::{
    self, CONTROL_BREAK, LEADER_WORD, LeaderArgs, Report, Utf8Carry, Utf16Decoder,
};
use super::{NoopObserver, RelayIo, RelayObserver};
use crate::config::IdleCompactMode;
use crate::platform::launcher::{ChildEnv, ChildHandle, Launcher};
use crate::platform::windows::WindowsLauncher;

/// How often the input thread wakes to check the window size and whether
/// the launch is over.
const INPUT_POLL_MS: u32 = 100;
/// After claude exits, wait this long at most for the pseudoconsole's last
/// frame before closing it.
const FINAL_FRAME_WAIT: Duration = Duration::from_millis(500);
/// How long a console close event waits for the helper to exit.
const CLOSE_WAIT_MS: u32 = 3000;

/// Lock a mutex even if a panic poisoned it: restoring the console must work
/// from a panic hook or a control handler.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ─── activation ──────────────────────────────────────────────────────────────

fn std_handle(id: u32) -> HANDLE {
    // SAFETY: GetStdHandle has no preconditions.
    unsafe { GetStdHandle(id) }
}

fn console_mode(h: HANDLE) -> Option<u32> {
    if h.is_null() || h as isize == -1 {
        return None;
    }
    let mut mode = 0u32;
    // SAFETY: `mode` is a valid out pointer; a non-console handle just fails.
    (unsafe { GetConsoleMode(h, &mut mode) } != 0).then_some(mode)
}

/// I/O shell around [`conpty_logic::should_activate`]: stdin and stdout must
/// both be console handles, and `CSM_RELAY` must not be `0`.
pub(crate) fn platform_should_activate(mode: IdleCompactMode) -> bool {
    let stdin_is_console = console_mode(std_handle(STD_INPUT_HANDLE)).is_some();
    let stdout_is_console = console_mode(std_handle(STD_OUTPUT_HANDLE)).is_some();
    let csm_relay = std::env::var("CSM_RELAY").ok();
    conpty_logic::should_activate(
        mode,
        stdin_is_console,
        stdout_is_console,
        csm_relay.as_deref(),
    )
}

// ─── outer console modes (process-global, restore from anywhere) ─────────────

#[derive(Clone, Copy)]
struct SavedModes {
    input: usize,
    input_mode: u32,
    output: usize,
    output_mode: u32,
}

static ORIGINAL_MODES: Mutex<Option<SavedModes>> = Mutex::new(None);
static PANIC_HOOK: OnceLock<()> = OnceLock::new();

fn install_panic_hook() {
    PANIC_HOOK.get_or_init(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_console();
            prev(info);
        }));
    });
}

fn apply_raw(s: &SavedModes) -> io::Result<()> {
    let input = s.input as HANDLE;
    let output = s.output as HANDLE;
    // SAFETY: both handles were checked to be console handles when saved.
    unsafe {
        if SetConsoleMode(input, conpty_logic::raw_input_mode(s.input_mode)) == 0 {
            return Err(io::Error::last_os_error());
        }
        if SetConsoleMode(output, conpty_logic::vt_output_mode(s.output_mode)) == 0
            && SetConsoleMode(output, conpty_logic::vt_output_mode_fallback(s.output_mode)) == 0
        {
            let e = io::Error::last_os_error();
            SetConsoleMode(input, s.input_mode);
            return Err(e);
        }
    }
    Ok(())
}

/// Put the outer console back the way [`RawConsole::enter`] found it. A
/// no-op when no guard is active.
fn restore_console() {
    if let Some(s) = *lock(&ORIGINAL_MODES) {
        // SAFETY: see `apply_raw`.
        unsafe {
            SetConsoleMode(s.input as HANDLE, s.input_mode);
            SetConsoleMode(s.output as HANDLE, s.output_mode);
        }
    }
}

/// Put the outer console back in raw mode after an observer swallowed a
/// panic (the panic hook restored the original modes).
pub(crate) fn reassert_raw() {
    if let Some(s) = *lock(&ORIGINAL_MODES) {
        let _ = apply_raw(&s);
    }
}

/// Saves the outer console's modes, switches to raw VT mode, and restores
/// them on `Drop`.
struct RawConsole;

impl RawConsole {
    fn enter() -> io::Result<Self> {
        install_panic_hook();
        let input = std_handle(STD_INPUT_HANDLE);
        let output = std_handle(STD_OUTPUT_HANDLE);
        let (Some(input_mode), Some(output_mode)) = (console_mode(input), console_mode(output))
        else {
            return Err(io::Error::other("stdin or stdout is not a console"));
        };
        let saved = SavedModes {
            input: input as usize,
            input_mode,
            output: output as usize,
            output_mode,
        };
        *lock(&ORIGINAL_MODES) = Some(saved);
        if let Err(e) = apply_raw(&saved) {
            *lock(&ORIGINAL_MODES) = None;
            return Err(e);
        }
        Ok(RawConsole)
    }
}

impl Drop for RawConsole {
    fn drop(&mut self) {
        restore_console();
        *lock(&ORIGINAL_MODES) = None;
    }
}

// ─── the outer console's size ────────────────────────────────────────────────

/// The visible window's size (rows, cols), clamped.
fn console_size() -> io::Result<(u16, u16)> {
    // SAFETY: zeroed is a valid CONSOLE_SCREEN_BUFFER_INFO; the call fills it.
    let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
    if unsafe { GetConsoleScreenBufferInfo(std_handle(STD_OUTPUT_HANDLE), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let w = info.srWindow;
    Ok(conpty_logic::window_size(w.Left, w.Top, w.Right, w.Bottom))
}

fn coord(rows: u16, cols: u16) -> COORD {
    COORD {
        X: cols as i16,
        Y: rows as i16,
    }
}

// ─── platform half of RelayIo ────────────────────────────────────────────────

/// What [`RelayIo`] writes claude's input to: the ConPTY's input pipe.
pub(super) struct Master {
    pipe: File,
}

/// Write all of `bytes` to the ConPTY (claude's input).
pub(super) fn write_master(master: &Master, bytes: &[u8]) -> io::Result<()> {
    (&master.pipe).write_all(bytes)
}

static OUTPUT_CARRY: Mutex<Option<Utf8Carry>> = Mutex::new(None);

/// Write all of `bytes` (UTF-8, possibly ending mid-character) to the outer
/// console with `WriteConsoleW`, so the console's code page plays no part.
pub(super) fn write_stdout(bytes: &[u8]) -> io::Result<()> {
    let wide = lock(&OUTPUT_CARRY)
        .get_or_insert_with(Utf8Carry::new)
        .decode(bytes);
    let out = std_handle(STD_OUTPUT_HANDLE);
    let mut rest: &[u16] = &wide;
    while !rest.is_empty() {
        let chunk = rest.len().min(8192);
        let mut written = 0u32;
        // SAFETY: `rest[..chunk]` is valid for reads; `written` is an out param.
        let ok = unsafe {
            WriteConsoleW(
                out,
                rest.as_ptr(),
                chunk as u32,
                &mut written,
                std::ptr::null(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        if written == 0 {
            return Err(io::Error::from(io::ErrorKind::WriteZero));
        }
        rest = &rest[written as usize..];
    }
    Ok(())
}

/// Bytes the user typed: to claude, or into the hold buffer.
fn user_input(io: &RelayIo, bytes: &[u8]) {
    if io.is_held() {
        io.buffer_held(bytes);
    } else {
        let _ = io.write_master(bytes);
    }
}

// ─── the pseudoconsole ───────────────────────────────────────────────────────

/// The pseudoconsole handle, closed exactly once by whoever gets there first
/// (the launching thread, an outer hangup, or a console close event).
struct Pcon {
    hpc: Mutex<Option<HPCON>>,
}

impl Pcon {
    fn resize(&self, rows: u16, cols: u16) -> bool {
        match *lock(&self.hpc) {
            // SAFETY: a live pseudoconsole handle.
            Some(h) => (unsafe { ResizePseudoConsole(h, coord(rows, cols)) }) == 0,
            None => false,
        }
    }

    fn close(&self) {
        if let Some(h) = lock(&self.hpc).take() {
            // SAFETY: taken out of the Option, so closed once.
            unsafe { ClosePseudoConsole(h) };
        }
    }
}

impl Drop for Pcon {
    fn drop(&mut self) {
        self.close();
    }
}

fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let mut r: HANDLE = std::ptr::null_mut();
    let mut w: HANDLE = std::ptr::null_mut();
    // SAFETY: out params; default security, default buffer size.
    if unsafe { CreatePipe(&mut r, &mut w, std::ptr::null(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both are fresh handles this process owns.
    Ok(unsafe {
        (
            OwnedHandle::from_raw_handle(r as RawHandle),
            OwnedHandle::from_raw_handle(w as RawHandle),
        )
    })
}

fn set_inheritable(h: &OwnedHandle) -> io::Result<()> {
    // SAFETY: a live handle this process owns.
    if unsafe {
        SetHandleInformation(
            h.as_raw_handle() as HANDLE,
            HANDLE_FLAG_INHERIT,
            HANDLE_FLAG_INHERIT,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().collect()
}

/// Start `exe args...` attached to the pseudoconsole `hpc`, inheriting only
/// `inherit`. Returns the process handle and pid.
fn spawn_attached(
    exe: &std::path::Path,
    args: &[OsString],
    hpc: HPCON,
    inherit: &[HANDLE],
) -> io::Result<(OwnedHandle, u32)> {
    let mut cmdline = Vec::new();
    conpty_logic::append_arg(&mut cmdline, &wide(exe.as_os_str()));
    for a in args {
        conpty_logic::append_arg(&mut cmdline, &wide(a));
    }
    cmdline.push(0);
    let mut app = wide(exe.as_os_str());
    app.push(0);

    let mut size = 0usize;
    // SAFETY: the documented size query (fails with ERROR_INSUFFICIENT_BUFFER).
    unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 2, 0, &mut size) };
    // u64 storage keeps the list pointer-aligned.
    let mut storage = vec![0u64; size.div_ceil(8).max(1)];
    let list = storage.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
    // SAFETY: `list` points at `size` writable bytes.
    if unsafe { InitializeProcThreadAttributeList(list, 2, 0, &mut size) } == 0 {
        return Err(io::Error::last_os_error());
    }
    struct ListGuard(LPPROC_THREAD_ATTRIBUTE_LIST);
    impl Drop for ListGuard {
        fn drop(&mut self) {
            // SAFETY: initialised above, deleted once.
            unsafe { DeleteProcThreadAttributeList(self.0) };
        }
    }
    let _guard = ListGuard(list);
    // SAFETY: PSEUDOCONSOLE takes the HPCON value itself; HANDLE_LIST takes a
    // pointer to `inherit`, which outlives CreateProcessW below.
    unsafe {
        if UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            hpc as *const c_void,
            std::mem::size_of::<HPCON>(),
            std::ptr::null_mut(),
            std::ptr::null(),
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        if !inherit.is_empty()
            && UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                inherit.as_ptr() as *const c_void,
                std::mem::size_of_val(inherit),
                std::ptr::null_mut(),
                std::ptr::null(),
            ) == 0
        {
            return Err(io::Error::last_os_error());
        }
    }

    // SAFETY: zeroed is a valid STARTUPINFOEXW.
    let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    si.lpAttributeList = list;
    // Null std handles: the helper must not inherit csm's own stdio (which
    // may be redirected files); it opens CONIN$/CONOUT$ itself.
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    // SAFETY: zeroed is a valid PROCESS_INFORMATION out param.
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: all pointers are valid for the call; `cmdline` is mutable and
    // NUL-terminated as CreateProcessW requires.
    let ok = unsafe {
        CreateProcessW(
            app.as_ptr(),
            cmdline.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            if inherit.is_empty() { FALSE } else { TRUE },
            EXTENDED_STARTUPINFO_PRESENT,
            std::ptr::null(),
            std::ptr::null(),
            &si.StartupInfo,
            &mut pi,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateProcessW returned both handles; the thread one is unused.
    unsafe { CloseHandle(pi.hThread) };
    let process = unsafe { OwnedHandle::from_raw_handle(pi.hProcess as RawHandle) };
    Ok((process, pi.dwProcessId))
}

fn exit_code(process: &OwnedHandle) -> u32 {
    let mut code = 1u32;
    // SAFETY: a live process handle; `code` is an out param.
    unsafe { GetExitCodeProcess(process.as_raw_handle() as HANDLE, &mut code) };
    code
}

// ─── console control events ──────────────────────────────────────────────────

/// What the console control handler needs while a relay session is up.
struct Active {
    io: Arc<RelayIo>,
    pcon: Arc<Pcon>,
    helper: Arc<OwnedHandle>,
    control: Mutex<Option<File>>,
}

impl Active {
    fn send_control(&self, line: &str) {
        if let Some(f) = lock(&self.control).as_mut() {
            let _ = writeln!(f, "{line}").and_then(|_| f.flush());
        }
    }
}

static ACTIVE: Mutex<Option<Arc<Active>>> = Mutex::new(None);

/// csm's console control handler while the relay runs. Never lets the
/// default handler kill csm for Ctrl-C or Ctrl-Break; for a close, logoff or
/// shutdown it restores the console, closes the pseudoconsole (claude gets
/// its own close event) and waits briefly for the helper before letting the
/// default handler end csm.
unsafe extern "system" fn relay_ctrl_handler(ctrl_type: u32) -> BOOL {
    let active = lock(&ACTIVE).clone();
    match ctrl_type {
        CTRL_C_EVENT => {
            if let Some(a) = active {
                a.io.note_keystroke();
                user_input(&a.io, &[0x03]);
            }
            TRUE
        }
        CTRL_BREAK_EVENT => {
            if let Some(a) = active {
                a.send_control(CONTROL_BREAK);
            }
            TRUE
        }
        _ => {
            restore_console();
            if let Some(a) = active {
                a.pcon.close();
                // SAFETY: a live process handle.
                unsafe { WaitForSingleObject(a.helper.as_raw_handle() as HANDLE, CLOSE_WAIT_MS) };
            }
            FALSE
        }
    }
}

/// Registers [`relay_ctrl_handler`] for the life of the guard.
struct CtrlGuard;

impl CtrlGuard {
    fn install() -> Self {
        // SAFETY: registering a process-wide handler.
        unsafe { SetConsoleCtrlHandler(Some(relay_ctrl_handler), TRUE) };
        CtrlGuard
    }
}

impl Drop for CtrlGuard {
    fn drop(&mut self) {
        *lock(&ACTIVE) = None;
        // SAFETY: unregisters what `install` registered.
        unsafe { SetConsoleCtrlHandler(Some(relay_ctrl_handler), FALSE) };
    }
}

// ─── worker threads ──────────────────────────────────────────────────────────

/// Resize the pseudoconsole and the screen model when the window changed.
fn sync_size(io: &RelayIo, pcon: &Pcon, observer: &Arc<dyn RelayObserver>) {
    let Ok((rows, cols)) = console_size() else {
        return;
    };
    if io.size() == (rows, cols) {
        return;
    }
    pcon.resize(rows, cols);
    io.set_size(rows, cols);
    observer.on_resize(rows, cols);
}

fn input_thread(
    io: Arc<RelayIo>,
    pcon: Arc<Pcon>,
    observer: Arc<dyn RelayObserver>,
    stop: Arc<AtomicBool>,
) {
    let input = std_handle(STD_INPUT_HANDLE);
    let mut classifier = Classifier::new();
    let mut decoder = Utf16Decoder::new();
    // SAFETY: zeroed INPUT_RECORDs are valid buffer contents.
    let mut records: [INPUT_RECORD; 128] = unsafe { std::mem::zeroed() };
    let mut bytes = Vec::with_capacity(512);
    while !stop.load(Ordering::SeqCst) {
        // SAFETY: a console input handle (activation checked it).
        let wait = unsafe { WaitForSingleObject(input, INPUT_POLL_MS) };
        sync_size(&io, &pcon, &observer);
        if wait == WAIT_TIMEOUT {
            continue;
        }
        if wait != WAIT_OBJECT_0 {
            // The outer console is gone: hang up claude's.
            pcon.close();
            break;
        }
        let mut n = 0u32;
        // SAFETY: `records` holds 128 entries; `n` is an out param.
        if unsafe { ReadConsoleInputW(input, records.as_mut_ptr(), records.len() as u32, &mut n) }
            == 0
        {
            pcon.close();
            break;
        }
        bytes.clear();
        for rec in &records[..n as usize] {
            match u32::from(rec.EventType) {
                KEY_EVENT => {
                    // SAFETY: EventType says this union member is the live one.
                    let key = unsafe { rec.Event.KeyEvent };
                    let ch = unsafe { key.uChar.UnicodeChar };
                    if key.bKeyDown != 0 && ch != 0 {
                        for _ in 0..key.wRepeatCount.max(1) {
                            decoder.push(ch, &mut bytes);
                        }
                    }
                }
                WINDOW_BUFFER_SIZE_EVENT => sync_size(&io, &pcon, &observer),
                _ => {}
            }
        }
        if bytes.is_empty() {
            continue;
        }
        if classifier.feed(&bytes) {
            io.note_keystroke();
        }
        user_input(&io, &bytes);
    }
}

/// Output that arrived before the observer was started is kept here and
/// handed over in order once it is.
type Early = Arc<Mutex<Option<Vec<u8>>>>;

fn output_thread(io: Arc<RelayIo>, observer: Arc<dyn RelayObserver>, mut pipe: File, early: Early) {
    let mut buf = [0u8; 8192];
    loop {
        let n = match pipe.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        io.note_output();
        let _ = io.relay_output(&buf[..n]);
        let mut early = lock(&early);
        match early.as_mut() {
            Some(pending) => pending.extend_from_slice(&buf[..n]),
            None => {
                drop(early);
                observer.on_output(&buf[..n]);
            }
        }
    }
}

// ─── ConptyLauncher ──────────────────────────────────────────────────────────

enum Failure {
    /// Before claude was confirmed running: fall back to the direct launcher.
    Setup(String),
}

/// The Windows relay launcher. See the module doc.
pub struct ConptyLauncher {
    fallback: WindowsLauncher,
    observer: Arc<dyn RelayObserver>,
}

impl ConptyLauncher {
    pub fn new() -> Self {
        Self::with_observer(Arc::new(NoopObserver))
    }

    pub fn with_observer(observer: Arc<dyn RelayObserver>) -> Self {
        ConptyLauncher {
            fallback: WindowsLauncher,
            observer,
        }
    }
}

impl Default for ConptyLauncher {
    fn default() -> Self {
        Self::new()
    }
}

impl Launcher for ConptyLauncher {
    fn run_foreground(
        &self,
        sid: &str,
        cli: &[OsString],
        env: &ChildEnv,
        on_spawn: &mut dyn FnMut(),
    ) -> io::Result<(ExitStatus, ChildHandle)> {
        match self.try_run_foreground(sid, cli, env, on_spawn) {
            Ok(result) => Ok(result),
            Err(Failure::Setup(msg)) => {
                let _ = crate::hook::notify::append_log(
                    sid,
                    &format!("conpty relay unavailable ({msg}); falling back to direct launcher"),
                );
                self.fallback.run_foreground(sid, cli, env, on_spawn)
            }
        }
    }
}

impl ConptyLauncher {
    fn try_run_foreground(
        &self,
        sid: &str,
        cli: &[OsString],
        env: &ChildEnv,
        on_spawn: &mut dyn FnMut(),
    ) -> Result<(ExitStatus, ChildHandle), Failure> {
        let setup = |what: &str| {
            let what = what.to_owned();
            move |e: io::Error| Failure::Setup(format!("{what}: {e}"))
        };
        let exe = std::env::current_exe().map_err(setup("current_exe"))?;
        let launch = crate::config::launch_command_for_spawn().map_err(setup("launch command"))?;

        let raw = RawConsole::enter().map_err(setup("raw mode"))?;
        let (rows, cols) = console_size().map_err(setup("console size"))?;

        // claude's input: we write `in_w`, the pseudoconsole reads `in_r`.
        // claude's output: the pseudoconsole writes `out_w`, we read `out_r`.
        let (in_r, in_w) = pipe().map_err(setup("input pipe"))?;
        let (out_r, out_w) = pipe().map_err(setup("output pipe"))?;
        let mut hpc: HPCON = 0;
        // SAFETY: valid pipe handles and out param.
        let hr = unsafe {
            CreatePseudoConsole(
                coord(rows, cols),
                in_r.as_raw_handle() as HANDLE,
                out_w.as_raw_handle() as HANDLE,
                0,
                &mut hpc,
            )
        };
        if hr != 0 {
            return Err(Failure::Setup(format!(
                "CreatePseudoConsole: HRESULT {hr:#010x}"
            )));
        }
        let pcon = Arc::new(Pcon {
            hpc: Mutex::new(Some(hpc)),
        });
        // The pseudoconsole holds its own duplicates of these two.
        drop(in_r);
        drop(out_w);

        let io = Arc::new(RelayIo::new(
            Master {
                pipe: File::from(in_w),
            },
            (rows, cols),
        ));

        // Drain the pseudoconsole from the start: it blocks when its output
        // pipe is full, and old Windows blocks ClosePseudoConsole until the
        // pipe is drained.
        let early: Early = Arc::new(Mutex::new(Some(Vec::new())));
        let output = thread::spawn({
            let io = Arc::clone(&io);
            let observer = Arc::clone(&self.observer);
            let early = Arc::clone(&early);
            let pipe = File::from(out_r);
            move || output_thread(io, observer, pipe, early)
        });

        let (report_r, report_w) = pipe().map_err(setup("report pipe"))?;
        let (control_r, control_w) = pipe().map_err(setup("control pipe"))?;
        set_inheritable(&report_w).map_err(setup("report pipe"))?;
        set_inheritable(&control_r).map_err(setup("control pipe"))?;

        let mut leader_env = env.clone();
        leader_env.set.insert(
            OsString::from(crate::idle_compact::SUPERVISOR_PID_ENV),
            OsString::from(std::process::id().to_string()),
        );
        let mut argv = launch;
        argv.extend(cli.iter().cloned());
        let args = LeaderArgs {
            report: report_w.as_raw_handle() as usize,
            control: control_r.as_raw_handle() as usize,
            sid: sid.to_owned(),
            env: leader_env,
            argv,
        };
        let mut leader_argv = vec![OsString::from(LEADER_WORD)];
        leader_argv.extend(args.to_args());

        let ctrl = CtrlGuard::install();
        let born = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let inherit = [
            report_w.as_raw_handle() as HANDLE,
            control_r.as_raw_handle() as HANDLE,
        ];
        let spawned = spawn_attached(&exe, &leader_argv, hpc, &inherit);
        // The helper has its own copies now; ours would keep the pipes open.
        drop(report_w);
        drop(control_r);
        let abort = |pcon: &Pcon, output: JoinHandle<()>, msg: String| {
            pcon.close();
            let _ = output.join();
            Failure::Setup(msg)
        };
        let (helper, _helper_pid) = match spawned {
            Ok(p) => p,
            Err(e) => {
                drop(ctrl);
                let f = abort(&pcon, output, format!("spawn helper: {e}"));
                drop(raw);
                return Err(f);
            }
        };
        let helper = Arc::new(helper);

        let mut report = BufReader::new(File::from(report_r));
        let mut first = String::new();
        let pid = match report
            .read_line(&mut first)
            .ok()
            .and_then(|_| Report::parse(&first))
        {
            Some(Report::Pid(p)) => p,
            other => {
                // SAFETY: the helper is our own child process.
                unsafe { TerminateProcess(helper.as_raw_handle() as HANDLE, 1) };
                unsafe { WaitForSingleObject(helper.as_raw_handle() as HANDLE, INFINITE) };
                drop(ctrl);
                let why = match other {
                    Some(Report::Fail(m)) => m,
                    _ => "helper did not report a pid".to_owned(),
                };
                let f = abort(&pcon, output, why);
                drop(raw);
                return Err(f);
            }
        };
        let _ = crate::platform::pid::write_pid_file(&crate::paths::pid_file(sid), pid, born);
        on_spawn();

        // From here on claude is running: no more fallback.
        *lock(&ACTIVE) = Some(Arc::new(Active {
            io: Arc::clone(&io),
            pcon: Arc::clone(&pcon),
            helper: Arc::clone(&helper),
            control: Mutex::new(Some(File::from(control_w))),
        }));
        self.observer.on_start(Arc::clone(&io), pid, env);
        {
            let mut early = lock(&early);
            if let Some(pending) = early.take()
                && !pending.is_empty()
            {
                self.observer.on_output(&pending);
            }
        }

        let stop = Arc::new(AtomicBool::new(false));
        let input = thread::spawn({
            let io = Arc::clone(&io);
            let pcon = Arc::clone(&pcon);
            let observer = Arc::clone(&self.observer);
            let stop = Arc::clone(&stop);
            move || input_thread(io, pcon, observer, stop)
        });

        // SAFETY: a live process handle.
        unsafe { WaitForSingleObject(helper.as_raw_handle() as HANDLE, INFINITE) };
        let code = exit_code(&helper);

        // Let the pseudoconsole paint claude's last frame, then close it;
        // the output thread ends at the pipe's EOF.
        let start = Instant::now();
        while start.elapsed() < FINAL_FRAME_WAIT
            && io
                .last_output()
                .is_some_and(|t| t.elapsed() < Duration::from_millis(100))
        {
            thread::sleep(Duration::from_millis(20));
        }
        stop.store(true, Ordering::SeqCst);
        pcon.close();
        let _ = output.join();
        let _ = input.join();
        drop(ctrl);
        drop(raw);
        *lock(&OUTPUT_CARRY) = None;
        self.observer.on_exit();

        Ok((ExitStatus::from_raw(code), ChildHandle { pid, born }))
    }
}

// ─── the helper inside the pseudoconsole ─────────────────────────────────────

/// `csm __conpty-leader <report> <control> <sid> [--set K V]... [--unset K]...
/// -- <argv...>`: run claude attached to the pseudoconsole this process was
/// started in, report its pid, and exit with its exit code. Never reached
/// except through [`ConptyLauncher`]'s own spawn.
pub fn leader_main(args: &[OsString]) -> i32 {
    let a = match LeaderArgs::parse(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("csm {LEADER_WORD}: {e}");
            return 2;
        }
    };
    // SAFETY: csm passed these two handle values and made exactly them
    // inheritable for this process; nothing else here owns them.
    let mut report = unsafe { File::from_raw_handle(a.report as RawHandle) };
    let control = unsafe { File::from_raw_handle(a.control as RawHandle) };
    let say = |report: &mut File, r: Report| {
        let _ = writeln!(report, "{}", r.format()).and_then(|_| report.flush());
    };

    let console = |name: &str| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(name)
    };
    let stdio = (console("CONIN$"), console("CONOUT$"), console("CONOUT$"));
    let (Ok(stdin), Ok(stdout), Ok(stderr)) = stdio else {
        say(
            &mut report,
            Report::Fail("cannot open the pseudoconsole".into()),
        );
        return 127;
    };

    let (bin, rest) = a
        .argv
        .split_first()
        .expect("parse checks argv is non-empty");
    let mut cmd = std::process::Command::new(bin);
    cmd.args(rest);
    a.env.apply(&mut cmd);
    cmd.stdin(stdin).stdout(stdout).stderr(stderr);
    // Own process group so the console control calls can target claude
    // alone, as in direct mode.
    cmd.creation_flags(0x0000_0200);
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            say(&mut report, Report::Fail(format!("spawn claude: {e}")));
            return 127;
        }
    };
    let pid = child.id();
    say(&mut report, Report::Pid(pid));
    drop(report);

    crate::platform::windows::forward_console_ctrl(pid);
    thread::spawn(move || {
        for line in BufReader::new(control).lines().map_while(Result::ok) {
            if line.trim() == CONTROL_BREAK {
                // SAFETY: documented console-control API; `pid` is claude's group.
                unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
            }
        }
    });

    let stop_flag = crate::paths::stop_flag(&a.sid);
    let grace = Duration::from_millis(crate::envvar::u64_or("CLAUDE_SWITCH_GRACE_MS", 5_000));
    let status = crate::platform::windows::supervise(child, &stop_flag, pid, grace);
    crate::platform::windows::stop_forwarding_console_ctrl();
    match status {
        Ok(s) => s.code().unwrap_or(1),
        Err(_) => 1,
    }
}
