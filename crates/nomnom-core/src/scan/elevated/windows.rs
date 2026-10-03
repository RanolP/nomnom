//! The Win32 half of the elevated scan: the pipe, the UAC launch, the helper.

use std::ffi::{OsStr, OsString, c_void};
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{mem, ptr, thread};

use windows_sys::Win32::Foundation::{
    ERROR_CANCELLED, ERROR_PIPE_CONNECTED, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
    WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER,
    TokenElevation, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows_sys::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
};
use windows_sys::Win32::System::Console::FreeConsole;
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
    GetNamedPipeServerProcessId, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
    PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, GetProcessId, INFINITE, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, TerminateProcess, WaitForSingleObject,
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
const TOKEN_BYTES: usize = 32;

/// Helper exit codes, which the parent reports when the stream breaks.
const EXIT_BAD_LAUNCH: u8 = 2;
const EXIT_PARENT_GONE: u8 = 3;
const EXIT_REFUSED_PEER: u8 = 4;

/// The helper this process launched, kept alive so one UAC prompt serves
/// every later scan. The lock also serializes scans: one request at a time.
///
/// A static is never dropped, so the helper's end at exit is its own doing:
/// it watches this process's handle and exits when it signals, and the pipe
/// breaking under it does the same.
static HELPER: Mutex<Option<Helper>> = Mutex::new(None);

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
    let mut slot = HELPER.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    if let Some(code) = slot.as_ref().and_then(Helper::exited) {
        *slot = None;
        let note = format!(
            "the elevated scan helper had exited (code {code}), so this scan starts a new one \
             and asks for Administrator access again"
        );
        eprintln!("nomnom: {note}");
        if let Some(progress) = &opts.progress {
            let _ = progress.relaunch.set(note);
        }
    }
    let helper = match &mut *slot {
        Some(helper) => helper,
        empty => empty.insert(Helper::launch()?),
    };
    timings::lap("helper ready (launch + UAC + connect, or reused)", started);

    match helper.scan(root, opts) {
        Ok(report) => {
            timings::lap("parent total in scan_elevated", started);
            Ok(report)
        }
        Err(Answer::Failed(message)) => Err(ElevatedScanError::Failed(message)),
        Err(Answer::Broken(message)) => {
            // Mid-answer the stream's position is unknown; this helper can
            // serve nothing more.
            *slot = None;
            Err(ElevatedScanError::Failed(format!(
                "{message}; the next scan starts a new elevated helper, which asks for \
                 Administrator access again"
            )))
        }
    }
}

/// Why one request to a live helper produced no report.
enum Answer {
    /// The helper ran the scan and reported its failure; it serves on.
    Failed(String),
    /// The stream broke; the helper is gone or unusable.
    Broken(String),
}

/// A launched, authenticated helper and the server end of its pipe.
struct Helper {
    process: Arc<OwnedHandle>,
    stream: BufReader<File>,
}

impl Helper {
    fn launch() -> Result<Self, ElevatedScanError> {
        let debug_walk = std::env::var_os(DEBUG_WALK_ENV).is_some_and(|v| v == "1");
        let failed =
            |what: &str, error: io::Error| ElevatedScanError::Failed(format!("{what}: {error}"));

        let token: [u8; TOKEN_BYTES] =
            random_bytes().map_err(|e| failed("could not draw the helper token", e))?;
        let name = pipe_name().map_err(|e| failed("could not name the helper pipe", e))?;
        let pipe = create_pipe(&name).map_err(|e| failed("could not create the helper pipe", e))?;
        let exe = std::env::current_exe().map_err(|e| failed("could not locate this program", e))?;
        let parameters = format!(
            "{HELPER_FLAG} {} {} {} {}",
            name.to_string_lossy(),
            if debug_walk { "walk" } else { "mft" },
            std::process::id(),
            hex_bytes(&token),
        );
        let child = Arc::new(launch(&exe, &parameters, if debug_walk { "open" } else { "runas" })?);

        // ConnectNamedPipe blocks until a client opens the pipe, so a helper
        // that dies before connecting would hang us forever. The waiter
        // connects in its place once the helper exits; the client-PID check
        // tells the two apart. After a real connection its open finds the one
        // instance busy and does nothing.
        {
            let child = Arc::clone(&child);
            let name = name.clone();
            thread::spawn(move || {
                // SAFETY: the handle stays open for as long as the Arc lives.
                unsafe { WaitForSingleObject(child.as_raw_handle(), INFINITE) };
                let _ = File::options().write(true).open(&name);
            });
        }

        // SAFETY: a live process handle.
        let child_pid = unsafe { GetProcessId(child.as_raw_handle()) };
        if let Err(peer) = accept_client(&pipe, child_pid) {
            return Err(ElevatedScanError::Failed(match exit_code(&child, Duration::from_secs(2)) {
                Some(code) => format!("the helper exited with code {code} before connecting"),
                None => format!("{peer} in place of the helper"),
            }));
        }

        let mut helper =
            Self { process: child, stream: BufReader::with_capacity(PIPE_BUFFER as usize, File::from(pipe)) };
        let handshake = helper
            .stream
            .get_mut()
            .write_all(&token)
            .and_then(|()| wire::read_header(&mut helper.stream));
        handshake.map_err(|error| ElevatedScanError::Failed(helper.broken(&error)))?;
        Ok(helper)
    }

    fn exited(&self) -> Option<u32> {
        exit_code(&self.process, Duration::ZERO)
    }

    fn broken(&self, error: &io::Error) -> String {
        let code = exit_code(&self.process, Duration::from_secs(5)).map_or_else(
            || "is still running".to_string(),
            |code| format!("exited with code {code}"),
        );
        format!("the helper stream broke ({error}); the helper {code}")
    }

    fn scan(&mut self, root: &VolumeRoot, opts: &ScanOptions) -> Result<ScanReport, Answer> {
        let sent = wire::write_request(self.stream.get_mut(), root.as_path());
        sent.map_err(|error| Answer::Broken(self.broken(&error)))?;
        let mut waited = Instant::now();
        loop {
            let frame = wire::read_frame(&mut self.stream);
            match frame.map_err(|error| Answer::Broken(self.broken(&error)))? {
                Frame::Progress { entries, total, bytes } => {
                    if let Some(progress) = &opts.progress {
                        progress.entries.store(entries, Ordering::Relaxed);
                        progress.entries_total.store(total, Ordering::Relaxed);
                        progress.bytes.store(bytes, Ordering::Relaxed);
                    }
                    waited = Instant::now();
                }
                Frame::Report(report) => {
                    // From the last progress frame, which the helper stops
                    // sending once its scan returns: its encode overlaps this
                    // decode.
                    timings::lap("parent receive + decode (from last progress)", waited);
                    return Ok(report);
                }
                Frame::Failure(message) => return Err(Answer::Failed(message)),
            }
        }
    }
}

impl Drop for Helper {
    /// Disconnecting breaks the helper's pipe, which ends it; one that does
    /// not end promptly is killed, so no admin process outlives its use.
    fn drop(&mut self) {
        // SAFETY: the pipe handle is live until `self.stream` drops after this.
        unsafe { DisconnectNamedPipe(self.stream.get_ref().as_raw_handle()) };
        if exit_code(&self.process, Duration::from_secs(2)).is_none() {
            // SAFETY: a live process handle. May be denied for an elevated
            // child; the helper's own parent watch still ends it then.
            unsafe { TerminateProcess(self.process.as_raw_handle(), u32::from(EXIT_PARENT_GONE)) };
        }
    }
}

/// Waits for the one client of `pipe` and accepts it only when it is process
/// `expected`; otherwise says who connected.
fn accept_client(pipe: &OwnedHandle, expected: u32) -> Result<(), String> {
    // SAFETY: `pipe` is a live pipe server handle.
    let connected = unsafe { ConnectNamedPipe(pipe.as_raw_handle(), ptr::null_mut()) } != 0
        || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
    if !connected {
        return Err(format!("the helper pipe did not connect: {}", io::Error::last_os_error()));
    }
    let mut client = 0;
    // SAFETY: a live pipe handle and a u32 out-pointer.
    let ok = unsafe { GetNamedPipeClientProcessId(pipe.as_raw_handle(), &mut client) } != 0;
    if !ok || client != expected {
        return Err(format!("process {client} connected to the helper pipe"));
    }
    Ok(())
}

/// The helper's half of the handshake: the pipe's server must be process
/// `parent`, and its first bytes must be the token from the command line.
fn authenticate(pipe: &File, parent: u32, token: &[u8; TOKEN_BYTES]) -> io::Result<()> {
    let mut server = 0;
    // SAFETY: a live pipe handle and a u32 out-pointer.
    let ok = unsafe { GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut server) } != 0;
    if !ok || server != parent {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("the pipe is served by process {server}, not {parent}"),
        ));
    }
    let mut sent = [0u8; TOKEN_BYTES];
    let mut reader = pipe;
    reader.read_exact(&mut sent)?;
    // Compared without an early exit, so timing says nothing about a prefix.
    if sent.iter().zip(token).fold(0, |diff, (a, b)| diff | (a ^ b)) != 0 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "wrong token"));
    }
    Ok(())
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

/// Serves scan requests from the parent until it closes the pipe or exits.
/// Accepts nothing but a volume root to scan with the backend it was
/// launched with.
fn run_helper(args: &[OsString]) -> u8 {
    timings::join_scan("helper");
    let [pipe, backend, parent, token] = args else { return EXIT_BAD_LAUNCH };
    let backend = match backend.to_str() {
        Some("mft") => Backend::Mft,
        Some("walk") => Backend::Walk,
        _ => return EXIT_BAD_LAUNCH,
    };
    let Some(parent) = parent.to_str().and_then(|pid| pid.parse::<u32>().ok()) else {
        return EXIT_BAD_LAUNCH;
    };
    let Some(token) = token.to_str().and_then(bytes_from_hex) else { return EXIT_BAD_LAUNCH };

    // Opened before the pipe: while the pipe's server is alive it owns this
    // PID, and the held handle keeps the PID from being reused after it.
    // SAFETY: plain call; the handle is owned on success.
    let watched =
        owned(unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, parent) });
    let Some(watched) = watched else { return EXIT_BAD_LAUNCH };
    thread::spawn(move || {
        // SAFETY: the handle is owned by this thread for the process's life.
        unsafe { WaitForSingleObject(watched.as_raw_handle(), INFINITE) };
        std::process::exit(EXIT_PARENT_GONE.into());
    });

    let Ok(file) = File::options().read(true).write(true).open(pipe) else {
        return EXIT_BAD_LAUNCH;
    };
    if authenticate(&file, parent, &token).is_err() {
        return EXIT_REFUSED_PEER;
    }
    let Ok(writer) = file.try_clone() else { return EXIT_BAD_LAUNCH };
    let out = Arc::new(Mutex::new(BufWriter::with_capacity(PIPE_BUFFER as usize, writer)));
    let mut requests = BufReader::new(file);

    send(&out, &|out| wire::write_header(out));
    // A read error is the parent closing its end, or speaking out of turn:
    // either way the session is over.
    while let Ok(root) = wire::read_request(&mut requests) {
        serve(&out, backend, &root);
    }
    0
}

type Out = Arc<Mutex<BufWriter<File>>>;

fn send(out: &Out, write: &dyn Fn(&mut BufWriter<File>) -> io::Result<()>) {
    let mut out = out.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if write(&mut out).and_then(|()| out.flush()).is_err() {
        std::process::exit(EXIT_PARENT_GONE.into());
    }
}

fn serve(out: &Out, backend: Backend, root: &Path) {
    let root = match VolumeRoot::new(root) {
        Ok(root) => root,
        Err(_) => {
            let message = format!("not a volume root: {}", root.display());
            send(out, &|out| wire::write_failure(out, &message));
            return;
        }
    };

    let progress = Arc::new(ScanProgress::default());
    let done = Arc::new(AtomicBool::new(false));
    let ticker = {
        let (out, progress, done) = (Arc::clone(out), Arc::clone(&progress), Arc::clone(&done));
        thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                send(&out, &|out| {
                    wire::write_progress(
                        out,
                        progress.entries.load(Ordering::Relaxed),
                        progress.entries_total.load(Ordering::Relaxed),
                        progress.bytes.load(Ordering::Relaxed),
                    )
                });
                thread::sleep(PROGRESS_EVERY);
            }
        })
    };

    let opts = ScanOptions { progress: Some(progress), ..ScanOptions::default() };
    let result = dispatch(root.as_path(), backend, &opts);
    done.store(true, Ordering::Relaxed);
    let _ = ticker.join();
    match result {
        Ok(report) => {
            let started = Instant::now();
            send(out, &|out| wire::write_report(out, &report));
            timings::lap("helper encode + send", started);
        }
        Err(error) => send(out, &|out| wire::write_failure(out, &error.to_string())),
    }
}

fn random_bytes<const N: usize>() -> io::Result<[u8; N]> {
    let mut bytes = [0u8; N];
    // SAFETY: the buffer and its exact length; no algorithm handle with the
    // system-preferred flag.
    let status = unsafe {
        BCryptGenRandom(
            ptr::null_mut(),
            bytes.as_mut_ptr(),
            N as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status < 0 {
        return Err(io::Error::other(format!("BCryptGenRandom failed with NTSTATUS {status:#x}")));
    }
    Ok(bytes)
}

/// Unguessable, so no other process can open or squat it before we do.
fn pipe_name() -> io::Result<OsString> {
    let nonce: [u8; 16] = random_bytes()?;
    Ok(format!(r"\\.\pipe\nomnom-elevated-{}", hex_bytes(&nonce)).into())
}

/// A single-instance, duplex pipe whose DACL admits this user and the
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
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
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

fn launch(exe: &Path, parameters: &str, verb: &str) -> Result<OwnedHandle, ElevatedScanError> {
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

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn bytes_from_hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N * 2 || !text.is_ascii() {
        return None;
    }
    let mut bytes = [0u8; N];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a token that does not survive the command line, which would
    /// make every helper refuse its own parent.
    #[test]
    fn the_token_parameter_round_trips_and_rejects_garbage() {
        let token: [u8; TOKEN_BYTES] = random_bytes().unwrap();
        assert_eq!(bytes_from_hex::<TOKEN_BYTES>(&hex_bytes(&token)), Some(token));
        for bad in ["", "0", "zz", "00 ", "０"] {
            assert_eq!(bytes_from_hex::<1>(bad), None, "{bad:?}");
        }
    }

    /// One pipe with this process on both ends: the server end as the parent
    /// holds it, the client end as the helper opens it.
    fn pipe_pair() -> (OwnedHandle, File) {
        let name = pipe_name().unwrap();
        let server = create_pipe(&name).unwrap();
        let client = File::options().read(true).write(true).open(&name).unwrap();
        (server, client)
    }

    /// Catches an elevated helper that would serve whoever reaches its pipe:
    /// a peer with the wrong token, a pipe served by a process other than the
    /// one that launched it, or a parent that accepts the wrong client.
    #[test]
    fn the_handshake_refuses_a_wrong_token_or_a_wrong_peer() {
        let me = std::process::id();
        let token: [u8; TOKEN_BYTES] = random_bytes().unwrap();

        let (server, client) = pipe_pair();
        accept_client(&server, me).expect("this process is the client");
        File::from(server).write_all(&token).unwrap();
        authenticate(&client, me, &token).expect("the right parent with the right token");

        let (server, client) = pipe_pair();
        accept_client(&server, me).unwrap();
        let mut wrong = token;
        wrong[TOKEN_BYTES - 1] ^= 1;
        File::from(server).write_all(&wrong).unwrap();
        let refused = authenticate(&client, me, &token).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied, "{refused}");

        let (server, client) = pipe_pair();
        accept_client(&server, me).unwrap();
        File::from(server).write_all(&token).unwrap();
        let refused = authenticate(&client, me.wrapping_add(4), &token).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied, "{refused}");

        let (server, _client) = pipe_pair();
        let refused = accept_client(&server, me.wrapping_add(4)).unwrap_err();
        assert!(refused.contains(&me.to_string()), "{refused}");
    }
}
