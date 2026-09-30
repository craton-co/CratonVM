// SPDX-License-Identifier: Apache-2.0
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
use cratonvm_native_api::{Capability, CapabilityCheck, CapabilityMode, NativeContext};
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
    match io.kind() {
        ErrorKind::AddrInUse => RuntimeError::BindException {
            message: format!("Address already in use: {}", io_message(&io)),
        }
        .into(),
        ErrorKind::AddrNotAvailable => RuntimeError::BindException {
            message: format!("Cannot assign requested address: {}", io_message(&io)),
        }
        .into(),
        ErrorKind::PermissionDenied => RuntimeError::BindException {
            message: format!("Permission denied: {}", io_message(&io)),
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
    let vm = ctx.vm_identity();
    if let Some((cached_vm, state)) = raw_memory_slot().with(Cell::get) {
        if cached_vm == vm && state != RawGate::Checked {
            return Ok(());
        }
    }
    gate_raw_memory_slow(ctx, op, vm)
}

#[track_caller]
#[cold]
fn gate_raw_memory_slow(
    ctx: &dyn NativeContext,
    op: &str,
    vm: usize,
) -> Result<(), MethodCallFailed> {
    let Some(caps) = ctx.vm_capabilities() else {
        raw_memory_slot().with(|c| c.set(Some((vm, RawGate::NoPolicy))));
        return Ok(());
    };
    let outcome = caps.check_or_throw(Capability::raw_memory(op));
    let state = if caps.mode() == CapabilityMode::Permissive {
        RawGate::PermissiveRecorded
    } else {
        RawGate::Checked
    };
    raw_memory_slot().with(|c| c.set(Some((vm, state))));
    outcome
}

/// Drop this thread's raw-memory memo, so the next call re-resolves the
/// policy. Tests that install a policy after a raw-memory access — and
/// embedders that install one late — must call this.
pub fn reset_raw_memory_gate_memo() {
    raw_memory_slot().with(|c| c.set(None));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{mock_ctx, MockNativeContext};
    // `fd_table` is declared on `NativeSystemAccess`, not `NativeContext`; the
    // trait must be in scope for the mock to expose it.
    use cratonvm_native_api::NativeSystemAccess;
    use cratonvm_native_api::{
        capability_audit, install_capabilities, uninstall_capabilities, CapabilityKind,
        CapabilitySet, VmId,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Hands out a VM identity nothing else in the suite uses.
    ///
    /// `install_capabilities` is a process-global index keyed on
    /// `vm_identity()`, and every other mock context in the crate reports the
    /// default `0`. Installing an `Enforce` policy under `0` would be visible
    /// to every test running in parallel and could refuse *their* I/O. A
    /// private identity per test keeps the blast radius at zero and lets these
    /// tests run unserialized.
    fn ctx_with_private_vm() -> (MockNativeContext, VmId) {
        static NEXT: AtomicUsize = AtomicUsize::new(0xCA9_0001);
        let raw = NEXT.fetch_add(1, Ordering::Relaxed);
        let ctx = mock_ctx();
        ctx.set_vm_identity(raw);
        (ctx, VmId::from_raw(raw))
    }

    /// Installs a policy for one VM and removes it on drop.
    struct PolicyGuard {
        vm: VmId,
    }

    impl PolicyGuard {
        fn install(set: CapabilitySet) -> PolicyGuard {
            let vm = set.vm();
            install_capabilities(Arc::new(set));
            // The raw-memory memo is keyed by VM, so a private identity is
            // already a miss — reset anyway so a policy re-installed for the
            // same VM inside one test is picked up.
            reset_raw_memory_gate_memo();
            PolicyGuard { vm }
        }
    }

    impl Drop for PolicyGuard {
        fn drop(&mut self) {
            uninstall_capabilities(self.vm);
            reset_raw_memory_gate_memo();
        }
    }

    fn policy(vm: VmId, mode: CapabilityMode, grants: &str) -> CapabilitySet {
        let mut set = CapabilitySet::new(vm, mode);
        if !grants.is_empty() {
            let bad = set.grant_from_list(grants);
            assert!(bad.is_empty(), "unparseable grants: {bad:?}");
        }
        set
    }

    static TEST_SEQ: AtomicUsize = AtomicUsize::new(0);

    fn temp_path(tag: &str) -> String {
        let n = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!(
                "cratonvm_capgate_{}_{}_{}.bin",
                std::process::id(),
                n,
                tag
            ))
            .to_string_lossy()
            .into_owned()
    }

    fn write_file(path: &str, bytes: &[u8]) {
        std::fs::write(path, bytes).expect("temp file must be writable");
    }

    /// How many checks the audit recorded for `kind`, across every scope.
    fn recorded(vm: VmId, kind: CapabilityKind) -> u64 {
        capability_audit(vm)
            .map(|r| {
                r.uses
                    .iter()
                    .filter(|u| u.capability.kind() == kind)
                    .map(|u| u.count)
                    .sum()
            })
            .unwrap_or(0)
    }

    // -----------------------------------------------------------------------
    // No policy at all — the configuration the whole suite runs in today
    // -----------------------------------------------------------------------

    #[test]
    fn no_policy_installed_means_the_helpers_are_the_raw_openers() {
        let (ctx, vm) = ctx_with_private_vm();
        let path = temp_path("nopolicy");
        write_file(&path, b"craton");

        let fd = open_read_gated(&ctx, &path).expect("read must succeed with no policy");
        let mut buf = [0u8; 6];
        let n = ctx.fd_table().read_bytes(fd, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"craton");
        let _ = ctx.fd_table().close(fd);

        gate_raw_memory(&ctx, RAW_MEMORY_UNSAFE_ADDRESS)
            .expect("raw memory is ungated when no policy is installed");
        assert!(
            capability_audit(vm).is_none(),
            "no policy means nothing to report"
        );
        let _ = std::fs::remove_file(&path);
    }

    // -----------------------------------------------------------------------
    // Permissive — allows, and is observably identical to the ungated path
    // -----------------------------------------------------------------------

    #[test]
    fn permissive_allows_a_read_and_changes_nothing_observable() {
        let (ctx, vm) = ctx_with_private_vm();
        let path = temp_path("permissive");
        write_file(&path, b"0123456789");

        // Baseline: the raw opener, no policy installed.
        let mut baseline = [0u8; 10];
        let baseline_len = {
            let fd = ctx.fd_table().open_read(&path).unwrap();
            let n = ctx.fd_table().read_bytes(fd, &mut baseline).unwrap();
            let _ = ctx.fd_table().close(fd);
            n
        };

        // The same operation through the gate under a `Permissive` policy with
        // no grants at all: same success, same length, same bytes.
        let _guard = PolicyGuard::install(policy(vm, CapabilityMode::Permissive, ""));
        let fd = open_read_gated(&ctx, &path).expect("permissive must allow");
        let mut gated = [0u8; 10];
        let n = ctx.fd_table().read_bytes(fd, &mut gated).unwrap();
        let _ = ctx.fd_table().close(fd);

        assert_eq!(n, baseline_len, "gated read returned a different length");
        assert_eq!(gated, baseline, "gated read returned different bytes");
        assert!(
            recorded(vm, CapabilityKind::FileRead) >= 1,
            "permissive must still record the file-read"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn track_caller_names_the_calling_native_not_the_helper() {
        let (ctx, vm) = ctx_with_private_vm();
        let path = temp_path("site");
        write_file(&path, b"x");

        let _guard = PolicyGuard::install(policy(vm, CapabilityMode::Permissive, ""));
        let fd = open_read_gated(&ctx, &path).unwrap();
        let _ = ctx.fd_table().close(fd);

        let report = capability_audit(vm).expect("a policy is installed");
        let row = report
            .uses
            .iter()
            .find(|u| u.capability.kind() == CapabilityKind::FileRead)
            .expect("file-read must appear in the report");
        assert!(
            row.first_site.file.ends_with("capability_gate.rs"),
            "the recorded site should be the caller, got {}",
            row.first_site
        );
        let _ = std::fs::remove_file(&path);
    }

    // -----------------------------------------------------------------------
    // Audit — allows, records, and prices the Enforce flip
    // -----------------------------------------------------------------------

    #[test]
    fn audit_allows_but_tallies_the_ungranted_use() {
        let (ctx, vm) = ctx_with_private_vm();
        let path = temp_path("audit");
        write_file(&path, b"audit");

        let _guard = PolicyGuard::install(policy(vm, CapabilityMode::Audit, ""));
        let fd = open_read_gated(&ctx, &path).expect("audit mode must still allow");
        let _ = ctx.fd_table().close(fd);

        let report = capability_audit(vm).expect("a policy is installed");
        assert_eq!(report.mode, CapabilityMode::Audit);
        assert!(report.total_checks() >= 1, "audit mode must record the use");
        assert!(
            report.total_ungranted() >= 1,
            "with no grants, audit must price the Enforce flip"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn audit_counts_every_raw_memory_access_not_just_the_first() {
        let (ctx, vm) = ctx_with_private_vm();
        let _guard = PolicyGuard::install(policy(vm, CapabilityMode::Audit, ""));

        for _ in 0..5 {
            gate_raw_memory(&ctx, RAW_MEMORY_UNSAFE_ADDRESS).expect("audit allows");
        }
        assert_eq!(
            recorded(vm, CapabilityKind::RawMemory),
            5,
            "Audit mode takes the Checked arm and must not memoize"
        );
    }

    #[test]
    fn permissive_raw_memory_records_the_capability_then_goes_transparent() {
        let (ctx, vm) = ctx_with_private_vm();
        let _guard = PolicyGuard::install(policy(vm, CapabilityMode::Permissive, ""));

        for _ in 0..64 {
            gate_raw_memory(&ctx, RAW_MEMORY_UNSAFE_ADDRESS).expect("permissive allows");
        }
        // Present in the report (so the capability is discoverable) but counted
        // once per thread — the documented trade on `gate_raw_memory`.
        assert_eq!(
            recorded(vm, CapabilityKind::RawMemory),
            1,
            "permissive raw-memory is recorded once per thread by design"
        );
    }

    // -----------------------------------------------------------------------
    // Enforce — denies, and a denial has no observable effect
    // -----------------------------------------------------------------------

    #[test]
    fn enforce_denies_an_ungranted_read() {
        let (ctx, vm) = ctx_with_private_vm();
        let path = temp_path("denyread");
        write_file(&path, b"secret");

        let _guard =
            PolicyGuard::install(policy(vm, CapabilityMode::Enforce, "file-read:/nowhere"));
        let err = open_read_gated(&ctx, &path).expect_err("enforce must refuse");
        assert!(
            matches!(err, FdCapabilityError::Denied(_)),
            "refusal must be a capability denial, not an I/O error: {err}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_denied_open_is_refused_before_the_syscall() {
        let (ctx, vm) = ctx_with_private_vm();
        // A path that does not exist. The *raw* opener answers `NotFound` (it
        // reached `fs::File::open`); the gated opener must answer `Denied`
        // instead, which is only possible if the check ran first — so no fd
        // was reserved and no syscall was issued.
        let path = temp_path("missing");
        assert!(matches!(
            ctx.fd_table().open_read(&path).map_err(|e| e.kind()),
            Err(std::io::ErrorKind::NotFound)
        ));

        let _guard = PolicyGuard::install(policy(vm, CapabilityMode::Enforce, ""));
        assert!(
            matches!(
                open_read_gated(&ctx, &path),
                Err(FdCapabilityError::Denied(_))
            ),
            "the refusal must precede the open, so it cannot surface as NotFound"
        );
    }

    #[test]
    fn a_denied_write_creates_no_file() {
        let (ctx, vm) = ctx_with_private_vm();
        let path = temp_path("nocreate");
        assert!(!std::path::Path::new(&path).exists());

        let _guard =
            PolicyGuard::install(policy(vm, CapabilityMode::Enforce, "file-write:/nowhere"));
        let denied = open_write_gated(&ctx, &path, false);
        assert!(matches!(denied, Err(FdCapabilityError::Denied(_))));
        assert!(
            !std::path::Path::new(&path).exists(),
            "a refused write must not have created the file"
        );
    }

    #[test]
    fn enforce_admits_a_granted_path_and_refuses_a_traversal_out_of_it() {
        let (ctx, vm) = ctx_with_private_vm();
        let root = std::env::temp_dir().to_string_lossy().into_owned();
        let path = temp_path("granted");
        write_file(&path, b"ok");

        let grants = format!("file-read:{root}");
        let _guard = PolicyGuard::install(policy(vm, CapabilityMode::Enforce, &grants));

        let fd = open_read_gated(&ctx, &path).expect("a file under the granted root is admitted");
        let _ = ctx.fd_table().close(fd);

        // `..` is resolved before the prefix test, so this is not under the root.
        let escape = format!("{root}/../etc/passwd");
        assert!(
            matches!(
                open_read_gated(&ctx, &escape),
                Err(FdCapabilityError::Denied(_))
            ),
            "a traversal out of the granted root must be refused, not merely fail to open"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_write_open_needs_both_capabilities() {
        let (ctx, vm) = ctx_with_private_vm();
        let root = std::env::temp_dir().to_string_lossy().into_owned();
        let path = temp_path("rwboth");
        write_file(&path, b"rw");

        // Read granted, write not: the fd could still write, so this is refused.
        let read_only = format!("file-read:{root}");
        let guard = PolicyGuard::install(policy(vm, CapabilityMode::Enforce, &read_only));
        assert!(
            matches!(
                open_read_write_gated(&ctx, &path, false),
                Err(FdCapabilityError::Denied(_))
            ),
            "an fd that can write needs file-write too"
        );
        drop(guard);

        let both = format!("file-read:{root};file-write:{root}");
        let _guard = PolicyGuard::install(policy(vm, CapabilityMode::Enforce, &both));
        let fd = open_read_write_gated(&ctx, &path, false).expect("both granted");
        let _ = ctx.fd_table().close(fd);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn enforce_denies_an_ungranted_bind_and_binds_nothing() {
        let (ctx, vm) = ctx_with_private_vm();
        let _guard = PolicyGuard::install(policy(
            vm,
            CapabilityMode::Enforce,
            "network:127.0.0.1:9000-9100",
        ));
        // Port 0 is outside the granted range, so the bind never happens.
        assert!(
            matches!(
                open_tcp_listener_gated(&ctx, "127.0.0.1:0"),
                Err(FdCapabilityError::Denied(_))
            ),
            "a bind outside the granted port range must be refused"
        );
        assert!(
            recorded(vm, CapabilityKind::Network) >= 1,
            "the refused bind must still be recorded"
        );
    }

    #[test]
    fn enforce_denies_raw_memory_and_keeps_denying_it() {
        let (ctx, vm) = ctx_with_private_vm();
        let _guard = PolicyGuard::install(policy(vm, CapabilityMode::Enforce, ""));
        for _ in 0..3 {
            let err = gate_raw_memory(&ctx, RAW_MEMORY_UNSAFE_ADDRESS)
                .expect_err("enforce with no raw-memory grant must refuse");
            assert!(
                format!("{err:?}").contains("SecurityException"),
                "a denial must map to SecurityException, got {err:?}"
            );
        }
        assert_eq!(
            recorded(vm, CapabilityKind::RawMemory),
            3,
            "Enforce must not memoize the verdict away"
        );
    }

    #[test]
    fn a_raw_memory_grant_admits_the_unsafe_address_scope() {
        let (ctx, vm) = ctx_with_private_vm();
        let grants = format!("raw-memory:{RAW_MEMORY_UNSAFE_ADDRESS}");
        let _guard = PolicyGuard::install(policy(vm, CapabilityMode::Enforce, &grants));
        gate_raw_memory(&ctx, RAW_MEMORY_UNSAFE_ADDRESS).expect("the exact scope is granted");
    }

    #[test]
    fn spawn_and_foreign_gates_record_a_scope_that_round_trips_as_a_grant() {
        let (ctx, vm) = ctx_with_private_vm();
        let _guard = PolicyGuard::install(policy(vm, CapabilityMode::Audit, ""));

        gate_process_spawn(&ctx, "/bin/sh").expect("audit allows");
        gate_foreign_upcall(&ctx, "com.example.Callback::onEvent").expect("audit allows");
        gate_foreign_downcall(&ctx, "snprintf").expect("audit allows");
        gate_library_load(&ctx, "ssl").expect("audit allows");
        gate_network(&ctx, "example.invalid:443").expect("audit allows");

        let report = capability_audit(vm).unwrap();
        let grants = report.suggested_grants();
        assert!(grants.contains("process-spawn:"), "{grants}");
        assert!(
            grants.contains("foreign-upcall:com.example.Callback::onEvent"),
            "{grants}"
        );
        assert!(grants.contains("foreign-downcall:snprintf"), "{grants}");
        assert!(grants.contains("library-load:ssl"), "{grants}");
        assert!(grants.contains("network:example.invalid:443"), "{grants}");

        // The derived grant list, loaded into a fresh `Enforce` set, admits
        // exactly what the run did — the gates and the report agree.
        let (ctx2, vm2) = ctx_with_private_vm();
        let _guard2 = PolicyGuard::install(policy(vm2, CapabilityMode::Enforce, &grants));
        gate_process_spawn(&ctx2, "/bin/sh").expect("derived grant must admit the spawn");
        gate_foreign_downcall(&ctx2, "snprintf").expect("derived grant must admit the downcall");
        assert!(
            gate_foreign_downcall(&ctx2, "system").is_err(),
            "the derived grant must not admit a symbol the run never used"
        );
    }
}
