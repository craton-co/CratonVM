// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Per-call-site capability gates for the `native-builtins` native surface.
//!
//! # What this is
//!
//! `native-api`'s [`CapabilitySet`] is the policy; this module is the set of
//! **gates** that consult it from inside `native-builtins`. It exists as its
//! own module for three reasons:
//!
//! 1. A gate needs a [`CapabilitySet`], and the only way to reach one from a
//!    native is `ctx.vm_capabilities()` — an `Option<Arc<…>>` that every call
//!    site would otherwise have to unwrap by hand, with a different fallback
//!    each time. Getting that fallback wrong in one place is a silent hole.
//! 2. The checked openers on [`FileDescriptorTable`] take `&CapabilitySet`,
//!    not `&dyn NativeContext`. The adaptation is mechanical and belongs in
//!    one place.
//! 3. `grep capability_gate:: native-builtins/src` is then the complete list
//!    of gated sites in this crate, which is what
//!    `docs/security/capability-wiring.md` is derived from.
//!
//! # Nothing here changes behaviour by default
//!
//! With no policy installed for the VM (today's configuration — nothing calls
//! `install_capabilities` yet) every helper takes the `None` arm and calls the
//! exact unchecked operation the call site used before. With a policy in
//! [`CapabilityMode::Permissive`] the check records the use and returns `Ok`.
//! Only [`CapabilityMode::Enforce`] can refuse.
//!
//! # Order of operations
//!
//! Every file/socket helper delegates to the `_checked` opener, which runs the
//! capability check **before the fd is reserved and before the syscall**. A
//! denial therefore consumes no descriptor, creates no file and attempts no
//! connection.
//!
//! # The raw-memory gate is not like the others
//!
//! [`gate_raw_memory`] sits under `Unsafe.get*/put*` at a raw address — a path
//! `java.nio.Bits`, direct `ByteBuffer` and every `Unsafe`-backed JDK
//! collection drive per element. A full [`CapabilitySet::check`] there is a
//! `String` allocation (to build the scope), a `Mutex` acquisition and a
//! `BTreeMap` lookup **per byte**, which is not affordable. See
//! [`gate_raw_memory`] for the memo that makes the permissive path a
//! thread-local load and an integer compare, and for exactly what that costs
//! in audit fidelity.

use std::cell::Cell;
use std::thread::LocalKey;

use cratonvm_native_api::fd_table::{FdCapabilityError, FdId};
use cratonvm_native_api::{
    Capability, CapabilityCheck, CapabilityKind, CapabilityMode, NativeContext, Scope,
};
use cratonvm_types::error::MethodCallFailed;

// ---------------------------------------------------------------------------
// File openers
// ---------------------------------------------------------------------------

/// `fd_table().open_read(path)`, gated on `FileRead(path)`.
///
/// `#[track_caller]` so the audit report names the *native* that opened the
/// file, not this helper.
#[track_caller]
pub fn open_read_gated(ctx: &dyn NativeContext, path: &str) -> Result<FdId, FdCapabilityError> {
    match ctx.vm_capabilities() {
        Some(caps) => ctx.fd_table().open_read_checked(&caps, path),
        None => Ok(ctx.fd_table().open_read(path)?),
    }
}

/// `fd_table().open_write(path, append)`, gated on `FileWrite(path)`.
#[track_caller]
pub fn open_write_gated(
    ctx: &dyn NativeContext,
    path: &str,
    append: bool,
) -> Result<FdId, FdCapabilityError> {
    match ctx.vm_capabilities() {
        Some(caps) => ctx.fd_table().open_write_checked(&caps, path, append),
        None => Ok(ctx.fd_table().open_write(path, append)?),
    }
}

/// `fd_table().open_read_write(path, create)`, gated on **both** `FileRead`
/// and `FileWrite` — the fd it hands back can do either.
#[track_caller]
pub fn open_read_write_gated(
    ctx: &dyn NativeContext,
    path: &str,
    create: bool,
) -> Result<FdId, FdCapabilityError> {
    match ctx.vm_capabilities() {
        Some(caps) => ctx.fd_table().open_read_write_checked(&caps, path, create),
        None => Ok(ctx.fd_table().open_read_write(path, create)?),
    }
}

/// `fd_table().open_write_options(path, create, truncate, append)`, gated on
/// **both** `FileRead` and `FileWrite` (the fd it hands back is opened
/// read+write). The opener for `FileChannel.open(.., APPEND)` under
/// `--compatible` (nio20 wave 3, L1): the OS descriptor carries `O_APPEND`,
/// as `UnixChannelFactory`'s does, so every write lands at end-of-file
/// whatever the channel's position, and a positional write / `transferFrom`
/// onto it appends as Linux `pwrite(2)` does on HotSpot.
#[track_caller]
pub fn open_write_options_gated(
    ctx: &dyn NativeContext,
    path: &str,
    create: bool,
    truncate: bool,
    append: bool,
) -> Result<FdId, FdCapabilityError> {
    if let Some(caps) = ctx.vm_capabilities() {
        caps.check(Capability::file_read(path))?;
        caps.check(Capability::file_write(path))?;
    }
    Ok(ctx
        .fd_table()
        .open_write_options(path, create, truncate, append)?)
}

/// `fd_table().open_random_access(path, write)`, gated on `FileRead` plus
/// `FileWrite` when `write`.
#[track_caller]
pub fn open_random_access_gated(
    ctx: &dyn NativeContext,
    path: &str,
    write: bool,
) -> Result<FdId, FdCapabilityError> {
    match ctx.vm_capabilities() {
        Some(caps) => ctx
            .fd_table()
            .open_random_access_checked(&caps, path, write),
        None => Ok(ctx.fd_table().open_random_access(path, write)?),
    }
}

// ---------------------------------------------------------------------------
// Socket openers
// ---------------------------------------------------------------------------

/// `fd_table().open_tcp_connect(addr)`, gated on `Network(addr)`.
#[track_caller]
pub fn open_tcp_connect_gated(
    ctx: &dyn NativeContext,
    addr: &str,
) -> Result<FdId, FdCapabilityError> {
    match ctx.vm_capabilities() {
        Some(caps) => ctx.fd_table().open_tcp_connect_checked(&caps, addr),
        None => Ok(ctx.fd_table().open_tcp_connect(addr)?),
    }
}

/// `fd_table().open_tcp_listener(addr)`, gated on `Network(bind addr)`.
///
/// Binding is its own authority, not a weaker form of connecting: a listener
/// on `0.0.0.0:8080` exposes the host rather than reaching out from it.
#[track_caller]
pub fn open_tcp_listener_gated(
    ctx: &dyn NativeContext,
    addr: &str,
) -> Result<FdId, FdCapabilityError> {
    match ctx.vm_capabilities() {
        Some(caps) => ctx.fd_table().open_tcp_listener_checked(&caps, addr),
        None => Ok(ctx.fd_table().open_tcp_listener(addr)?),
    }
}

/// `fd_table().open_udp(bind_addr)`, gated on `Network`. A `None` bind address
/// checks as `Scope::Any`, which only an unscoped grant admits (fail closed).
#[track_caller]
pub fn open_udp_gated(
    ctx: &dyn NativeContext,
    bind_addr: Option<&str>,
) -> Result<FdId, FdCapabilityError> {
    match ctx.vm_capabilities() {
        Some(caps) => ctx.fd_table().open_udp_checked(&caps, bind_addr),
        None => Ok(ctx.fd_table().open_udp(bind_addr)?),
    }
}

/// [`open_udp_gated`] for a WILDCARD bind, opening a dual-stack AF_INET6
/// socket the way the JDK's `DatagramSocket` does.
///
/// `java.net.DatagramSocket` on JDK 25 is a `DatagramChannel` adaptor, so its
/// wildcard constructors give an AF_INET6 socket with `IPV6_V6ONLY` off —
/// measured, `new DatagramSocket().getLocalAddress()` is
/// `/0:0:0:0:0:0:0:0` on HotSpot 25 and was `/0.0.0.0` here.
///
/// That mismatch is observable, and netty's
/// `DnsNameResolverTest.testAddressAlreadyInUse` is where: it holds a port
/// with a `DatagramSocket`, points a resolver at the same address, and
/// asserts a `BindException`. With the socket on AF_INET and the resolver's
/// channel on dual-stack AF_INET6, the two wildcards did not collide, the
/// second bind SUCCEEDED, and the test saw a query timeout instead of the
/// bind failure it was written to observe.
pub fn open_udp_wildcard_dual_stack_gated(
    ctx: &dyn NativeContext,
    port: u16,
) -> Result<FdId, FdCapabilityError> {
    match ctx.vm_capabilities() {
        Some(caps) => ctx.fd_table().open_udp_dual_stack_checked(&caps, port),
        None => Ok(ctx.fd_table().open_udp_dual_stack_port(port)?),
    }
}

// ---------------------------------------------------------------------------
// Error translation
// ---------------------------------------------------------------------------

/// Turn a gated-open failure into the exception the call site should throw.
///
/// The two halves of [`FdCapabilityError`] are genuinely different and must not
/// be collapsed:
///
/// * a **refusal** happened *before* the syscall and must not be retried, so it
///   surfaces as the `SecurityException` `CapabilityDenied` converts to,
///   carrying the kind, the scope, the VM and the gate's source location;
/// * an **I/O failure** keeps the exact `IOException` message the call site
///   already produced, so nothing that catches or matches on it changes.
///
/// `io_message` is only invoked on the I/O arm, so building the message costs
/// nothing on the success and refusal paths.
pub fn translate_open_failure(
    err: FdCapabilityError,
    io_message: impl FnOnce(&std::io::Error) -> String,
) -> MethodCallFailed {
    match err {
        FdCapabilityError::Denied(denied) => denied.into(),
        FdCapabilityError::Io(io) => translate_bind_io(io, io_message),
    }
}

/// The I/O arm of [`translate_open_failure`], split out so the BIND failures
/// keep their JDK type.
///
/// An unavailable address is `java.net.BindException` on HotSpot, and callers
/// test for it by type rather than by message —
/// `new DatagramSocket(portAlreadyBound)` is specified to throw
/// `SocketException`, and the JDK narrows it to `BindException`. A flat
/// `IOException` here also loses to locale: the Windows text for
/// `WSAEADDRINUSE` is translated, so a message match cannot substitute.
///
/// Windows reports the clash as `WSAEADDRINUSE` normally and as `WSAEACCES`
/// (Rust `PermissionDenied`) when the caller set `SO_REUSEADDR` against an
/// exclusively-held port. Both are `BindException` on HotSpot.
/// [`translate_bind_io`] for a caller that already has a bare `io::Error` —
/// the `rebind` paths, which never go through the capability gate because the
/// socket they replace was gated when it was opened.
pub fn translate_bind_failure(
    io: std::io::Error,
    io_message: impl FnOnce(&std::io::Error) -> String,
) -> MethodCallFailed {
    translate_bind_io(io, io_message)
}

fn translate_bind_io(
    io: std::io::Error,
    io_message: impl FnOnce(&std::io::Error) -> String,
) -> MethodCallFailed {
    use cratonvm_types::error::RuntimeError;
    use std::io::ErrorKind;
    // The BindException message is the bare strerror text (`Address already in
    // use`), measured on HotSpot 25.0.4+7 by `Sbr2W7TailNet.java`:
    // no address, no caller context, no `(os error N)`. `io_message` still
    // describes the non-bind arm.
    match io.kind() {
        ErrorKind::AddrInUse => RuntimeError::BindException {
            message: "Address already in use".to_string(),
        }
        .into(),
        ErrorKind::AddrNotAvailable => RuntimeError::BindException {
            message: "Cannot assign requested address".to_string(),
        }
        .into(),
        ErrorKind::PermissionDenied => RuntimeError::BindException {
            message: "Permission denied".to_string(),
        }
        .into(),
        _ => RuntimeError::IOException {
            message: io_message(&io),
        }
        .into(),
    }
}

// ---------------------------------------------------------------------------
// Bare capability gates (for sites that do not go through the fd table)
// ---------------------------------------------------------------------------

/// Gate a network operation that does **not** allocate through the fd table —
/// e.g. the plain `ServerSocket.bind` path, which binds a `std::net::TcpListener`
/// directly and files it in `servlet::s2_alloc_listener`.
#[track_caller]
pub fn gate_network(ctx: &dyn NativeContext, addr: &str) -> Result<(), MethodCallFailed> {
    ctx.check_capability_or_throw(Capability::network(addr))
}

/// Gate a host-process spawn on `ProcessSpawn(program)`.
///
/// Deliberately independent of `native-io`'s CWD-confinement profile: that
/// profile is opt-in and off by default, so without this gate the capability
/// layer never learns a spawn happened at all (audit row **P3**).
#[track_caller]
pub fn gate_process_spawn(ctx: &dyn NativeContext, program: &str) -> Result<(), MethodCallFailed> {
    ctx.check_capability_or_throw(Capability::process_spawn(program))
}

/// Gate the creation or invocation of an FFM upcall stub on
/// `ForeignUpcall(target)` (audit rows **F3**/**F4**).
#[track_caller]
pub fn gate_foreign_upcall(ctx: &dyn NativeContext, target: &str) -> Result<(), MethodCallFailed> {
    ctx.check_capability_or_throw(Capability::foreign_upcall(target))
}

/// Gate an FFM downcall on `ForeignDowncall(symbol)`.
#[track_caller]
pub fn gate_foreign_downcall(
    ctx: &dyn NativeContext,
    symbol: &str,
) -> Result<(), MethodCallFailed> {
    ctx.check_capability_or_throw(Capability::foreign_downcall(symbol))
}

/// Gate a named raw-memory operation on a path that is **not** per-element hot.
///
/// The FFM `MemorySegment` accessors go through here rather than through
/// [`gate_raw_memory`]: each already takes a policy read-lock and a
/// `SecurityManager` check per call, so the full check is noise, and — more
/// importantly — each accessor reports its **own** name, which the single-slot
/// memo in [`gate_raw_memory`] could not preserve.
#[track_caller]
pub fn gate_raw_memory_named(ctx: &dyn NativeContext, op: &str) -> Result<(), MethodCallFailed> {
    ctx.check_capability_or_throw(Capability::raw_memory(op))
}

/// Gate a native-library load on `LibraryLoad(name)`.
#[track_caller]
pub fn gate_library_load(ctx: &dyn NativeContext, name: &str) -> Result<(), MethodCallFailed> {
    ctx.check_capability_or_throw(Capability::library_load(name))
}

// ---------------------------------------------------------------------------
// Raw memory — the hot path
// ---------------------------------------------------------------------------

/// The scope name every raw-address `Unsafe` get/put reports.
///
/// One name for the whole family, matching item 20 of the ordered work list in
/// `docs/security/native-capabilities.md`: `CapabilityKind::RawMemory` does not
/// distinguish reading from writing, so splitting the scope would produce two
/// audit rows a deployment has to grant together anyway.
pub const RAW_MEMORY_UNSAFE_ADDRESS: &str = "Unsafe.rawAddress";

/// What the raw-memory gate learned about this VM's policy.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RawGate {
    /// No policy installed for this VM. This is today's default and the gate
    /// is then a thread-local load plus an integer compare, forever.
    NoPolicy,
    /// A `Permissive` policy, and this thread has already recorded one use.
    /// Subsequent calls on this thread are transparent — see the fidelity note
    /// on [`gate_raw_memory`].
    PermissiveRecorded,
    /// `Audit` or `Enforce`: every call must reach `CapabilitySet::check`,
    /// because the per-call tally (and, under `Enforce`, the refusal) is the
    /// whole point of those modes.
    Checked,
}

thread_local! {
    /// `(vm_identity, state)` memo for [`gate_raw_memory`]. `Cell` of a `Copy`
    /// payload: no `RefCell` borrow flag, no `Arc` clone, no allocation.
    static RAW_MEMORY_GATE: Cell<Option<(usize, RawGate)>> = const { Cell::new(None) };
}

thread_local! {
    /// The same memo for the `RawMemory(Any)` scope the `ScopedMemoryAccess`
    /// natives report ([`gate_raw_memory_any`]). A second slot, not a shared
    /// one: a verdict recorded for `Unsafe.rawAddress` says nothing about
    /// whether this scope's first use has been recorded.
    static RAW_MEMORY_ANY_GATE: Cell<Option<(usize, RawGate)>> = const { Cell::new(None) };
}

fn raw_memory_slot() -> &'static LocalKey<Cell<Option<(usize, RawGate)>>> {
    &RAW_MEMORY_GATE
}

/// Gate a raw-address memory access on `RawMemory("Unsafe.rawAddress")`.
///
/// # Why this is memoized and the file/socket gates are not
///
/// The file and socket gates are followed by a syscall, so the cost of
/// building a scope and taking the audit lock is noise. This gate is not: it
/// sits under `Unsafe.getByte(long)` / `putByte(long, byte)`, which Netty's
/// pooled direct buffers and `java.nio.Bits` drive one element at a time. A
/// full `CapabilitySet::check` per call is a `String` allocation (`Scope::name`),
/// a `Mutex` acquisition and a `BTreeMap` lookup keyed on that `String` —
/// per byte, and contended across every thread of the VM.
///
/// So the resolved verdict is memoized per `(thread, vm_identity)`:
///
/// | policy | first call on the thread | every later call |
/// |---|---|---|
/// | none installed (**default**) | one `capabilities_for` lookup | TLS load + `usize` compare |
/// | `Permissive` | one full `check` (records the use, its count and first site) | TLS load + `usize` compare |
/// | `Audit` / `Enforce` | full `check` | full `check` |
///
/// # The fidelity this trades away, stated plainly
///
/// Under `Permissive`, `RawMemory` is recorded **once per thread**, not once
/// per access. The audit report therefore names the capability, its scope and
/// its first call site correctly, and under-reports `count`. That is the one
/// number the report's purpose — deriving a least-privilege grant set — does
/// not depend on. `Audit` mode, which exists precisely to price an `Enforce`
/// flip, takes the `Checked` arm and counts every call exactly.
///
/// # Staleness
///
/// The memo assumes a VM installs its policy during init, before Java code
/// runs — which is what item 3 of the work list specifies. A policy installed
/// *after* a thread has already taken a raw-memory path would not be seen by
/// that thread. [`reset_raw_memory_gate_memo`] exists for tests and for any
/// embedder that installs late.
///
/// The clean fix is out of this crate: a `capability::any_capabilities_installed()`
/// backed by a relaxed `AtomicUsize` counter in `native-api` would let this be
/// a single atomic load with no memo and no staleness at all. See
/// `docs/security/capability-wiring.md`.
#[track_caller]
#[inline]
pub fn gate_raw_memory(ctx: &dyn NativeContext, op: &str) -> Result<(), MethodCallFailed> {
    gate_memoized(raw_memory_slot(), ctx, || Capability::raw_memory(op))
}

/// Gate a `ScopedMemoryAccess` bulk or scalar access on `RawMemory(Any)`.
///
/// The `ScopedMemoryAccess` natives sit under every direct `ByteBuffer` bulk
/// copy and every `MemorySegment` access, so Netty's compression and buffer
/// code reaches this per call. It used the full `CapabilitySet::check` --
/// the global policy-index mutex, an `Arc` clone every thread of the VM
/// fights over, then the audit lock -- which `perf` put at 13% of a 64-byte
/// `ByteBuffer.put(int, ByteBuffer, int, int)`. The fidelity trade is the one
/// [`gate_raw_memory`] documents: under `Permissive` the use is recorded once
/// per thread, `Audit` and `Enforce` still check every call.
#[track_caller]
#[inline]
pub fn gate_raw_memory_any(ctx: &dyn NativeContext) -> Result<(), MethodCallFailed> {
    gate_memoized(&RAW_MEMORY_ANY_GATE, ctx, || {
        Capability::of(CapabilityKind::RawMemory, Scope::Any)
    })
}

#[track_caller]
#[inline]
fn gate_memoized(
    slot: &'static LocalKey<Cell<Option<(usize, RawGate)>>>,
    ctx: &dyn NativeContext,
    capability: impl FnOnce() -> Capability,
) -> Result<(), MethodCallFailed> {
    let vm = ctx.vm_identity();
    if let Some((cached_vm, state)) = slot.with(Cell::get) {
        if cached_vm == vm && state != RawGate::Checked {
            return Ok(());
        }
    }
    gate_memoized_slow(slot, ctx, vm, capability())
}

#[track_caller]
#[cold]
fn gate_memoized_slow(
    slot: &'static LocalKey<Cell<Option<(usize, RawGate)>>>,
    ctx: &dyn NativeContext,
    vm: usize,
    capability: Capability,
) -> Result<(), MethodCallFailed> {
    let Some(caps) = ctx.vm_capabilities() else {
        slot.with(|c| c.set(Some((vm, RawGate::NoPolicy))));
        return Ok(());
    };
    let outcome = caps.check_or_throw(capability);
    let state = if caps.mode() == CapabilityMode::Permissive {
        RawGate::PermissiveRecorded
    } else {
        RawGate::Checked
    };
    slot.with(|c| c.set(Some((vm, state))));
    outcome
}

/// Drop this thread's raw-memory memos, so the next call re-resolves the
/// policy. Tests that install a policy after a raw-memory access — and
/// embedders that install one late — must call this.
pub fn reset_raw_memory_gate_memo() {
    raw_memory_slot().with(|c| c.set(None));
    RAW_MEMORY_ANY_GATE.with(|c| c.set(None));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
