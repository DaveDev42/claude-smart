//! Test-only Windows counterpart of `pty_harness`: hosts a program (csm)
//! inside a pseudoconsole the test controls, the way Windows Terminal hosts a
//! shell, so csm sees real console handles on stdin and stdout. Never built
//! into a release artifact; a no-op off Windows.
//!
//! Usage: `conpty_harness <program> [args...]`
//!
//! - `CONPTY_HARNESS_SIZE=<rows>x<cols>`: the pseudoconsole's initial size
//!   (default 24x80).
//! - `CONPTY_HARNESS_PID_FILE=<path>`: where the program's pid is written
//!   once it runs.
//!
//! Everything the pseudoconsole prints is copied, raw, to this process's
//! stdout. Commands arrive on stdin, one per line:
//!
//! - `W <hex>`: write these bytes to the pseudoconsole's input (keys typed
//!   at the terminal).
//! - `R <rows> <cols>`: resize the pseudoconsole (the terminal window).
//! - `Q`: close the pseudoconsole (the terminal window was closed).
//!
//! This process exits with the program's exit code.
//!
//! The program is not attached directly: this harness re-runs itself as
//! `conpty_harness --inner <program> [args...]` inside the pseudoconsole, and
//! that inner copy starts the program with `CONIN$`/`CONOUT$` as its stdio
//! through `std::process::Command`. That keeps the test's own redirected
//! stdio out of the program and spares this file a hand-written argv quoter
//! for arbitrary arguments.

#[cfg(windows)]
fn main() {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if args.get(1).is_some_and(|a| a == "--inner") {
        inner(&args[2..]);
    } else {
        outer(&args[1..]);
    }
}

#[cfg(windows)]
fn inner(args: &[std::ffi::OsString]) {
    use windows_sys::Win32::Foundation::TRUE;
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

    // A Ctrl-C typed before csm puts the console in raw mode must not end
    // this process (it would orphan csm's session).
    unsafe { SetConsoleCtrlHandler(None, TRUE) };
    let console = |name: &str| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(name)
            .expect("conpty_harness: open console")
    };
    let (program, rest) = args.split_first().expect("usage: conpty_harness <program>");
    let mut child = std::process::Command::new(program)
        .args(rest)
        .stdin(console("CONIN$"))
        .stdout(console("CONOUT$"))
        .stderr(console("CONOUT$"))
        .spawn()
        .expect("conpty_harness: spawn program");
    if let Ok(path) = std::env::var("CONPTY_HARNESS_PID_FILE") {
        let tmp = format!("{path}.tmp");
        let _ = std::fs::write(&tmp, child.id().to_string());
        let _ = std::fs::rename(&tmp, &path);
    }
    let code = child.wait().map(|s| s.code().unwrap_or(1)).unwrap_or(1);
    std::process::exit(code);
}

#[cfg(windows)]
fn outer(args: &[std::ffi::OsString]) {
    use std::io::{BufRead, Read, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::sync::{Arc, Mutex};

    use windows_sys::Win32::Foundation::{CloseHandle, FALSE, HANDLE};
    use windows_sys::Win32::System::Console::{
        COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
    };
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
        GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
        LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION,
        STARTF_USESTDHANDLES, STARTUPINFOEXW, UpdateProcThreadAttribute, WaitForSingleObject,
    };

    let (rows, cols) = std::env::var("CONPTY_HARNESS_SIZE")
        .ok()
        .as_deref()
        .and_then(|v| v.split_once('x'))
        .and_then(|(r, c)| Some((r.parse::<i16>().ok()?, c.parse::<i16>().ok()?)))
        .unwrap_or((24, 80));

    let pipe = || unsafe {
        let mut r: HANDLE = std::ptr::null_mut();
        let mut w: HANDLE = std::ptr::null_mut();
        assert!(
            CreatePipe(&mut r, &mut w, std::ptr::null(), 0) != 0,
            "CreatePipe"
        );
        (
            OwnedHandle::from_raw_handle(r as _),
            OwnedHandle::from_raw_handle(w as _),
        )
    };
    let (in_r, in_w) = pipe();
    let (out_r, out_w) = pipe();
    let mut hpc: HPCON = 0;
    let hr = unsafe {
        CreatePseudoConsole(
            COORD { X: cols, Y: rows },
            in_r.as_raw_handle() as HANDLE,
            out_w.as_raw_handle() as HANDLE,
            0,
            &mut hpc,
        )
    };
    assert_eq!(hr, 0, "CreatePseudoConsole");
    drop(in_r);
    drop(out_w);
    let pcon: Arc<Mutex<Option<HPCON>>> = Arc::new(Mutex::new(Some(hpc)));

    // Output: pseudoconsole -> our stdout, raw.
    let mut out_file = std::fs::File::from(out_r);
    let output = std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        let mut stdout = std::io::stdout();
        loop {
            match out_file.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let _ = stdout.write_all(&buf[..n]);
                    let _ = stdout.flush();
                }
            }
        }
    });

    // Our own exe, re-run as `--inner <program> [args...]`.
    let quote = |s: &std::ffi::OsStr, out: &mut Vec<u16>| {
        // Test-controlled paths and flags only: quote when there is a space
        // or a quote, escaping quotes and the backslashes before them.
        let w: Vec<u16> = s.encode_wide().collect();
        if !out.is_empty() {
            out.push(b' ' as u16);
        }
        let plain = !w.is_empty() && !w.iter().any(|&c| c == b' ' as u16 || c == b'"' as u16);
        if plain {
            out.extend(w);
            return;
        }
        out.push(b'"' as u16);
        let mut bs = 0;
        for c in w {
            if c == b'\\' as u16 {
                bs += 1;
            } else {
                if c == b'"' as u16 {
                    out.extend(std::iter::repeat_n(b'\\' as u16, bs + 1));
                }
                bs = 0;
            }
            out.push(c);
        }
        out.extend(std::iter::repeat_n(b'\\' as u16, bs));
        out.push(b'"' as u16);
    };
    let exe = std::env::current_exe().expect("current_exe");
    let mut cmdline = Vec::new();
    quote(exe.as_os_str(), &mut cmdline);
    quote(std::ffi::OsStr::new("--inner"), &mut cmdline);
    for a in args {
        quote(a, &mut cmdline);
    }
    cmdline.push(0);

    let process = unsafe {
        let mut size = 0usize;
        InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size);
        let mut storage = vec![0u64; size.div_ceil(8)];
        let list = storage.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        assert!(InitializeProcThreadAttributeList(list, 1, 0, &mut size) != 0);
        assert!(
            UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                hpc as *const std::ffi::c_void,
                std::mem::size_of::<HPCON>(),
                std::ptr::null_mut(),
                std::ptr::null(),
            ) != 0
        );
        let mut si: STARTUPINFOEXW = std::mem::zeroed();
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.lpAttributeList = list;
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let ok = CreateProcessW(
            std::ptr::null(),
            cmdline.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            FALSE,
            EXTENDED_STARTUPINFO_PRESENT,
            std::ptr::null(),
            std::ptr::null(),
            &si.StartupInfo,
            &mut pi,
        );
        DeleteProcThreadAttributeList(list);
        assert!(
            ok != 0,
            "CreateProcessW: {}",
            std::io::Error::last_os_error()
        );
        CloseHandle(pi.hThread);
        OwnedHandle::from_raw_handle(pi.hProcess as _)
    };

    // Commands on stdin.
    let input = std::sync::Mutex::new(std::fs::File::from(in_w));
    {
        let pcon = Arc::clone(&pcon);
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines().map_while(Result::ok) {
                let line = line.trim();
                if let Some(hex) = line.strip_prefix("W ") {
                    let bytes: Vec<u8> = (0..hex.len() / 2)
                        .filter_map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok())
                        .collect();
                    let mut f = input.lock().unwrap();
                    let _ = f.write_all(&bytes);
                    let _ = f.flush();
                } else if let Some(rc) = line.strip_prefix("R ") {
                    let mut it = rc.split_whitespace().filter_map(|v| v.parse::<i16>().ok());
                    if let (Some(r), Some(c)) = (it.next(), it.next())
                        && let Some(h) = *pcon.lock().unwrap()
                    {
                        unsafe { ResizePseudoConsole(h, COORD { X: c, Y: r }) };
                    }
                } else if line == "Q"
                    && let Some(h) = pcon.lock().unwrap().take()
                {
                    unsafe { ClosePseudoConsole(h) };
                }
            }
        });
    }

    unsafe { WaitForSingleObject(process.as_raw_handle() as HANDLE, INFINITE) };
    let mut code = 1u32;
    unsafe { GetExitCodeProcess(process.as_raw_handle() as HANDLE, &mut code) };
    std::thread::sleep(std::time::Duration::from_millis(300));
    if let Some(h) = pcon.lock().unwrap().take() {
        unsafe { ClosePseudoConsole(h) };
    }
    let _ = output.join();
    std::process::exit(code as i32);
}

#[cfg(not(windows))]
fn main() {}
