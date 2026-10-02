//! The Win32 half of the elevated scan: the pipe, the UAC launch, the helper.

use std::ffi::{OsStr, OsString, c_void};
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{mem, ptr, thread};

use windows_sys::Win32::Foundation::{
    ERROR_CANCELLED, ERROR_PIPE_CONNECTED, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
    WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER,
    TokenElevation, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_INBOUND};
use windows_sys::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
};
use windows_sys::Win32::System::Console::FreeConsole;
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, GetProcessId, INFINITE, OpenProcessToken,
    WaitForSingleObject,
};
use windows_sys::Win32::UI::Shell::{
    SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    ShellExecuteExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

use super::ElevatedScanError;
use super::wire::{self, Frame};
use crate::scan::backend::dispatch;
use crate::scan::{Backend, ScanOptions, ScanProgress, ScanReport, VolumeRoot};
use crate::timings;

const HELPER_FLAG: &str = "--nomnom-elevated-scan";
const DEBUG_WALK_ENV: &str = "NOMNOM_DEBUG_HELPER_WALK";
const PIPE_BUFFER: u32 = 1 << 20;
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

/// Helper exit codes, which the parent reports when the stream breaks.
const EXIT_FAILURE_SENT: u8 = 1;
const EXIT_BAD_LAUNCH: u8 = 2;
const EXIT_PARENT_GONE: u8 = 3;

pub(super) fn is_elevated() -> bool {
    let Some(token) = process_token() else { return false };
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut len = 0;
    // SAFETY: the buffer is a TOKEN_ELEVATION and its exact size is passed.
    let ok = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenElevation,
            (&raw mut elevation).cast(),
            mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
    };
    ok != 0 && elevation.TokenIsElevated != 0
}

pub(super) fn scan_elevated(
    root: &VolumeRoot,
    opts: &ScanOptions,
) -> Result<ScanReport, ElevatedScanError> {
    if is_elevated() {
        return dispatch(root.as_path(), Backend::Mft, opts)
            .map_err(|e| ElevatedScanError::Failed(e.to_string()));
    }
    let started = Instant::now();
    let debug_walk = std::env::var_os(DEBUG_WALK_ENV).is_some_and(|v| v == "1");
    let failed =
        |what: &str, error: io::Error| ElevatedScanError::Failed(format!("{what}: {error}"));

    let name = pipe_name();
    let pipe = create_pipe(&name).map_err(|e| failed("could not create the helper pipe", e))?;
    let exe = std::env::current_exe().map_err(|e| failed("could not locate this program", e))?;
    let parameters = format!(
        "{HELPER_FLAG} {} {} {}",
        name.to_string_lossy(),
        if debug_walk { "walk" } else { "mft" },
        hex_units(root.as_path().as_os_str()),
    );
    let child = launch(&exe, &parameters, if debug_walk { "open" } else { "runas" })?;
    let child = Arc::new(child);

    // ConnectNamedPipe blocks until a client opens the pipe, so a helper that
    // dies before connecting would hang us forever. The waiter connects in its
    // place once the helper exits; the client-PID check below tells the two
    // apart.
    {
        let child = Arc::clone(&child);
        let name = name.clone();
        thread::spawn(move || {
            // SAFETY: the handle stays open for as long as the Arc lives.
            unsafe { WaitForSingleObject(child.as_raw_handle(), INFINITE) };
            let _ = File::options().write(true).open(&name);
        });
    }

    // SAFETY: `pipe` is a live pipe server handle.
    let connected = unsafe { ConnectNamedPipe(pipe.as_raw_handle(), ptr::null_mut()) } != 0
        || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
    if !connected {
        return Err(failed("the helper pipe did not connect", io::Error::last_os_error()));
    }
    let mut client = 0;
    // SAFETY: both handles are live; the out-pointer is a u32.
    let child_pid = unsafe { GetProcessId(child.as_raw_handle()) };
    let ok = unsafe { GetNamedPipeClientProcessId(pipe.as_raw_handle(), &mut client) } != 0;
    if !ok || client != child_pid {
        return Err(ElevatedScanError::Failed(match exit_code(&child, Duration::from_secs(2)) {
            Some(code) => format!("the helper exited with code {code} before connecting"),
            None => format!("process {client} connected to the helper pipe in place of the helper"),
        }));
    }

    let mut waited = timings::lap("helper launch + UAC + connect", started);
    let mut stream = BufReader::with_capacity(PIPE_BUFFER as usize, File::from(pipe));
    let broken = |error: io::Error| {
        let code = exit_code(&child, Duration::from_secs(5)).map_or_else(
            || "is still running".to_string(),
            |code| format!("exited with code {code}"),
        );
        ElevatedScanError::Failed(format!("the helper stream broke ({error}); the helper {code}"))
    };
    wire::read_header(&mut stream).map_err(broken)?;
    loop {
        match wire::read_frame(&mut stream).map_err(broken)? {
            Frame::Progress { entries, total, bytes } => {
                if let Some(progress) = &opts.progress {
                    progress.entries.store(entries, Ordering::Relaxed);
                    progress.entries_total.store(total, Ordering::Relaxed);
                    progress.bytes.store(bytes, Ordering::Relaxed);
                }
                waited = Instant::now();
            }
            Frame::Report(report) => {
                // From the last progress frame, which the helper stops sending
                // once its scan returns: its encode overlaps this decode.
                timings::lap("parent receive + decode (from last progress)", waited);
                timings::lap("parent total in scan_elevated", started);
                return Ok(report);
            }
            Frame::Failure(message) => return Err(ElevatedScanError::Failed(message)),
        }
    }
}

pub(super) fn maybe_run_helper() -> Option<ExitCode> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.first().is_none_or(|flag| flag != HELPER_FLAG) {
        return None;
    }
    // A console program relaunched by ShellExecute gets a console of its own;
    // the helper has nothing to show in it.
    // SAFETY: no preconditions.
    unsafe { FreeConsole() };
    Some(ExitCode::from(run_helper(&args[1..])))
}

fn run_helper(args: &[OsString]) -> u8 {
    timings::join_scan("helper");
    let [pipe, backend, root] = args else { return EXIT_BAD_LAUNCH };
    let backend = match backend.to_str() {
        Some("mft") => Backend::Mft,
        Some("walk") => Backend::Walk,
        _ => return EXIT_BAD_LAUNCH,
    };
    let Some(root) = root.to_str().and_then(units_from_hex) else { return EXIT_BAD_LAUNCH };
    let root = PathBuf::from(OsString::from_wide(&root));
    let Ok(file) = File::options().write(true).open(pipe) else { return EXIT_BAD_LAUNCH };
    let out = Arc::new(Mutex::new(BufWriter::with_capacity(PIPE_BUFFER as usize, file)));

    let send = |write: &dyn Fn(&mut BufWriter<File>) -> io::Result<()>| {
        let mut out = out.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if write(&mut out).and_then(|()| out.flush()).is_err() {
            std::process::exit(EXIT_PARENT_GONE.into());
        }
    };
    send(&|out| wire::write_header(out));

    let root = match VolumeRoot::new(&root) {
        Ok(root) => root,
        Err(_) => {
            let message = format!("not a volume root: {}", root.display());
            send(&|out| wire::write_failure(out, &message));
            return EXIT_FAILURE_SENT;
        }
    };

    let progress = Arc::new(ScanProgress::default());
    let done = Arc::new(AtomicBool::new(false));
    let ticker = {
        let (out, progress, done) = (Arc::clone(&out), Arc::clone(&progress), Arc::clone(&done));
        thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                let mut out = out.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                let frame = wire::write_progress(
                    &mut *out,
                    progress.entries.load(Ordering::Relaxed),
                    progress.entries_total.load(Ordering::Relaxed),
                    progress.bytes.load(Ordering::Relaxed),
                );
                // A failed write means the parent closed its end: nobody will
                // read the rest of this scan.
                if frame.and_then(|()| out.flush()).is_err() {
                    std::process::exit(EXIT_PARENT_GONE.into());
                }
                drop(out);
                thread::sleep(PROGRESS_EVERY);
            }
        })
    };

    let opts = ScanOptions { progress: Some(progress), ..ScanOptions::default() };
    let result = dispatch(root.as_path(), backend, &opts);
    done.store(true, Ordering::Relaxed);
    let _ = ticker.join();
    match result {
        Ok(mut report) => {
            // The catalog sorts entries this way anyway. Sorted here, each path
            // shares most of the one before it, so the stream is a fifth the
            // size and the parent's sort finds the order already in place.
            let started = Instant::now();
            crate::catalog::sort_subtrees(&mut report.entries);
            let started = timings::lap("helper sort for the wire", started);
            send(&|out| wire::write_report(out, &report));
            timings::lap("helper encode + send", started);
            0
        }
        Err(error) => {
            send(&|out| wire::write_failure(out, &error.to_string()));
            EXIT_FAILURE_SENT
        }
    }
}

fn pipe_name() -> OsString {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    format!(r"\\.\pipe\nomnom-elevated-{}-{nanos}", std::process::id()).into()
}

/// A single-instance, inbound pipe whose DACL admits this user and the
/// Administrators group — the elevated helper is one of the two, depending on
/// whether UAC elevated this user or an administrator who typed credentials.
fn create_pipe(name: &OsStr) -> io::Result<OwnedHandle> {
    let sid = user_sid_string()?;
    let sddl = wide(OsStr::new(&format!("D:P(A;;GA;;;{sid})(A;;GA;;;BA)")));
    let mut descriptor = ptr::null_mut();
    // SAFETY: `sddl` is NUL-terminated; the descriptor is LocalFree'd below.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let name = wide(name);
    // SAFETY: `name` is NUL-terminated and `attributes` outlives the call.
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            PIPE_BUFFER,
            PIPE_BUFFER,
            0,
            &attributes,
        )
    };
    let error = io::Error::last_os_error();
    // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
    unsafe { LocalFree(descriptor) };
    owned(handle).ok_or(error)
}

fn user_sid_string() -> io::Result<String> {
    let token = process_token().ok_or_else(io::Error::last_os_error)?;
    let mut len = 0;
    // SAFETY: a size query; it fails with the length it needs.
    unsafe { GetTokenInformation(token.as_raw_handle(), TokenUser, ptr::null_mut(), 0, &mut len) };
    // u64 elements keep the buffer aligned for TOKEN_USER.
    let mut buffer = vec![0u64; (len as usize).div_ceil(8).max(1)];
    // SAFETY: the buffer holds at least `len` bytes.
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            len,
            &mut len,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: GetTokenInformation filled the buffer with a TOKEN_USER.
    let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut text = ptr::null_mut();
    // SAFETY: `sid` points into `buffer`, which is alive; `text` is LocalFree'd.
    if unsafe { ConvertSidToStringSidW(sid, &mut text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a NUL-terminated string from ConvertSidToStringSidW.
    let sid = unsafe {
        let len = (0..).take_while(|&i| *text.add(i) != 0).count();
        String::from_utf16_lossy(std::slice::from_raw_parts(text, len))
    };
    // SAFETY: allocated by ConvertSidToStringSidW.
    unsafe { LocalFree(text.cast::<c_void>()) };
    Ok(sid)
}

fn launch(
    exe: &std::path::Path,
    parameters: &str,
    verb: &str,
) -> Result<OwnedHandle, ElevatedScanError> {
    // ShellExecuteEx may hand the launch to a COM-based shell extension, so
    // this thread needs COM; a caller that already chose a model keeps it.
    // SAFETY: balanced by CoUninitialize when it succeeded.
    let com = unsafe {
        CoInitializeEx(ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32)
    };
    let (verb, file, parameters) =
        (wide(OsStr::new(verb)), wide(exe.as_os_str()), wide(OsStr::new(parameters)));
    // SAFETY: an all-zero SHELLEXECUTEINFOW is valid; the fields set below
    // point at NUL-terminated buffers that outlive the call.
    let mut info: SHELLEXECUTEINFOW = unsafe { mem::zeroed() };
    info.cbSize = mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    info.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI;
    info.lpVerb = verb.as_ptr();
    info.lpFile = file.as_ptr();
    info.lpParameters = parameters.as_ptr();
    info.nShow = SW_HIDE;
    // SAFETY: `info` is fully initialised as above.
    let ok = unsafe { ShellExecuteExW(&mut info) } != 0;
    let error = unsafe { GetLastError() };
    // A failure (RPC_E_CHANGED_MODE: the caller already chose a model) owes
    // no CoUninitialize, and the launch above works either way.
    if com >= 0 {
        // SAFETY: pairs the successful CoInitializeEx above.
        unsafe { CoUninitialize() };
    }
    if !ok {
        return Err(if error == ERROR_CANCELLED {
            ElevatedScanError::Declined
        } else {
            ElevatedScanError::Failed(format!(
                "could not start the elevated helper: {}",
                io::Error::from_raw_os_error(error as i32)
            ))
        });
    }
    owned(info.hProcess)
        .ok_or_else(|| ElevatedScanError::Failed("the shell started no helper process".into()))
}

fn exit_code(process: &OwnedHandle, wait: Duration) -> Option<u32> {
    let millis = u32::try_from(wait.as_millis()).unwrap_or(u32::MAX);
    // SAFETY: a live process handle and a u32 out-pointer.
    unsafe {
        if WaitForSingleObject(process.as_raw_handle(), millis) != WAIT_OBJECT_0 {
            return None;
        }
        let mut code = 0;
        (GetExitCodeProcess(process.as_raw_handle(), &mut code) != 0).then_some(code)
    }
}

fn process_token() -> Option<OwnedHandle> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: the pseudo-handle needs no closing; `token` is owned on success.
    (unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } != 0)
        .then(|| owned(token))
        .flatten()
}

fn owned(handle: HANDLE) -> Option<OwnedHandle> {
    // SAFETY: a valid handle returned to us, closed exactly once by OwnedHandle.
    (!handle.is_null() && handle != INVALID_HANDLE_VALUE)
        .then(|| unsafe { OwnedHandle::from_raw_handle(handle) })
}

fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(Some(0)).collect()
}

/// The root travels as hex UTF-16 units: ShellExecute re-parses the parameter
/// string, and a hex word survives its quoting rules whatever the path holds.
fn hex_units(text: &OsStr) -> String {
    text.encode_wide().map(|unit| format!("{unit:04x}")).collect()
}

fn units_from_hex(text: &str) -> Option<Vec<u16>> {
    if text.is_empty() || !text.len().is_multiple_of(4) || !text.is_ascii() {
        return None;
    }
    (0..text.len()).step_by(4).map(|i| u16::from_str_radix(&text[i..i + 4], 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a root that does not survive the command line, which would
    /// make the helper scan, or refuse, a drive the user never picked.
    #[test]
    fn the_root_parameter_round_trips_and_rejects_garbage() {
        let root = OsString::from_wide(&[0x0044, 0x003a, 0x005c, 0xd800, 0xac00]);
        assert_eq!(units_from_hex(&hex_units(&root)), Some(root.encode_wide().collect()));
        for bad in ["", "004", "zzzz", "0044 ", "００４４"] {
            assert_eq!(units_from_hex(bad), None, "{bad:?}");
        }
    }
}
