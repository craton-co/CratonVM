// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Windows attach transport. The embedded DLL owns only a bounded IPC queue;
//! the owning listener supplies its VM's command table on ordinary threads.
use super::{execute_attach_operation, read_attach_request, DiagnosticCommand};
use std::io::{Read, Write};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[repr(C)]
struct Request {
    version: i32,
    command: [u8; 17],
    args: [[u8; 1025]; 3],
    pipe: [u8; 257],
}
type Claim = unsafe extern "system" fn() -> i32;
type Release = unsafe extern "system" fn();
type Take = unsafe extern "system" fn(*mut Request) -> i32;

#[link(name = "kernel32")]
extern "system" {
    fn GetModuleHandleExW(
        flags: u32,
        address: *const u16,
        module: *mut *mut std::ffi::c_void,
    ) -> i32;
    fn PeekNamedPipe(
        pipe: *mut std::ffi::c_void,
        buffer: *mut std::ffi::c_void,
        size: u32,
        read: *mut u32,
        available: *mut u32,
        left: *mut u32,
    ) -> i32;
}

pub(super) struct WindowsAttachTarget {
    shutdown: Arc<AtomicBool>,
    /// Latched (`cratonvm_types::latched_thread`): `Drop` can run in a
    /// thread-local destructor, where an OS join deadlocks.
    thread: Option<cratonvm_types::latched_thread::LatchedJoinHandle<()>>,
    release: Release,
}

impl WindowsAttachTarget {
    pub(super) fn start(
        commands: Arc<parking_lot::RwLock<Vec<DiagnosticCommand>>>,
    ) -> Result<Self, String> {
        let path = transport_dll()?;
        // SAFETY: the file's bytes are verified against the transport compiled
        // into this binary. Every symbol below has that DLL's exact C ABI.
        let library = unsafe { libloading::Library::new(path) }.map_err(|e| e.to_string())?;
        let (claim, release, take) = unsafe {
            (
                *library
                    .get::<Claim>(b"CratonAttachClaim\0")
                    .map_err(|e| e.to_string())?,
                *library
                    .get::<Release>(b"CratonAttachRelease\0")
                    .map_err(|e| e.to_string())?,
                *library
                    .get::<Take>(b"CratonAttachTake\0")
                    .map_err(|e| e.to_string())?,
            )
        };
        // Injected clients may still be returning through the DLL during VM
        // teardown. Pin its code until process exit; Release clears the queue
        // and its ownership, so another embedded VM can subsequently claim it.
        let mut module = std::ptr::null_mut();
        if unsafe { GetModuleHandleExW(0x1 | 0x4, take as *const u16, &mut module) } == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        if unsafe { claim() } == 0 {
            return Err("the process attach endpoint already belongs to another VM".into());
        }
        let shutdown = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&shutdown);
        let thread = cratonvm_types::latched_thread::spawn_latched(
            std::thread::Builder::new().name("Attach-Listener".into()),
            move || {
                while !stop.load(Ordering::Acquire) {
                    let mut request: Request = unsafe { std::mem::zeroed() };
                    if unsafe { take(&mut request) } != 0 {
                        let commands = Arc::clone(&commands);
                        let _ = std::thread::Builder::new()
                            .name("Attach-Connection".into())
                            .spawn(move || serve(request, &commands));
                    } else {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                }
            },
        );
        match thread {
            Ok(thread) => Ok(Self {
                shutdown,
                thread: Some(thread),
                release,
            }),
            Err(error) => {
                unsafe { release() };
                Err(error.to_string())
            }
        }
    }
}

impl Drop for WindowsAttachTarget {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        unsafe { (self.release)() };
    }
}

fn text(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Content-addressed per-user native-code cache. Cargo-built and installed
/// launchers, renamed binaries and embedders all carry their own DLL bytes;
/// no build-machine path or host HotSpot jvm.dll is used.
fn transport_dll() -> Result<std::path::PathBuf, String> {
    use std::hash::{Hash, Hasher};
    const DLL: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/jvm.dll"));
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    DLL.hash(&mut hash);
    let root = std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA is unavailable")?;
    let directory = std::path::PathBuf::from(root)
        .join("CratonVM")
        .join("attach")
        .join(format!("{:016x}", hash.finish()));
    std::fs::create_dir_all(&directory).map_err(|e| e.to_string())?;
    let path = directory.join("jvm.dll");
    if !path.exists() {
        let staging = directory.join(format!("jvm-{}.tmp", std::process::id()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)
            .map_err(|e| e.to_string())?;
        file.write_all(DLL).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        drop(file);
        let rename = std::fs::rename(&staging, &path);
        let _ = std::fs::remove_file(staging);
        if let Err(error) = rename {
            if !path.exists() {
                return Err(error.to_string());
            }
        }
    }
    if std::fs::read(&path).map_err(|e| e.to_string())? != DLL {
        return Err("attach transport cache does not match this binary".into());
    }
    Ok(path)
}

struct Pipe {
    file: std::fs::File,
    deadline: std::time::Instant,
}
impl Read for Pipe {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        use std::os::windows::io::AsRawHandle;
        if bytes.is_empty() {
            return Ok(0);
        }
        loop {
            let mut available = 0;
            // SAFETY: a live named-pipe handle, valid output, no read buffer.
            if unsafe {
                PeekNamedPipe(
                    self.file.as_raw_handle(),
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut available,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            if available != 0 {
                let count = bytes.len().min(available as usize);
                return self.file.read(&mut bytes[..count]);
            }
            if std::time::Instant::now() >= self.deadline {
                return Err(std::io::ErrorKind::TimedOut.into());
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

fn serve(request: Request, commands: &parking_lot::RwLock<Vec<DiagnosticCommand>>) {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).read(request.version == 2);
    let Ok(file) = options.open(text(&request.pipe)) else {
        return;
    };
    let mut pipe = Pipe {
        file,
        deadline: std::time::Instant::now() + std::time::Duration::from_secs(5),
    };
    let fields = if request.version == 2 {
        match read_attach_request(&mut pipe) {
            Ok(fields) => fields,
            Err(_) => return,
        }
    } else {
        vec![
            "1".into(),
            text(&request.command),
            text(&request.args[0]),
            text(&request.args[1]),
            text(&request.args[2]),
        ]
    };
    let (code, body) = execute_attach_operation(&fields, commands);
    let _ = write!(pipe.file, "{code}\n{body}");
}
