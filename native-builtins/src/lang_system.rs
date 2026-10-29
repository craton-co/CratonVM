// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! System, Runtime, ProcessBuilder, and Thread native method implementations.

use cratonvm_native_api::{NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::{LinkageError, MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

use crate::{obj_arg, platform_lib_name, try_alloc_concurrent_synthetic};

// ---------------------------------------------------------------------------
// System.exit / Runtime.exit pre-termination hook.
//
// `native_system_exit` and `native_runtime_exit` both end the process
// (`cratonvm_native_api::process_exit::exit_process`, gc-common w11-f)
// after printing the `[cratonvm] System.exit(N) called` line. Once the
// process exits, downstream observers (dispatch_trace ring, watchdog
// printers) lose their chance to dump state. Crates higher up the
// dependency stack (e.g. `cratonvm-vm` / `vm-cli`) can register a pre-exit
// hook here so they get one last shot at printing diagnostics before
// we terminate.
//
// Wired by `vm-cli::main::run()` so a silent `System.exit(0)` during real
// app boot (e.g. Cassandra NodeTool's airline NPE catch path) at least
// dumps the dispatch_trace ring when `CRATONVM_DBG_EXIT=1` is set.
// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Runtime.addShutdownHook / removeShutdownHook registry.
//
// The hook threads are held as JNI-global-ref handles (see
// `NativeContext::add_global_root`), a persistent, GC-remapped root.
//
// Registration MUST root the hook. A shutdown hook is normally never started,
// so on HotSpot the only thing keeping it — and everything it references —
// alive is the `ApplicationShutdownHooks.hooks` static map, and real
// applications rely on that. WildFly's `BootstrapImpl$ShutdownHook.register()`
// calls `addShutdownHook(this)` and only then stores the MSC `ServiceContainer`
// in its own field, while MSC 1.5's `ServiceContainer$Factory.create` registers
// a `Cleaner` that calls `container.shutdown()` once the container object
// becomes unreachable (its leak detector). With the hook dropped on the floor
// that whole chain became garbage the moment `Main.main` returned, the leak
// detector fired mid-boot, `AbstractControllerService.stop` reset `controller`
// to null, and the boot thread died on `WFLYCTL0085` / `WFLYSRV0056`.
//
// Handles rather than raw addresses: the global-ref table is remapped by the
// moving collector, so a stored `ObjectRef` would go stale while a handle stays
// valid. `removeShutdownHook` resolves each handle and compares it against the
// argument, so identity matching survives object motion.
//
// W7-92 (2026-08-12): this list HAD no reader. `shutdown_hook_add` pushed,
// `shutdown_hook_remove` popped, and nothing ever ran a hook on any of the five
// exit paths — the "write-only counter" shape, with the gap named in this very
// comment for months. The cost was not local: a corpus harness keyed on a
// `completed=` marker printed from a shutdown hook, the marker never appeared,
// and three separate lanes each read that as a sweeping cross-VM DIVERGE
// verdict (12, 9 and 36 findings). See W7-100.
//
// `run_shutdown_hooks` below is that reader. It is reached from `System.exit`,
// from `Runtime.exit`, from the intercepted `java/lang/Shutdown.runHooks()V`,
// and from the launcher's post-`main` path (`vm-cli/src/main.rs`, after the
// non-daemon join). It is deliberately NOT reached from `Runtime.halt`, which
// is specified as forcible termination — measured on HotSpot 25.0.3+9: `halt`
// skips hooks and `halt` called from INSIDE a hook terminates immediately.
//
// gc-common w16-e: the registry is PER VM (it was three process globals, the
// W7-92 §9.5 nomination). The handles are slots in ONE VM's global-root table,
// so a process-wide list let VM B resolve, compare against and START whatever
// object sat at VM A's slot numbers in B's own table, and VM A's shutdown set
// the in-progress flag for every VM in the process: after one embedded VM had
// shut down, every later `addShutdownHook` in the process threw
// `IllegalStateException("Shutdown in progress")`. The row is dropped at VM
// teardown (`forget_vm_system_singletons`).
// ---------------------------------------------------------------------------
#[derive(Default)]
struct ShutdownState {
    /// Global-root handles of the registered hook `Thread`s, in THIS VM's
    /// global-root table.
    hooks: Vec<usize>,
    /// Set the moment `run_shutdown_hooks` drains the list, i.e. once shutdown
    /// has begun. From that point HotSpot's `ApplicationShutdownHooks.add` /
    /// `remove` throw `IllegalStateException("Shutdown in progress")`.
    ///
    /// MEASURED on 25.0.3+9 (lane C11, `HookContract addduring` /
    /// `removeduring`): both calls, made from inside a running hook, threw
    /// `java.lang.IllegalStateException: Shutdown in progress`, and in the
    /// `remove` case the hook it tried to cancel ran anyway.
    ///
    /// Without this flag a late registration would be accepted and then
    /// silently dropped — the same "a marker that never appears" shape this
    /// whole record is about, reintroduced by its own repair.
    in_progress: bool,
    /// Set the first time [`run_jdk_shutdown_slots`] walks
    /// `java.lang.Shutdown`'s slot array, so a second entry into shutdown (a
    /// `System.exit` from inside a hook, or `Shutdown.runHooks` after the
    /// launcher's own drain) cannot run the JDK's hooks twice. The hook list
    /// needs no such flag: it is drained with `mem::take`, and an empty list is
    /// its own guard.
    jdk_slots_ran: bool,
    /// The thread that began shutdown (`Shutdown.exit` holds the monitor of
    /// `java.lang.Shutdown` for the whole of it). A DIFFERENT thread calling
    /// `System.exit` / `Runtime.exit` meanwhile blocks forever on HotSpot (SBR5
    /// sysprops, `Sbr5SyspropsExit nested`: a hook that starts a thread calling
    /// `System.exit(2)` printed `hook-done` and the process left with rc 0; this VM's
    /// native exit left at once with rc 2). The same thread re-entering is a
    /// recursive monitor enter and proceeds.
    exiting_thread: Option<u64>,
}

/// Claim the right to run shutdown for the calling thread, or block forever.
///
/// The `--compatible` twin of `Shutdown.exit`'s `synchronized (Shutdown.class)`: under
/// `--jdk-only` `System.exit` runs the JDK's own bytecode and this is never reached.
fn claim_exit_or_block_forever(ctx: &mut dyn NativeContext) {
    // Interpreter round i3 wave 79, lane L6
    // (`CRATONVM_COMPAT_EXIT_BLOCKS_ON_SHUTDOWN`, default off; page
    // `i66-L1-second-exit-caller-reads-waiting-not-blocked`): take
    // `Shutdown.class`'s monitor FIRST, as JDK 25's `Shutdown.exit` does
    // (`synchronized (Shutdown.class)`), and keep it until the process ends
    // (every caller of this function ends in `exit_process`). A second
    // exiting thread then blocks entering that monitor through the VM's
    // contended enter: BLOCKED, JMX lock `java.lang.Class@..` owned by the
    // exiting thread, JVMTI `BLOCKED_ON_MONITOR_ENTER` -- HotSpot's answers,
    // where the sleep loop below reads WAITING with no lock. The same thread
    // re-entering (an exit from its own hook) is a recursive enter. The claim
    // below still decides, so a VM without the class keeps the old loop.
    if cratonvm_types::flags::runtime_flag_default_on("CRATONVM_COMPAT_EXIT_BLOCKS_ON_SHUTDOWN") {
        if let Some(shutdown) = ctx.class_id_by_name("java/lang/Shutdown") {
            let mirror = ctx.get_class_mirror(shutdown);
            let _ = ctx.monitor_enter_gc_safe(mirror);
        }
    }
    let vm = ctx.vm_identity();
    let me = ctx.thread_id();
    let mine = SHUTDOWN_STATE.with(vm, |s| match s.exiting_thread {
        None => {
            s.exiting_thread = Some(me);
            true
        }
        Some(owner) => owner == me,
    });
    if mine {
        return;
    }
    loop {
        // A blocked region, as every long wait in this file: the owner's hooks allocate,
        // so a stop-the-world collection can be requested while this thread sleeps.
        ctx.begin_blocking_region();
        std::thread::sleep(std::time::Duration::from_millis(500));
        ctx.end_blocking_region();
    }
}

/// The shutdown-hook registry, one row per VM. Never held across a
/// `NativeContext` call (the `VmScoped` lock discipline).
static SHUTDOWN_STATE: VmScoped<ShutdownState> = VmScoped::new();

/// Has `vm`'s shutdown begun?
fn shutdown_in_progress(vm: usize) -> bool {
    SHUTDOWN_STATE.peek(vm, |s| s.in_progress).unwrap_or(false)
}

/// A copy of `vm`'s registered hook handles, taken so the caller can resolve
/// them with the table lock released.
fn registered_shutdown_hooks(vm: usize) -> Vec<usize> {
    SHUTDOWN_STATE
        .peek(vm, |s| s.hooks.clone())
        .unwrap_or_default()
}

// The four refusal messages of the shutdown-hook registry, as CONSTANTS.
//
// Named rather than inlined for the reason the handoff's §5 gives: "messages
// often cannot be derived, only transcribed". Every one of these was read off
// Temurin 25.0.3+9 — three from a run (`HookProbe dup`, `HookProbe addduring`)
// and all four confirmed against `javap -p -c java.lang.ApplicationShutdownHooks`
// — and none of them is guessable from the method name. Constants give the
// unit tests below something to pin, so a later "tidy" of the wording fails a
// test instead of quietly failing a differential.
//
// All four are pure ASCII, and that is checked, not assumed: `HANDOFF-20260814`
// §7 records a differential that failed with every assertion passing because
// one em-dash crossed HotSpot's Windows console code page differently.

/// `ApplicationShutdownHooks.add` pc 10 and `remove` pc 10, and
/// `Shutdown.add` pc 97/120. MEASURED (`HookProbe addduring`) on both
/// `addShutdownHook` and `removeShutdownHook` called from inside a hook.
const HOOK_MSG_SHUTDOWN_IN_PROGRESS: &str = "Shutdown in progress";

/// NOT an explicit check — this is HotSpot's helpful-NPE rendering of the
/// `invokevirtual java/lang/Thread.isAlive` at `ApplicationShutdownHooks.add`
/// pc 17, which is the first thing that touches the argument. MEASURED
/// (`HookProbe dup`, `NULL-ADD`). The quoted `hook` is `add`'s parameter name.
const HOOK_MSG_NULL_ADD: &str =
    "Cannot invoke \"java.lang.Thread.isAlive()\" because \"hook\" is null";

/// `ApplicationShutdownHooks.add` pc 27. MEASURED (`HookProbe dup`,
/// `RUNNING-ADD`) by registering a `Thread` that had been `start()`ed and was
/// parked on a latch.
const HOOK_MSG_ALREADY_RUNNING: &str = "Hook already running";

/// `ApplicationShutdownHooks.add` pc 47. MEASURED (`HookProbe dup`,
/// `DUP-ADD`).
const HOOK_MSG_PREVIOUSLY_REGISTERED: &str = "Hook previously registered";

/// Register `hook` as a shutdown hook, rooting it for the life of the VM.
///
/// FOUR refusals, in this exact order, and the order is the contract rather
/// than a preference. SOURCE-VERIFIED against
/// `javap -p -c java.lang.ApplicationShutdownHooks` on Temurin 25.0.3+9 —
/// `add(Thread)` is `static synchronized` and its bytecode reads:
///
/// ```text
///   0: getstatic hooks; ifnonnull 16  -> IllegalStateException "Shutdown in progress"
///  16: aload_0; invokevirtual Thread.isAlive  -> NPE here when hook == null
///  20: ifeq 33                        -> IllegalArgumentException "Hook already running"
///  33: hooks.containsKey(hook)        -> IllegalArgumentException "Hook previously registered"
///  53: hooks.put(hook, hook)
/// ```
///
/// so:
///
/// * shutdown already begun → `IllegalStateException("Shutdown in progress")`,
///   and it wins over EVERY other refusal including the null check;
/// * `null` → `NullPointerException`. There is no explicit null check: the NPE
///   falls out of `invokevirtual Thread.isAlive` at pc 17, which is why
///   HotSpot's helpful-NPE text names `isAlive()` and the parameter `hook`.
///   MEASURED (`HookProbe dup`, 25.0.3+9): message is exactly
///   `Cannot invoke "java.lang.Thread.isAlive()" because "hook" is null`.
///   This used to be `if let Some(Value::Object(Some(hook)))` at the
///   registration site — a null was ACCEPTED and silently dropped, which is
///   the fabricated-success shape this whole record is about;
/// * an ALREADY RUNNING hook `Thread` →
///   `IllegalArgumentException("Hook already running")`. MEASURED
///   (`HookProbe dup`, `RUNNING-ADD`). This VM accepted it, and then
///   `run_shutdown_hooks` counted it `skipped` — a registration that reports
///   success and can never run;
/// * re-registering a hook that is already registered throws
///   `IllegalArgumentException("Hook previously registered")` (W7-92 §1.3).
///
/// NOT refused: a hook `Thread` that has already run to completion. It is not
/// alive, `ApplicationShutdownHooks.add` accepts it, and HotSpot printed
/// `TERMINATED-ADD accepted` for exactly that case. `run_shutdown_hooks` will
/// not re-run it (HotSpot does not either — measured, `RunTerm`), it counts as
/// `skipped`.
fn shutdown_hook_add(
    ctx: &mut dyn NativeContext,
    hook: Option<ObjectRef>,
) -> Result<(), MethodCallFailed> {
    let vm = ctx.vm_identity();
    if shutdown_in_progress(vm) {
        return Err(RuntimeError::IllegalStateException {
            message: HOOK_MSG_SHUTDOWN_IN_PROGRESS.to_string(),
        }
        .into());
    }
    // Transcribed, not derived — the handoff's §5 rule. HotSpot builds this
    // string from the bytecode at the faulting site, so it names `isAlive()`
    // and the `ApplicationShutdownHooks.add` parameter `hook`, not
    // `Runtime.addShutdownHook`'s.
    let Some(hook) = hook else {
        return Err(RuntimeError::NullPointerException {
            message: Some(HOOK_MSG_NULL_ADD.to_string()),
        }
        .into());
    };
    if ctx.thread_is_alive(hook) {
        return Err(RuntimeError::IllegalArgumentException {
            message: HOOK_MSG_ALREADY_RUNNING.to_string(),
        }
        .into());
    }
    if registered_shutdown_hooks(vm)
        .iter()
        .any(|h| ctx.resolve_global_root(*h) == Some(hook))
    {
        // HotSpot: `ApplicationShutdownHooks.add` throws
        // IllegalArgumentException("Hook previously registered").
        return Err(RuntimeError::IllegalArgumentException {
            message: HOOK_MSG_PREVIOUSLY_REGISTERED.to_string(),
        }
        .into());
    }
    let handle = ctx.add_global_root(hook);
    SHUTDOWN_STATE.with(vm, |s| s.hooks.push(handle));
    Ok(())
}

/// Drop a previously registered hook. Returns true iff it was registered —
/// `Runtime.removeShutdownHook`'s documented contract — and throws
/// `IllegalStateException` once shutdown has begun, as HotSpot does.
///
/// `null` throws `NullPointerException` **with no message**, and the two
/// details are both contract. SOURCE-VERIFIED
/// (`javap -p -c java.lang.ApplicationShutdownHooks`, `remove(Thread)`): the
/// `hooks == null` test is at pc 0 and the null test is an EXPLICIT
/// `new NullPointerException()` at pc 20 — no-arg, so `getMessage()` is
/// `null`, unlike `add`'s helpful NPE. MEASURED (`HookProbe dup`): the two
/// lines are `NULL-ADD threw=java.lang.NullPointerException msg=Cannot invoke
/// "java.lang.Thread.isAlive()" because "hook" is null` and `NULL-REMOVE
/// threw=java.lang.NullPointerException msg=null`. That asymmetry is why the
/// message here is `None` rather than `Some("")` — `msg=null` and `msg=` are
/// different cells and the differential prints them differently.
///
/// This body used to answer `false` for a null hook: a caller asking "was it
/// registered?" got a plausible, wrong "no" instead of the throw.
fn shutdown_hook_remove(
    ctx: &mut dyn NativeContext,
    hook: Option<ObjectRef>,
) -> Result<bool, MethodCallFailed> {
    let vm = ctx.vm_identity();
    if shutdown_in_progress(vm) {
        return Err(RuntimeError::IllegalStateException {
            message: HOOK_MSG_SHUTDOWN_IN_PROGRESS.to_string(),
        }
        .into());
    }
    let Some(hook) = hook else {
        return Err(RuntimeError::NullPointerException { message: None }.into());
    };
    let Some(handle) = registered_shutdown_hooks(vm)
        .into_iter()
        .find(|h| ctx.resolve_global_root(*h) == Some(hook))
    else {
        return Ok(false);
    };
    // The handles were resolved with the table lock released; a concurrent
    // `remove` of the same hook may have taken it meanwhile, and only the one
    // that still finds it releases the root.
    let removed = SHUTDOWN_STATE.with(vm, |s| match s.hooks.iter().position(|h| *h == handle) {
        Some(pos) => {
            s.hooks.remove(pos);
            true
        }
        None => false,
    });
    if !removed {
        return Ok(false);
    }
    ctx.remove_global_root(handle);
    Ok(true)
}

/// Decode the `Thread` argument of `Runtime.addShutdownHook` /
/// `removeShutdownHook` out of the native argument vector.
///
/// Returns `Some(None)` for a **Java null** — which the two callers must turn
/// into a `NullPointerException`, because HotSpot does — and `None` only when
/// the vector is not the shape the descriptor guarantees (`[receiver, hook]`
/// for a one-argument instance method).
///
/// Those two cases are separated on purpose. Folding them together is what the
/// previous `if let Some(Value::Object(Some(hook)))` did, and it made a Java
/// null indistinguishable from a VM-side decoding failure — so both were
/// answered with silence. An NPE is the right answer to the first and a
/// **wrong** answer to the second, since it would report a caller error for a
/// fault inside this VM. The short-vector arm therefore says so on stderr,
/// unconditionally, and declines to decide; it is not expected to fire, and if
/// it ever does, the line is the finding.
fn shutdown_hook_argument(args: &[Value], which: &str) -> Option<Option<ObjectRef>> {
    match args.get(1) {
        Some(Value::Object(hook)) => Some(*hook),
        other => {
            eprintln!(
                "[cratonvm] Runtime.{which}: argument vector is not [receiver, Thread] \
                 (len={len}, slot1={other:?}); the hook was neither registered nor refused. \
                 This is a VM-side decoding fault, not an application error.",
                len = args.len()
            );
            None
        }
    }
}

/// Push whatever the VM has buffered for fd 1 and fd 2 out to the OS.
///
/// Called on every path that is about to end the process, and it is the half
/// of W7-92 that a working runner still would not have delivered.
/// `FileDescriptorTable`'s stdout/stderr entries are buffered writers shared by
/// every Java writer in this VM; `std::process::exit` runs no destructors, so
/// anything a shutdown hook printed and did not force out is dropped on the
/// floor by the very call that ends the run.
///
/// MEASURED on Temurin 25.0.3+9 that HotSpot does NOT lose those bytes, on
/// both of the paths that could:
///
/// * `HookProbe noflush` — a hook does `System.out.print` with no newline and
///   no `flush()`, main then calls `System.exit(0)`: `HOOK-PARTIAL-NO-NEWLINE`
///   is delivered.
/// * `HookProbe haltnoflush` — `System.out.print` with no newline followed by
///   `Runtime.halt(6)`, which runs no hooks at all:
///   `PARTIAL-NO-NEWLINE-BEFORE-HALT` is still delivered, rc 6.
///
/// So the flush belongs on the halt path too, and its placement there is not a
/// "hooks also run on halt" mistake — halt still runs no hooks.
///
/// Failures are swallowed deliberately: this runs when the process is already
/// committed to exiting with a code chosen elsewhere, and a broken pipe on
/// stdout must not change that code or emit anything new. The one thing it
/// must not do is *skip* the second stream because the first failed, which is
/// why the two calls are independent statements rather than a `?` chain.
pub(crate) fn flush_console_streams(ctx: &dyn NativeContext) {
    let _ = ctx.fd_table().flush(1);
    let _ = ctx.fd_table().flush(2);
}

/// How long `run_shutdown_hooks` waits for a started hook thread to finish.
///
/// HotSpot waits FOREVER (`ApplicationShutdownHooks.runHooks` loops on
/// `hook.join()`), and a hung hook there hangs the JVM with no diagnostic at
/// all. This VM is run overwhelmingly by harnesses that read a wedged process
/// as "the VM hung" and produce a false finding, so the default here was a
/// bounded wait plus a loud line naming what was still running — a stated,
/// visible divergence rather than a silent hang. It is now opt-in.
///
/// `CRATONVM_SHUTDOWN_HOOK_TIMEOUT_MS=0` (the default) is HotSpot's unbounded
/// wait; any other value sets the bound in milliseconds.
///
/// Read through `flags::runtime_var`. It used to use `std::env::var`, with a
/// note saying `nbflags()` lives in `lib.rs` which that lane did not own — but
/// the boundary this needs is `cratonvm_types::flags`, not `nbflags`, and this
/// crate already depends on it. Raw was wrong twice over: the name was declared
/// nowhere (so `CRATONVM_THREADS=shutdown-hook-timeout-ms=…` could not reach
/// it), and a raw read of a declared name is served by a live `getenv` instead
/// of the latched snapshot, which check 4 of `tools/flag-census/check-surface.sh`
/// flags for every core crate.
///
/// Interpreter round i5 wave 102, lane L6: read per VM
/// (`NativeContext::shutdown_hook_timeout_ms`, `VmConfig::shutdown_hook_timeout_ms`,
/// latched from the same variable when the VM's config is built). It was a
/// process-wide `OnceLock`, so the first VM to exit fixed the bound for every
/// later VM in the process (AGENTS.md: no process globals for compatibility
/// state).
///
/// Default 0 (HotSpot's unbounded wait) since the interpreter round i5
/// user-decision flip (WARNING interp-i5-hook-timeout-flip-20261015); the
/// bounded wait described above is now the opt-in for harnesses.
fn shutdown_hook_join_bound(ctx: &dyn NativeContext) -> Option<std::time::Duration> {
    match ctx.shutdown_hook_timeout_ms() {
        0 => None,
        ms => Some(std::time::Duration::from_millis(ms)),
    }
}

/// Has `thread` already been started (and possibly since finished)?
///
/// Extracted from `native_thread_start0`, which is the only other place that
/// asks. Two readers because only one lookup route is aliasing-proof: a
/// real-JDK mirror resolves through the process-unique `Thread.tid` index,
/// while a fabricated mirror has no `tid` field and would fall back to an
/// unguarded pointer walk, so it reads the on-mirror marker `vm_exec::
/// thread_start` writes instead. Keeping this in ONE function is deliberate:
/// the shutdown runner and `Thread.start()` must not drift apart about what
/// "already started" means, and this tree has a written record of exactly that
/// species of twin drifting.
pub(crate) fn thread_already_started(ctx: &mut dyn NativeContext, thread: ObjectRef) -> bool {
    if crate::has_real_jdk_thread_layout(ctx, thread) {
        ctx.thread_run_state(thread) != 0
    } else {
        ctx.object_num_fields(thread) > 2 && matches!(ctx.get_field(thread, 2), Value::Long(_))
    }
}

/// Pins `child` across [`capture_inheritable_tl_at_construction_body`] and hands the refreshed reference back.
///
/// The receiver is `&mut` on purpose. The body ALLOCATES and returns no
/// reference, so a moving collector could relocate `child` inside the call and
/// every caller was left holding a pre-move address -- the shape
/// `WORKER-5-NOTE-10` traced `TreeMap.size()` returning 0 to. `&mut` makes
/// forgetting the refresh a COMPILE ERROR instead of an audit finding.
pub(crate) fn capture_inheritable_tl_at_construction(
    ctx: &mut dyn NativeContext,
    child: &mut ObjectRef,
) {
    let w5_pin = ctx.pin_native_root(*child);
    let w5_out = capture_inheritable_tl_at_construction_body(ctx, *child);
    *child = ctx.read_native_pin(w5_pin, *child);
    ctx.unpin_native_roots(w5_pin);
    w5_out
}

/// Capture the constructing thread's `InheritableThreadLocal` values against
/// `child`, at CONSTRUCTION time — the moment HotSpot captures them.
///
/// MEASURED on Temurin 25.0.3+9 (`ItlProbe`, 2026-08-16): with
/// `ITL.set("a"); Thread t = new Thread(r); ITL.set("b"); t.start();` the
/// child sees `"a"` on HotSpot for BOTH the plain `Thread(Runnable)` shape and
/// the `Thread(ThreadGroup, Runnable, String)` shape that
/// `Executors.defaultThreadFactory()` uses; CratonVM sees `"b"` because
/// `native_thread_start0` was the only capture point. SOURCE-VERIFIED against
/// `javap -p -c java.lang.Thread`: the copy lives at pc 175..201 of the single
/// package-private master constructor that all eight public constructors
/// forward to, so "captured when the Thread object was built" is the whole
/// contract.
///
/// Three properties this function must have, each of them measured:
///
/// * It queues even an EMPTY snapshot. `ItlProbe` case 9 — parent `set`s,
///   `remove`s, constructs, then `set`s again — has the child see `null`. An
///   absent queue entry would let `native_thread_start0` fall back to the
///   parent's *current* map and hand the child the later value.
/// * It must be called with the child's identity already stable, because the
///   queue is keyed by `identity_hash_code` and drained by the child under its
///   own `current_thread_object()` identity.
/// * It is the ONLY thing `native_thread_start0` consults to decide whether to
///   capture. There is no second flag to drift out of step with the queue.
///
/// Callers today: `jdk25_concurrency::…fork`, which builds a subtask worker
/// Thread and hands it straight to `ctx.thread_start` — bypassing
/// `native_thread_start0`, so before this its subtasks inherited NOTHING.
/// HotSpot inherits there (MEASURED, `StsItl`: a subtask forked after
/// `ITL.set("scope-parent")` reads `scope-parent`). The ordinary
/// `new Thread(...)` paths cannot call this yet — see the nominations in
/// `docs/feature-designs/class-resolution-and-caching.md
/// construction-20260816.md` §6, and read that record BEFORE assuming the
/// `new Thread` timing divergence is closed. It is not.
pub(crate) fn capture_inheritable_tl_at_construction_body(
    ctx: &mut dyn NativeContext,
    child: ObjectRef,
) {
    // The queue is keyed by the child's ADDRESS (gc-common w5-b), and the
    // `childValue` upcalls in the snapshot can move it: queue under the address
    // read back after them.
    let child_pin = ctx.pin_native_root(child);
    let snapshot = crate::phases_early::snapshot_inheritable_tl_entries(ctx).unwrap_or_default();
    let child = ctx.read_native_pin(child_pin, child);
    ctx.unpin_native_roots(child_pin);
    crate::phases_early::queue_inherited_tl_for_child(ctx, child, snapshot);
}

/// Did some construction-time site already answer the inheritance question for
/// this child Thread?
///
/// Asked of the pending-inheritance queue itself rather than a side flag: an
/// entry is present if and only if a capture happened, empty entries included,
/// so the predicate and the data it guards cannot disagree. This tree has a
/// written record of exactly that species of twin drifting
/// (`thread_already_started`, just above, exists for the same reason).
///
/// `child` is the `Thread` at its current address: the queue is keyed by
/// `(vm_identity, child address)` since gc-common w5-b.
pub(crate) fn inheritable_tl_captured_at_construction(
    ctx: &dyn NativeContext,
    child: ObjectRef,
) -> bool {
    crate::phases_early::inherited_tl_queued_for_child(ctx, child)
}

/// Run every registered `Runtime.addShutdownHook` hook, once, before the
/// process is torn down. THE READER W7-92 is about.
///
/// **Each hook is genuinely `start()`ed as its own thread and then joined**,
/// not `run()` inline on the exiting thread. That is not gold-plating:
///
/// * `Thread.currentThread().getName()` inside the hook is observable, and the
///   shipped vector (`regression-suite/src/RShutdownHooks.java`) asserts on it
///   (`ownThread=true`);
/// * hooks run CONCURRENTLY on HotSpot — measured (`HookContract crosswait`):
///   a hook that blocks until a second hook signals it is released, which an
///   inline runner would deadlock on;
/// * a hook needs a real Java frame on a real VM thread the moment it touches
///   anything that walks the stack, and Tomcat/log4j teardown does.
///
/// Ordering is NOT part of the contract: HotSpot starts all hooks and then
/// joins them all, so their relative order is unspecified (measured: three
/// hooks came back 3, boom, 1). A fix must not be judged on hook order.
///
/// Idempotent: the list is drained under the lock and the VM's in-progress flag
/// is set, so a hook that itself calls `System.exit` re-enters this function
/// and finds nothing to run.
///
/// Four counters on ONE unconditional stderr line, because "ran three hooks"
/// and "the list was empty" being indistinguishable in the output is the exact
/// defect this whole record exists to close, and a repair that prints only on
/// failure reproduces it:
///
/// * `ran`    — hooks whose thread was started and observed to finish.
/// * `threw`  — hooks whose `start()` failed at the VM boundary. An exception
///   thrown by a hook's BODY is not counted here and must not be: it lands on
///   the hook's own thread, HotSpot swallows it (measured: rc unchanged, the
///   other two hooks still ran) and so does this.
/// * `skipped` — hooks that were already started or already finished. HotSpot
///   does not re-run a terminated hook either (measured, `RunTerm`).
/// * `unjoined` — started but still running when the wait bound expired.
///
/// NOT called from `native_shutdown_halt0`: `Runtime.halt` is forcible
/// termination and the JDK runs hooks from `Shutdown.exit`, which halt
/// bypasses. Measured both ways on 25.0.3+9. Do not "fix" that asymmetry.
/// Run the JDK's own `java.lang.Shutdown.hooks` slots, the way
/// `java.lang.Shutdown.runHooks` does: slot order, each one's throw caught and
/// counted, none of them re-run.
///
/// # The gap this closes, and how it was found
///
/// `Runtime.addShutdownHook` is intercepted (the list above), so the JDK's slot
/// 1 -- `ApplicationShutdownHooks` -- is never installed and running it would
/// run nothing. But slot 1 is not the only slot. `java.lang.Shutdown` carries
/// three:
///
/// ```text
///   0  Console.restoreEcho          (installed by java.io.Console)
///   1  ApplicationShutdownHooks.runHooks
///   2  DeleteOnExitHook.runHooks    (installed by File.deleteOnExit)
/// ```
///
/// and nothing in this VM ever ran slots 0 and 2. MEASURED 2026-09-19 (lane 4
/// wave 17, `apps/probes/L4FileTempExit.java`): with `java.io.File.deleteOnExit`
/// retired to its JDK body, HotSpot left the probe's temp directory empty and
/// this VM left the file, the directory and its child on disk, with
/// `shutdown hooks: ran=0` in the exit summary. That is what blocked the
/// `deleteOnExit` row -- the retirement was correct and the VM had nowhere to
/// deliver the hook the real body registers.
///
/// # Why it is safe to run them AFTER the Rust-side list
///
/// It is not merely safe, it is HotSpot's own order. The JDK runs the slots in
/// index order and the application hooks ARE slot 1, so anything this VM keeps
/// in the Rust list belongs between slot 0 and slot 2. Slot 0 restores console
/// echo and slot 2 deletes files; both are specified to happen after the
/// application's hooks have finished, which is exactly where this call sits.
///
/// # What it does NOT do
///
/// It does not load `java.lang.Shutdown`. `class_id_by_name` answers `None` for
/// a class this run never touched, and a run that never touched it registered
/// no slot hook either -- so the whole pass is skipped rather than forcing a
/// `<clinit>` on the way out of the process. That is also what keeps
/// synthetic-JDK mode, which has no such class, on this code path instead of on
/// a bridge that silently does nothing.
///
/// Returns `(ran, threw)`.
fn run_jdk_shutdown_slots(ctx: &mut dyn NativeContext) -> (usize, usize) {
    let vm = ctx.vm_identity();
    if SHUTDOWN_STATE.with(vm, |s| std::mem::replace(&mut s.jdk_slots_ran, true)) {
        return (0, 0);
    }
    let Some(class_id) = ctx.class_id_by_name("java/lang/Shutdown") else {
        return (0, 0);
    };
    let Some(field) = ctx.static_field_index_by_name(class_id, "hooks") else {
        return (0, 0);
    };
    let Value::Object(Some(arr)) = ctx.get_static_field(class_id, field) else {
        return (0, 0);
    };
    let slots = ctx.array_length(arr);
    let (mut ran, mut threw) = (0usize, 0usize);
    for i in 0..slots {
        // Re-read the ARRAY on every iteration rather than caching the
        // `ObjectRef`: a hook body allocates, a moving collection during it
        // relocates the array, and the static slot is a root the collector
        // rewrites while a Rust local is not. Same rule as the
        // `resolve_global_root` loop in `run_shutdown_hooks`.
        let Value::Object(Some(arr)) = ctx.get_static_field(class_id, field) else {
            break;
        };
        let Value::Object(Some(hook)) = ctx.get_array_element(arr, i) else {
            continue;
        };
        match ctx.invoke_virtual(hook, "run", "()V", &[]) {
            Ok(_) => ran += 1,
            Err(e) => {
                threw += 1;
                // `Shutdown.runHooks` catches `Throwable` per slot and carries
                // on to the next one; a slot-2 failure must not skip slot 0's
                // console restore. nio21 wave 9 lane L4: HotSpot swallows the
                // slot's Throwable silently, so the line is exit-census only.
                if exit_census_on() {
                    eprintln!(
                        "[cratonvm] java.lang.Shutdown hook slot {i} threw; continuing with \
                         the remaining slots: {e:?}"
                    );
                }
            }
        }
    }
    (ran, threw)
}

/// Is `java.lang.Shutdown.runHooks` the JDK's own bytecode in this VM (SBR5 sysprops:
/// retired under `--jdk-only`)? Then the hooks, the console-restore slot and the
/// `deleteOnExit` slot all live in `java.lang.Shutdown`/`ApplicationShutdownHooks`, and the
/// launcher drives them the way HotSpot's `DestroyJavaVM` does: `Shutdown.shutdown()`.
fn jdk_runs_the_shutdown_hooks(ctx: &dyn NativeContext) -> bool {
    ctx.is_jdk_only()
        && cratonvm_native_api::retired_shadow::triple_is_retired_shadow(
            "java/lang/Shutdown",
            "runHooks",
            "()V",
        )
}

pub fn run_shutdown_hooks(ctx: &mut dyn NativeContext, trigger: &str) {
    if jdk_runs_the_shutdown_hooks(&*ctx) {
        // A run that never loaded `java.lang.Shutdown` registered no hook of any kind
        // (`ApplicationShutdownHooks`, `Console`, `DeleteOnExitHook` all reach it through
        // `Shutdown.add`), so there is nothing to run, and nothing is loaded just to find out.
        if ctx.class_id_by_name("java/lang/Shutdown").is_some() {
            if let Err(e) = ctx.invoke("java/lang/Shutdown", "shutdown", "()V", &[]) {
                // nio21 wave 9 lane L4: exit-census only (HotSpot prints nothing).
                if exit_census_on() {
                    eprintln!("[cratonvm] java.lang.Shutdown.shutdown() ({trigger}) threw: {e:?}");
                }
            }
        }
        // The hooks have finished; their bytes have NOT necessarily left this process.
        flush_console_streams(&*ctx);
        return;
    }
    let vm = ctx.vm_identity();
    let me = ctx.thread_id();
    let handles: Vec<usize> = SHUTDOWN_STATE.with(vm, |s| {
        s.in_progress = true;
        s.exiting_thread.get_or_insert(me);
        std::mem::take(&mut s.hooks)
    });

    let mut threw = 0usize;
    let mut skipped = 0usize;
    let mut started: Vec<usize> = Vec::with_capacity(handles.len());

    // Phase 1 — start them all, then phase 2 joins them all. Same shape as
    // `ApplicationShutdownHooks.runHooks`, and the reason it is two loops
    // rather than one start-join pair is the `crosswait` measurement above.
    for &handle in &handles {
        // Re-resolved from the global-root table on every use: the handles are
        // GC-remapped, a raw `ObjectRef` cached across an `invoke_virtual`
        // would go stale under a moving collector, and a hook body allocates.
        let Some(hook) = ctx.resolve_global_root(handle) else {
            skipped += 1;
            continue;
        };
        if thread_already_started(ctx, hook) {
            skipped += 1;
            continue;
        }
        match ctx.invoke_virtual(hook, "start", "()V", &[]) {
            Ok(_) => started.push(handle),
            Err(e) => {
                threw += 1;
                // Unconditional, not `tracing::warn!`: tracing is compiled out
                // of release builds of this VM, and a hook that could not even
                // be started is precisely the state that must not be silent.
                // nio21 wave 9 lane L4: HotSpot prints nothing here, so the
                // line is exit-census only.
                if exit_census_on() {
                    eprintln!(
                        "[cratonvm] shutdown hook could not be started; continuing with \
                         the remaining hooks: {e:?}"
                    );
                }
            }
        }
    }

    // Phase 2 — join. Poll `thread_is_alive` rather than `ctx.thread_join`
    // so the wait can be bounded; `thread_start` registers the thread as alive
    // BEFORE spawning it, so there is no "not yet visible" race here.
    let bound = shutdown_hook_join_bound(&*ctx);
    let deadline = bound.map(|d| std::time::Instant::now() + d);
    let mut unjoined = 0usize;
    for &handle in &started {
        loop {
            let Some(hook) = ctx.resolve_global_root(handle) else {
                break;
            };
            if !ctx.thread_is_alive(hook) {
                break;
            }
            if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
                unjoined += 1;
                break;
            }
            // The blocked-region protocol is mandatory, not hygiene: the hook
            // threads allocate, so a stop-the-world collection can be requested
            // while we sleep here, and a thread sleeping outside a blocked
            // region never reaches the safepoint. `native_thread_join_timed`
            // does the same dance for the same reason.
            //
            // Interpreter round i2 wave 66, lane L1: the JDK body this stands
            // for, `ApplicationShutdownHooks.runHooks`, is `hook.join()`, a
            // `wait(0)` loop: WAITING, as the plain region was, and with
            // `CRATONVM_PUBLISHED_ALL_WAIT_KINDS` JVMTI `IN_OBJECT_WAIT` (the
            // served `Thread.join` publishes the same; it read `PARKED`).
            // Wave 68 (lane L1, `CRATONVM_STAND_IN_WAIT_OBJECTS`): and the hook
            // it waits on, as that `join` would ([`begin_join_region`]).
            begin_join_region(ctx, hook, cratonvm_native_api::BlockKind::ObjectWait);
            std::thread::sleep(std::time::Duration::from_millis(2));
            ctx.end_blocking_region();
        }
    }
    let ran = started.len() - unjoined;

    for handle in handles {
        ctx.remove_global_root(handle);
    }

    // The JDK's own slots (`Console.restoreEcho`, `DeleteOnExitHook`), which
    // this VM ran nowhere until 2026-09-20. See `run_jdk_shutdown_slots`.
    let (jdk_ran, jdk_threw) = run_jdk_shutdown_slots(ctx);

    // The hooks have finished; their bytes have NOT necessarily left this
    // process. Flush here rather than only at the `std::process::exit` sites,
    // for two reasons: the launcher's post-`main` path does not go through
    // either `exit` native at all, and the summary line below is written with
    // `eprintln!` while a hook's `System.out.println` went through the fd
    // table, so without this the two streams can be delivered out of order in
    // a `2>&1` capture — which is how every vector in `regression-suite` is
    // read. See `flush_console_streams` for what HotSpot was measured to do.
    flush_console_streams(&*ctx);

    if unjoined > 0 {
        eprintln!(
            "[cratonvm] shutdown hooks: {unjoined} still running after {}ms; continuing exit. \
             Set CRATONVM_SHUTDOWN_HOOK_TIMEOUT_MS=0 to wait forever (HotSpot's behaviour) \
             or to a larger bound.",
            bound.map(|d| d.as_millis()).unwrap_or(0)
        );
    }
    // UNCONDITIONAL, on stderr, next to the `[cratonvm] System.exit(N) called`
    // line that is already unconditional on that path. `ran=0` for a program
    // with no hooks is the honest reading, and it is what makes `ran=0` on a
    // program WITH hooks a finding rather than a silence.
    //
    // nio21 wave 8 lane L4: no longer unconditional. HotSpot prints nothing
    // here, and every program printed this line, so a plain run's stderr never
    // matched; it is `CRATONVM_DBG=exit-census` now, like the `System.exit`
    // line it sits beside.
    if exit_census_on() {
        eprintln!(
            "[cratonvm] shutdown hooks: ran={ran} threw={threw} skipped={skipped} \
             unjoined={unjoined} jdk_slots={jdk_ran} jdk_slots_threw={jdk_threw} \
             trigger={trigger}"
        );
    }
    report_vector_intrinsics();
    // The invoke cache's hit rate, when asked for. It is the acceptance
    // criterion for pc-keying the cache key: a `put` and a `get` that
    // disagree about a site's pc store and seek under different keys, and
    // the ONLY symptom is a lower hit rate -- missing the cache is always
    // safe, so no correctness test can see it.
    cratonvm_classloading::resolution::invoke_cache_stats_summary();
    // The FFM element fast path's engagement, on the same exit path and
    // for the same reason as the censuses below: a JUnit runner or a
    // benchmark harness leaves through `System.exit`, so a line printed
    // anywhere else appears in zero logs of the runs that matter. Silent
    // unless the process touched a segment. See `ffm_fast::exit_summary`.
    // nio21 wave 8 lane L4: under `CRATONVM_DBG=exit-census` (any FFM program
    // touches a segment; HotSpot prints no such row).
    if exit_census_on() {
        crate::ffm_fast::exit_summary();
    }
    report_filechannel_fast_io();
    // The socket transfer / selector census. Same exit path and the same
    // argument as `report_filechannel_fast_io` above: a throughput number
    // quoted without it cannot distinguish "the reusable buffer served every
    // transfer" from "every transfer allocated and the host happened to be
    // quieter". Opt-in via `CRATONVM_SC_IO_STATS`, and it prints every row
    // even when all are zero, because `scratch hit=0` is the single most
    // useful thing it can say.
    cratonvm_native_io::socket_fast_io::stats::report();
    crate::craton_gpu::dispatch_timing::report();
    // The corrupt-cell census. HERE and not in `vm-cli`, because a JUnit runner
    // exits through `System.exit` and never reaches `vm-cli`'s normal-return
    // arm -- which is how a 1975-class sweep produced this line in zero logs
    // and read as a clean run.
    // Whether `CRATONVM_SCALAR_DEOPT` actually reached this run. Same exit path
    // and the same reason as the census below it: a JUnit runner leaves through
    // `System.exit`, so a gauntlet sweep that reported engagement anywhere else
    // would report it in zero logs.
    // nio21 wave 9 lane L4: `[scalar-deopt] census` printed whenever a deopt
    // materialized a scalar-replaced object, which an ordinary JIT'd program
    // does; exit-census only now.
    if exit_census_on() {
        cratonvm_types::scalar_deopt_census::exit_summary();
    }
    cratonvm_types::cell_census::exit_summary();
    // Same exit path and the same argument: the netty runner this counter was
    // built for exits through `System.exit` on its failing test, so a line
    // printed only from `vm-cli`'s normal-return arm is absent from every run
    // worth reading. See `arena_translation_exit_summary`.
    crate::unsafe_natives_ext::arena_translation_exit_summary(
        cratonvm_types::flags::runtime_flag_on("CRATONVM_GC_STATS"),
    );
    // A3 (2026-09-01): the descriptor-coercion guard and the ref-word
    // degradation guard used to print per occurrence at WARN — 2048 lines on a
    // hello-world boot, which is how an instrument stops being read. They now
    // count silently and report here, for the same reason as the two above: a
    // JUnit runner leaves through `System.exit`. Both are `Once`-guarded and
    // print nothing at zero.
    cratonvm_types::compact_value::coercion_census::exit_summary();
    cratonvm_types::compact_value::degradation_exit_summary();
    // The JVMS 6.5 uninstantiable-receiver census, on the same exit path and
    // for the same reason: a JUnit runner leaves through `System.exit`, so a
    // summary printed anywhere else appears in zero logs of a suite sweep.
    // Silent unless a native handed back an abstract/interface receiver.
    cratonvm_native_api::instantiable::exit_summary();
    // Same argument, same exit path: the post-remap stale-frame-word detector
    // splits its hits into words something RESUMES from and words nothing
    // reads, and `resumed_from=0 dead_region=N` is the REPAIRED state rather
    // than a quiet one. A detector that only speaks when it finds something
    // cannot say that.
    cratonvm_types::stale_remap_census::exit_summary();
}

/// The Vector API engagement counters, at exit.
///
/// Printed when either counter moved, or when
/// `CRATONVM_VECTOR_INTRINSICS_STATS=1` asks for it explicitly — which is what
/// makes a printed `handled=0 fell_back=N` a finding rather than a silence. A
/// speedup quoted for `vector_support_intrinsics` without this line is
/// unreadable: "the kernels ran" and "every call took the fallback while the
/// host happened to be quieter" produce the same wall clock.
///
/// Silent for the overwhelming majority of programs, which never touch
/// `jdk.incubator.vector` and would only see noise.
fn report_vector_intrinsics() {
    let (handled, fell_back) = crate::vector_support_intrinsics::vector_intrinsic_counts();
    let asked = cratonvm_types::flags::runtime_var("CRATONVM_VECTOR_INTRINSICS_STATS")
        .ok()
        .as_deref()
        == Some("1");
    // nio21 wave 8 lane L4: a moved counter alone prints only under
    // `CRATONVM_DBG=exit-census`; a Vector API program on HotSpot prints no
    // such line.
    if !asked && !((handled != 0 || fell_back != 0) && exit_census_on()) {
        return;
    }
    eprintln!("[cratonvm] vector intrinsics: handled={handled} fell_back={fell_back}");
    // The per-entry rows, which SUM to the totals above. Only when asked:
    // nine lines is the right amount of detail for someone tuning coverage and
    // the wrong amount for someone reading a test log.
    if asked {
        for (name, h, f) in crate::vector_support_intrinsics::vector_intrinsic_census() {
            if h != 0 || f != 0 {
                eprintln!("[cratonvm] vector intrinsics:   {name} handled={h} fell_back={f}");
            }
        }
    }
}

/// The `FileChannelImpl.read/write(ByteBuffer)` fast-path engagement counters,
/// at exit.
///
/// Same rule as `report_vector_intrinsics` above, and for the same reason: a
/// `FileChannel` speedup quoted without this line cannot distinguish "the
/// native ran on every read" from "every read refused and the host was
/// quieter". Printed when the path was touched at all, or when
/// `CRATONVM_FC_FAST_IO_STATS=1` asks — so a run that PRINTS
/// `read fast=0 refused=N` is a finding rather than a silence.
fn report_filechannel_fast_io() {
    let asked = cratonvm_types::flags::runtime_var("CRATONVM_FC_FAST_IO_STATS")
        .ok()
        .as_deref()
        == Some("1");
    // nio21 wave 8 lane L4: "touched" alone no longer prints -- any program
    // that reads a file through a channel (class loading from a jar does)
    // touched it, and HotSpot prints no such row. `CRATONVM_DBG=exit-census`
    // keeps the touched-only reading.
    if !asked && !(exit_census_on() && cratonvm_native_io::file_channel_fast_read::stats::touched())
    {
        return;
    }
    eprintln!(
        "{}",
        cratonvm_native_io::file_channel_fast_read::stats::report()
    );
}

/// `CRATONVM_DBG=exit-census` (`CRATONVM_DBG_EXIT_CENSUS=1`): print the exit
/// lines that used to fire on any program -- `System.exit(N) called`,
/// `shutdown hooks: ran=`, and the engagement rows that printed whenever their
/// path was merely touched. HotSpot prints none of them (nio21 wave 8 lane
/// L4). `vm-cli` reads the same key for its own exit lines.
pub fn exit_census_on() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_EXIT_CENSUS")
}

type PreExitHook = fn(code: i32);
static PRE_EXIT_HOOK: std::sync::OnceLock<PreExitHook> = std::sync::OnceLock::new();

/// Install a pre-`std::process::exit` callback that fires from
/// `native_system_exit` / `native_runtime_exit` immediately before the
/// process is torn down. Intended for diagnostic dumps (dispatch_trace
/// ring, last-N-bytecodes printout). First-installer wins вЂ” subsequent
/// calls are silent no-ops, matching `OnceLock` semantics.
pub fn set_pre_exit_hook(hook: PreExitHook) {
    let _ = PRE_EXIT_HOOK.set(hook);
}

fn invoke_pre_exit_hook(code: i32) {
    if let Some(hook) = PRE_EXIT_HOOK.get() {
        hook(code);
    }
}

/// gci i11/ja (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
/// item 2): the exit natives' call of the pre-exit hook. The hook reports an
/// attached debugger's `VM_DEATH` and, for a suspending `VMDeath` request,
/// holds this thread GC-safe until the debugger resumes it
/// (`cratonvm_vm::debug::report_vm_death`). That hold used to open its
/// blocking region on a `&mut JvmThread` re-derived from the registry while
/// this native's `ctx` holds the thread's `&mut` (Rust aliasing UB). With
/// `CRATONVM_EXIT_HOOK_GC_SAFE` the region is opened HERE, on `ctx`, and the
/// hold, finding its thread already blocked, only waits. The hook's other
/// work (staged-archive cleanup, the JFR dump-on-exit file, the exit
/// censuses) reads no Java object, so it may run blocked; a collection
/// another thread requests meanwhile no longer waits for it (the switch's
/// pause-shape change, hence default off).
fn invoke_pre_exit_hook_from(ctx: &mut dyn NativeContext, code: i32) {
    // zt t12/tg5 (`zgc-zt11tg4-*` item 4, `CRATONVM_ZGC_TICK_EXIT_NATIVE_WAITS`,
    // default ON, `=0` kill): HotSpot's `before_exit` stops its ZGC director
    // and waits for a running cycle on every exit path; the launcher's
    // main-thread arms did, the exit natives' arm only stopped (in the hook,
    // whose thread state is not known there). Here the context is this
    // thread's own, so the wait can be GC-safe. Before the hook and outside
    // its region; a no-op off ZGC and without a tick.
    if zgc_tick_exit_native_waits() {
        ctx.zgc_stop_director_tick_before_exit();
    }
    if !exit_hook_gc_safe() {
        invoke_pre_exit_hook(code);
        return;
    }
    // A wait HotSpot performs inside the VM (the exiting thread posts
    // `VM_DEATH` and is held in the VM, `_thread_in_vm` / RUNNABLE): the
    // VM-wait region under `CRATONVM_VM_INTERNAL_WAITS_RUNNABLE`, not a
    // kind-less WAITING door (`native-builtins/tests/unrecorded_wait_site_ratchet.rs`).
    // Either region raises `in_blocked_region`, which the debugger hold reads.
    crate::begin_vm_internal_wait(ctx);
    invoke_pre_exit_hook(code);
    ctx.end_blocking_region();
}

/// `CRATONVM_EXIT_HOOK_GC_SAFE` (gci i11/ja, default off): see
/// [`invoke_pre_exit_hook_from`].
fn exit_hook_gc_safe() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_EXIT_HOOK_GC_SAFE")
}

/// `CRATONVM_ZGC_TICK_EXIT_NATIVE_WAITS` (zt t12/tg5, default ON, `=0`
/// kill): see [`invoke_pre_exit_hook_from`]. Reachable only on ZGC with
/// `-XX:ZCollectionInterval` (or the opt-in idle tick): the context's
/// `zgc_stop_director_tick_before_exit` waits only for a tick pause in
/// flight.
fn zgc_tick_exit_native_waits() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_TICK_EXIT_NATIVE_WAITS")
}

/// W7-90: the read-side slot-map sweep's **exit-path** trigger.
///
/// The launcher sweeps immediately after `main(String[])` returns, which is the
/// point in the process with the most classes loaded. Nothing on that path is
/// reached when the application terminates itself — and the fixtures this
/// instrument is aimed at (SbRunner, Surefire, every Spring Boot app) end in
/// `System.exit`, `Runtime.exit` or `Runtime.halt`. A sweep wired only to the
/// return path would report nothing on exactly the runs that matter, which is
/// the same "detector that never runs" shape W7-69 §7.2 filed and this lane is
/// closing. All three natives call this immediately before
/// `std::process::exit`, after their soft-return escape hatches, so a
/// soft-returned exit does not consume the census the launcher would print
/// later.
///
/// `pub(crate)` since 2026-08-12, and the reason is the third exit door.
/// The process exit (`exit_process` since gc-common w11-f; it was
/// `std::process::exit`) has **six** call sites in this crate, not three: four
/// triples on `org/apache/maven/surefire/booter/ForkedBooter` are registered
/// from `register_essential_natives_with_shims` — LIVE in **both** modes — onto
/// three bodies in `test_frameworks.rs` that each terminate on their own and
/// never reach `native_system_exit`. Registration is the gate, so real
/// `ForkedBooter` bytecode never runs: on a Surefire fork the launcher's
/// post-`main` line is skipped **and** these three natives are skipped, and the
/// sweep printed nothing at all on precisely the corpus this trigger was
/// written for. Those three bodies call this helper rather than open-coding a
/// fourth gate — one gated block with no `else`, one place to read. See
/// W7-90-slot-map-sweep-caller.md §2.2.1.
///
/// Gated with **no `else`** and observation-only: the report is printed by the
/// sweep and dropped here. With the flag off it is one `OnceLock` load and a
/// branch, on a path that runs once per process.
///
/// One limit, stated rather than hidden: the per-slot rows go through
/// `tracing::warn!` and `std::process::exit` does not unwind or flush a
/// subscriber, so a hard exit can print the stderr summary and lose the rows.
/// The fix is not a second emitter — two detectors on one primitive drift and
/// then disagree — it is to prefer the launcher's post-`main` trigger when a
/// workload can be made to return.
pub(crate) fn sweep_declared_slot_maps_before_exit(ctx: &dyn NativeContext, trigger: &str) {
    if cratonvm_native_api::layout_alias::enabled() {
        let _ = cratonvm_native_api::read_alias::sweep_declared_slot_maps_at(ctx, trigger);
    }
}

// ---------------------------------------------------------------------------
// java.lang.System natives
// ---------------------------------------------------------------------------

pub(crate) fn native_system_identity_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = the object (static method, no receiver)
    match args.first() {
        Some(Value::Object(Some(obj_ref))) => {
            let hash = ctx.identity_hash_code(*obj_ref);
            Ok(Some(Value::Int(hash)))
        }
        Some(Value::Object(None)) => Ok(Some(Value::Int(0))),
        _ => Ok(Some(Value::Int(0))),
    }
}

/// JIT round 21, lane native (RD16-3; `CRATONVM_JIT_IDENTITY_HASH_LEAF`,
/// default ON): `System.identityHashCode` claimed LEAF
/// ([`cratonvm_native_api::NativeMethodRegistry::set_leaf`]). The hash the
/// header holds or mints in place (`identity_hash_code_installed`: no
/// allocation, no lock, no GC point) answers on the leaf path; an object whose
/// hash must be displaced into its monitor declines BEFORE any effect and runs
/// [`native_system_identity_hash_code`] on the full funnel.
pub(crate) fn native_system_identity_hash_code_leaf(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Object(Some(obj_ref))) = args.first() {
        if let Some(hash) = ctx.identity_hash_code_installed(*obj_ref) {
            return Ok(Some(Value::Int(hash)));
        }
        if ctx.decline_leaf_call() {
            return Ok(None);
        }
    }
    native_system_identity_hash_code(ctx, args)
}

/// `CRATONVM_JIT_IDENTITY_HASH_LEAF` (JIT round 21, lane native; default ON,
/// `=0` registers `System.identityHashCode` and `Object.hashCode` non-leaf as
/// before). Read at registration (per registry, no cache).
pub(crate) fn identity_hash_leaf_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IDENTITY_HASH_LEAF")
}

pub(crate) fn native_system_current_time_millis(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    use std::time::{SystemTime, UNIX_EPOCH};
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    Ok(Some(Value::Long(millis)))
}

/// HotSpot's `Klass::external_name()` for an object's class: dotted, and an
/// array rendered as `component[]` rather than as its `[L…;` descriptor.
///
/// Used only on `arraycopy`'s cold error paths, where the message names the
/// offending class the way a Java stack trace would.
fn external_class_name_of(ctx: &mut dyn NativeContext, obj: ObjectRef) -> String {
    let class_id = ctx.class_id_of_object(obj);
    let internal = ctx.class_name_of_id(class_id).unwrap_or_default();
    external_class_name(&internal)
}

/// Descriptor -> HotSpot external name. `[Ljava/lang/String;` ->
/// `java.lang.String[]`, `[I` -> `int[]`, `java/lang/String` ->
/// `java.lang.String`.
fn external_class_name(internal: &str) -> String {
    cratonvm_types::error::arraycopy_message::external_class_name(internal)
}

/// HotSpot's per-element `ArrayStoreException` text for a reference copy whose
/// source holds an element the destination component type cannot accept.
///
/// It names the source ARRAY and the destination COMPONENT — not the offending
/// index, which is why the previous "source element at index N" wording could
/// not have come from HotSpot.
pub(crate) fn element_type_mismatch_message(
    ctx: &mut dyn NativeContext,
    src: ObjectRef,
    dst_elem_class: cratonvm_types::ClassId,
) -> String {
    // On a reference array the heap header's class id IS the component class,
    // so `class_id_of_object(src)` is the source array's component.
    let src_component = ctx
        .class_name_of_id(ctx.class_id_of_object(src))
        .unwrap_or_default();
    let dst_component = ctx.class_name_of_id(dst_elem_class).unwrap_or_default();
    // HotSpot's `ObjArrayKlass::do_copy` picks between two sentences: "element
    // type mismatch" only when the destination component is a subtype of the
    // source's (an element-by-element check could have passed), otherwise
    // "type mismatch: can not copy S[] into D[]" (interpreter round i1 wave
    // 29, lane L4; `tools/probes/interp/L4/L4W29ArraycopyArrayElements.java`).
    // Only the wording depends on it: the copy is element by element either
    // way, so the prefix before the refused element is committed in both.
    let src_component_id = ctx.class_id_of_object(src);
    // JIT round 21 wave 26 (lane vhase; `CRATONVM_ARRAYCOPY_HIDDEN_COMPONENT_TEXT`):
    // a HIDDEN component cannot be looked up by name, so the name rule below
    // answered "unknown, element mismatch" for `String[] -> H[]`, and the
    // sentence dotted the hidden class's `/0x<hex>` tail. With either
    // component hidden, decide on class ids and render each component by its
    // class's own hidden flag.
    let hidden = arraycopy_hidden_component_text_enabled()
        && (ctx.is_class_hidden(src_component_id) || ctx.is_class_hidden(dst_elem_class));
    let element_sentence = if hidden {
        src_component_id == dst_elem_class
            || ctx.is_subclass(dst_elem_class, src_component_id)
            || src_component == "java/lang/Object"
            || (!ctx.is_class_hidden(src_component_id)
                && ctx
                    .class_assignable_to_name(dst_elem_class, &src_component)
                    .unwrap_or(false))
    } else {
        src_component_id == dst_elem_class
            || ctx.is_subclass(dst_elem_class, src_component_id)
            || arraycopy_component_is_subtype(ctx, &dst_component, &src_component)
    };
    if hidden {
        let src_text = crate::lang_class::dotted_binary_name_of_class(
            &*ctx,
            Some(src_component_id),
            &src_component,
        );
        let dst_text = crate::lang_class::dotted_binary_name_of_class(
            &*ctx,
            Some(dst_elem_class),
            &dst_component,
        );
        return if element_sentence {
            cratonvm_types::error::arraycopy_message::element_type_mismatch_rendered(
                &src_text, &dst_text,
            )
        } else {
            cratonvm_types::error::arraycopy_message::reference_type_mismatch_rendered(
                &src_text, &dst_text,
            )
        };
    }
    if element_sentence {
        cratonvm_types::error::arraycopy_message::element_type_mismatch(
            &src_component,
            &dst_component,
        )
    } else {
        cratonvm_types::error::arraycopy_message::reference_type_mismatch(
            &src_component,
            &dst_component,
        )
    }
}

/// `CRATONVM_ARRAYCOPY_HIDDEN_COMPONENT_TEXT` (default ON, both modes; JIT
/// round 21 wave 26, lane vhase): a reference `System.arraycopy` refusal whose
/// source or destination component is a HIDDEN class picks HotSpot's sentence
/// on class ids (`String[] -> H[]` is `type mismatch: can not copy
/// java.lang.String[] into H[]`, since `H` is no `String`) and keeps the hidden
/// class's `/0x<hex>` tail (`Klass::external_name()`), judged by the class's
/// hidden flag. Measured, HotSpot 25.0.3, `R21Fz10VhCovariant1` rows
/// `arraycopy String[] -> hidden[]` / `Object[] -> hidden[]`. `0` restores the
/// name-only rule (element mismatch, dotted name).
fn arraycopy_hidden_component_text_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ARRAYCOPY_HIDDEN_COMPONENT_TEXT")
    })
}

/// JVMS §4.10.1.2 subtyping between two reference COMPONENT types given by
/// internal name, for `element_type_mismatch_message`'s choice of wording:
/// arrays are covariant in reference components, identical in primitive ones,
/// and subtypes of `Object`, `Cloneable` and `Serializable`. Two plain classes
/// are asked of the class table by name; a name it does not know answers
/// `true`, which keeps the historical "element type mismatch" sentence.
fn arraycopy_component_is_subtype(ctx: &mut dyn NativeContext, sub: &str, sup: &str) -> bool {
    if sub == sup || sup == "java/lang/Object" {
        return true;
    }
    // `Lx;` or `[...` of an array descriptor's element, as an internal name.
    fn reference_element(element: &str) -> Option<&str> {
        if element.starts_with('[') {
            return Some(element);
        }
        element.strip_prefix('L').and_then(|e| e.strip_suffix(';'))
    }
    match (sub.strip_prefix('['), sup.strip_prefix('[')) {
        (Some(_), None) => matches!(sup, "java/lang/Cloneable" | "java/io/Serializable"),
        (None, Some(_)) => false,
        (Some(sub_elem), Some(sup_elem)) => {
            match (reference_element(sub_elem), reference_element(sup_elem)) {
                (Some(a), Some(b)) => arraycopy_component_is_subtype(ctx, a, b),
                // A primitive element: only the identical array type, and
                // `sub != sup` here.
                _ => false,
            }
        }
        (None, None) => match (ctx.class_id_by_name(sub), ctx.class_id_by_name(sup)) {
            (Some(a), Some(b)) => a == b || ctx.is_subclass(a, b),
            _ => true,
        },
    }
}

pub(crate) fn native_system_arraycopy(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: src (Object), srcPos (int), dest (Object), destPos (int), length (int)
    let src = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            // HotSpot's `JVM_ArrayCopy` uses a bare `THROW(NPE)` for both
            // null arguments — no detail message at all, and no way to tell
            // which of the two was null from the exception. Inventing one
            // here would be a divergence, not an improvement.
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: None,
            }
            .into());
        }
    };
    let src_pos = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let dest = match args.get(2) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            // See the `src` arm above: HotSpot throws a message-less NPE.
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: None,
            }
            .into());
        }
    };
    let dest_pos = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let length = match args.get(4) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };

    // Validate that src and dest are arrays.
    //
    // The whole rest of this function follows HotSpot's check ORDER, which is
    // observable because each stage throws a different CLASS: null -> NPE,
    // not-an-array -> ArrayStoreException, element-type mismatch ->
    // ArrayStoreException, bad range -> ArrayIndexOutOfBoundsException. See
    // `JVM_ArrayCopy` plus `TypeArrayKlass::copy_array` /
    // `ObjArrayKlass::copy_array`; `probes/PreconditionsFormatterProbe`'s
    // "arraycopy check precedence" rows pin every pairwise ordering that a
    // reordering would flip.
    use cratonvm_types::error::arraycopy_message;
    use cratonvm_types::ObjectKind;
    if ctx.heap_kind_of(src) != ObjectKind::Array {
        let name = external_class_name_of(ctx, src);
        return Err(cratonvm_types::error::RuntimeError::ArrayStoreException {
            message: arraycopy_message::source_not_an_array(&name),
        }
        .into());
    }
    if ctx.heap_kind_of(dest) != ObjectKind::Array {
        let name = external_class_name_of(ctx, dest);
        return Err(cratonvm_types::error::RuntimeError::ArrayStoreException {
            message: arraycopy_message::destination_not_an_array(&name),
        }
        .into());
    }

    // Element-type compatibility.
    //
    // Three cases:
    //  1. Both primitive arrays of the same element type в†’ bulk-safe copy.
    //  2. One primitive, the other reference (or two primitives of
    //     different kinds) в†’ bulk-reject with ArrayStoreException.
    //  3. Both reference arrays в†’ per-element assignability check against
    //     the destination component class, with prefix-commit on failure
    //     (JLS В§5.5 / `java.lang.System.arraycopy` contract).
    //
    // This must run BEFORE the range check: `arraycopy(int[4], 0, long[4], 0,
    // 9)` violates both, and HotSpot reports the type mismatch. It used to run
    // after, so that call threw an AIOOBE where a `catch (ArrayStoreException)`
    // was written.
    use cratonvm_types::ArrayElementType;
    let src_elem = ctx.heap_element_type_of(src);
    let dest_elem = ctx.heap_element_type_of(dest);
    // Two `AbstractStringBuilder` accommodations copy ACROSS element types on
    // purpose (see the arms further down). Decide here whether this call is
    // one of them, so the rejection below can let them through — but run the
    // copies themselves after the range check, which they rely on.
    let sb_capacity_copy = src_elem == ArrayElementType::Char
        && dest_elem == ArrayElementType::Byte
        && is_abstract_string_builder_capacity_copy(ctx);
    let sb_append_copy = matches!(src_elem, ArrayElementType::Byte | ArrayElementType::Boolean)
        && dest_elem == ArrayElementType::Char
        && is_abstract_string_builder_append_copy(ctx);
    if src_elem != dest_elem && !sb_capacity_copy && !sb_append_copy {
        // CRATONVM_DBG_ARRAYCOPY=1 вЂ” dump the Java caller chain + array
        // identities for the element-type mismatch. Env-gated; default output
        // unchanged. Used to localize the Hibernate/H2 "src=Char, dest=Byte"
        // cluster (an array mislabeled at its allocation site).
        if crate::nbflags().dbg_arraycopy {
            let src_cls = ctx.class_id_of_object(src);
            let dest_cls = ctx.class_id_of_object(dest);
            let src_name = ctx.class_name_of_id(src_cls).unwrap_or_default();
            let dest_name = ctx.class_name_of_id(dest_cls).unwrap_or_default();
            let trace = ctx.capture_stack_trace();
            let total = trace.len();
            // Show the INNERMOST ~40 frames (closest to the arraycopy call
            // site); the trace is outermost-first so the tail is what matters.
            let skip = total.saturating_sub(40);
            let mut rendered = String::new();
            for (i, entry) in trace.iter().enumerate().skip(skip) {
                use std::fmt::Write as _;
                let _ = write!(
                    rendered,
                    "\n  #{i}/{total} {cls}.{m} (bci={bci})",
                    cls = entry.class_name,
                    m = entry.method_name,
                    bci = entry.byte_code_index,
                );
            }
            tracing::warn!(
                target: "cratonvm::arraycopy",
                "[DBG_ARRAYCOPY] mismatch src={src_elem:?}({src_name}) dest={dest_elem:?}({dest_name}) \
                 srcLen={srcl} destLen={destl} len={length} caller chain:{rendered}",
                srcl = ctx.array_length(src),
                destl = ctx.array_length(dest),
            );
        }
        return Err(cratonvm_types::error::RuntimeError::ArrayStoreException {
            message: arraycopy_message::type_mismatch(
                arraycopy_message::element_type_name(src_elem),
                arraycopy_message::element_type_name(dest_elem),
            ),
        }
        .into());
    }

    // Bounds checking
    let src_len = ctx.array_length(src) as i32;
    let dest_len = ctx.array_length(dest) as i32;
    if length >= 30_000
        && src_elem == ArrayElementType::Reference
        && cratonvm_types::flags::runtime_flag_on("CRATONVM_G1_DBG_GRAY_PROV")
    {
        eprintln!("[REF-ARRAYCOPY] src={:#x} src_len={src_len} dest={:#x} dest_len={dest_len} length={length}", src.as_ptr() as usize, dest.as_ptr() as usize);
    }

    // SECURITY FIX (V14): widen the end-offset additions to i64 so that
    // `src_pos + length` / `dest_pos + length` cannot wrap to a negative
    // value (Rust `+` wraps in release builds) and silently defeat the
    // `> len` bounds check. The individual >= 0 checks and the
    // ArrayIndexOutOfBoundsException semantics are preserved.
    let src_end = src_pos as i64 + length as i64;
    let dest_end = dest_pos as i64 + length as i64;
    if src_pos < 0
        || dest_pos < 0
        || length < 0
        || src_end > src_len as i64
        || dest_end > dest_len as i64
    {
        // HotSpot names WHICH argument failed and the array's type and
        // length; an index alone cannot distinguish "your source ran out"
        // from "your destination did". The five-way ladder below is its
        // priority order, and it uses the SOURCE array's element type for
        // both sides (the two are equal by the time we get here).
        let ty = arraycopy_message::element_type_name(src_elem);
        let (index, message) = if src_pos < 0 {
            (
                src_pos,
                arraycopy_message::source_index(src_pos, ty, src_len),
            )
        } else if dest_pos < 0 {
            (
                dest_pos,
                arraycopy_message::destination_index(dest_pos, ty, dest_len),
            )
        } else if length < 0 {
            (length, arraycopy_message::negative_length(length))
        } else if src_end > src_len as i64 {
            (
                src_end as i32,
                arraycopy_message::last_source_index(src_pos, length, ty, src_len),
            )
        } else {
            (
                dest_end as i32,
                arraycopy_message::last_destination_index(dest_pos, length, ty, dest_len),
            )
        };
        return Err(
            cratonvm_types::error::RuntimeError::aioobe_with_message(index, message).into(),
        );
    }

    if length == 0 {
        return Ok(None);
    }

    if src_elem != dest_elem {
        if sb_capacity_copy {
            for i in 0..length {
                let val = ctx.get_array_element(src, (src_pos + i) as usize);
                let byte = match val {
                    Value::Int(v) => v & 0xff,
                    _ => 0,
                };
                ctx.set_array_element(dest, (dest_pos + i) as usize, Value::Int(byte));
            }
            return Ok(None);
        }

        if sb_append_copy {
            for i in 0..length {
                let val = ctx.get_array_element(src, (src_pos + i) as usize);
                let ch = match val {
                    Value::Int(v) => v & 0xff,
                    _ => 0,
                };
                ctx.set_array_element(dest, (dest_pos + i) as usize, Value::Int(ch));
            }
            return Ok(None);
        }
        // Unreachable: the only two cross-element-type calls that survive the
        // rejection above are the accommodations, both handled.
        debug_assert!(false, "unhandled cross-element-type arraycopy");
        return Ok(None);
    }

    // Handle overlapping copy (same array).
    //
    // Note (audit-2026-05-16): the pointer-equality check below is correct
    // only because `ObjectRef` is a single-representation wrapper. If
    // `ObjectRef` ever grows multiple in-memory representations (compressed
    // oops, tagged pointers), this comparison must move to a canonicalising
    // helper.
    let same_array = src.as_ptr() == dest.as_ptr();

    if src_elem == ArrayElementType::Reference {
        // Reference-array path: per-element instanceof check against the
        // destination component class. The component class id for a
        // reference array is stored in the heap header (`alloc_array` is
        // given the component class id directly), so
        // `class_id_of_object(dest)` IS the dest component class.
        let dst_elem_class = ctx.class_id_of_object(dest);
        let src_elem_class = ctx.class_id_of_object(src);
        // JIT r23 wave 3c, lane L5: the context's per-VM answer, not a name
        // lookup per call (7% of a 10-element copy's samples).
        let object_class_id = ctx.object_class_id();

        // Fast path: src and dest reference arrays have the *same* component
        // class. Every element already stored in `src` is, by construction,
        // a valid value for a `src` component slot, hence also valid for an
        // identically-typed `dst` slot. No per-element check is needed.
        //
        // This is the structural fix for arrays-of-arrays. For `int[][]`
        // the element objects are primitive `int[]` arrays which all carry
        // the synthetic `ClassId::new(0)` (primitive arrays are allocated
        // with class id 0 вЂ” see `Newarray`/`alloc_multi_array` in the
        // interpreter), while the dest array's stored component class id is
        // the real `[I` class. The old per-element check compared the
        // element's class id (0) against the dest component class id (`[I`)
        // and wrongly threw `ArrayStoreException`. Comparing the *array*
        // component class ids (`class_id_of_object(src/dest)`) sidesteps
        // that mismatch: identical component class id в‡’ assignable.
        // Round 16 lane `arraylist` (opt-in, `CRATONVM_JIT_BULK_REF_ARRAYCOPY`):
        // a copy that needs no per-element store check -- the identical
        // component class below, or an `Object` destination component, which
        // `assignable_to_dst` accepts for every element without looking at
        // it -- is handed to the context's bulk reference copy, which
        // validates and forwards the two arrays once per call instead of once
        // per element. `false` means it wrote nothing; the loops below then
        // run exactly as before. See
        // `docs/feature-designs/native-dispatch-architecture.md`.
        if (src_elem_class == dst_elem_class || Some(dst_elem_class) == object_class_id)
            && {
                // nio21 wave 17: read once per process -- this runs per
                // arraycopy (2.4M declared-flag reads on the w16 `hdr` row).
                static BULK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
                *BULK.get_or_init(|| {
                    cratonvm_types::flags::runtime_flag_default_on(
                        "CRATONVM_JIT_BULK_REF_ARRAYCOPY",
                    )
                })
            }
            && ctx.copy_reference_array_elements(
                src,
                src_pos as usize,
                dest,
                dest_pos as usize,
                length as usize,
            )
        {
            return Ok(None);
        }
        if src_elem_class == dst_elem_class {
            // Same component type вЂ” copy without per-element checks.
            if same_array && src_pos < dest_pos {
                for i in (0..length).rev() {
                    let val = ctx.get_array_element(src, (src_pos + i) as usize);
                    ctx.set_array_element(dest, (dest_pos + i) as usize, val);
                }
            } else {
                for i in 0..length {
                    let val = ctx.get_array_element(src, (src_pos + i) as usize);
                    ctx.set_array_element(dest, (dest_pos + i) as usize, val);
                }
            }
            return Ok(None);
        }

        // Per-element assignability: matches the established pattern in
        // `native_class_is_assignable_from` /
        // `native_class_is_instance` вЂ” identity OR `is_subclass` (which
        // walks both the superclass chain AND implemented interfaces, so
        // it correctly handles dest-element-type-is-interface cases like
        // `Runnable[]`).
        let assignable_to_dst = |ctx: &mut dyn NativeContext, elem: ObjectRef| -> bool {
            if Some(dst_elem_class) == object_class_id {
                // Fast path: every reference is assignable to Object.
                return true;
            }
            // An ARRAY element first, and by the `aastore` predicate alone.
            // A reference array's header holds its COMPONENT class id, so the
            // class-id tests below read a `String[]` element as a `String`
            // and stored it into a `String[]`'s slot -- a `String[]` where a
            // `String` must be, which the next `String.length()` read as a
            // string (interpreter round i1 wave 29, lane L4; probe
            // `tools/probes/interp/L4/L4W29ArraycopyArrayElements.java`).
            // `None` (a context with no class hierarchy) keeps the old tests.
            if ctx.heap_kind_of(elem) == ObjectKind::Array {
                if let Some(verdict) = ctx.aastore_element_assignable(dest, elem) {
                    return verdict;
                }
            }
            let elem_class = ctx.class_id_of_object(elem);
            if elem_class == dst_elem_class || ctx.is_subclass(elem_class, dst_elem_class) {
                return true;
            }
            // A child classloader may define an implementation while delegating
            // its interface to the parent loader. Its exact ClassId edge can be
            // lost in the flat class store even though the real loader-resolved
            // hierarchy is assignable (e.g. Jackson's JDKKeyDeserializers ->
            // KeyDeserializers during Spring's ModifiedClassPath test setup).
            // Reuse reflection's bounded, loader-aware hierarchy walk rather
            // than treating all same-named classes as interchangeable.
            if let Some(dst_name) = ctx.class_name_of_id(dst_elem_class) {
                if crate::lang_class::loader_aware_reflect_assignable(
                    ctx,
                    elem_class,
                    dst_elem_class,
                    &dst_name,
                ) {
                    return true;
                }
            }
            // Array-typed elements: a primitive array (`int[]`, `byte[]`,
            // вЂ¦) carries the synthetic `ClassId::new(0)`, so the class-id
            // comparison above can never match a real array component
            // class. Fall back to a structural descriptor comparison so
            // e.g. an `int[]` element is accepted into an `int[][]` whose
            // component class is the loaded `[I` class.
            // Ask the VM's own `aastore` predicate, which decides this case on
            // descriptors and is the same one the interpreter opcode, the JIT
            // helper and `java.lang.reflect.Array.set` use.
            //
            // It replaces a BLANKET: any array element used to be accepted
            // into any array-of-array destination, so this succeeded where
            // HotSpot throws (MEASURED 2026-08-28, `probes/L7AseMsg`):
            //
            //   System.arraycopy(new Object[]{new String[]{"s"}}, 0,
            //                    new Integer[1][], 0, 1)
            //     HotSpot   ArrayStoreException
            //     CratonVM  copied
            //
            // The predicate is additive — it fails open for anything it cannot
            // resolve precisely — so narrowing to it cannot manufacture a false
            // `ArrayStoreException`. `None` means a context with no class
            // hierarchy (mocks), where the old blanket is still the answer.
            if ctx.heap_kind_of(elem) == ObjectKind::Array {
                if let Some(verdict) = ctx.aastore_element_assignable(dest, elem) {
                    return verdict;
                }
                if let Some(dst_name) = ctx.class_name_of_id(dst_elem_class) {
                    // dst component is itself an array type.
                    if dst_name.starts_with('[') {
                        return true;
                    }
                }
            }
            false
        };

        // For same-array overlap with `src_pos < dest_pos` the actual
        // copy must run backward (highв†’low) so we don't clobber unread
        // source slots. Doing per-element check-then-write in that
        // direction would let an early write corrupt source data read
        // later, breaking the type check. So in that case we do a
        // forward type-CHECK-only pre-pass first (no writes); on failure
        // we throw with NO elements written (an empty prefix вЂ” still
        // satisfies the "elements [0, i) are committed" contract since
        // i==0 means nothing was committed). If the pre-pass succeeds,
        // we then copy backward without re-checking.
        //
        // For all other cases (different arrays, or `src_pos >= dest_pos`
        // in the same array) forward direction is safe, so we do
        // check-then-write per element and observe true partial commit
        // on failure: indices [0, i) of dest at positions
        // `dest_pos..dest_pos+i` are written before the throw.
        if same_array && src_pos < dest_pos {
            // Forward type-check pre-pass вЂ” no writes.
            for i in 0..length {
                let val = ctx.get_array_element(src, (src_pos + i) as usize);
                if let Value::Object(Some(elem)) = val {
                    if !assignable_to_dst(ctx, elem) {
                        return Err(cratonvm_types::error::RuntimeError::ArrayStoreException {
                            message: element_type_mismatch_message(ctx, src, dst_elem_class),
                        }
                        .into());
                    }
                }
            }
            // Pre-check passed вЂ” copy backward to handle overlap.
            for i in (0..length).rev() {
                let val = ctx.get_array_element(src, (src_pos + i) as usize);
                ctx.set_array_element(dest, (dest_pos + i) as usize, val);
            }
        } else {
            // JIT round 23 wave 2e (`jit-r23-w2-proxy-proposals.md` item 2,
            // `CRATONVM_ZGC_BULK_CHECKED_REF_ARRAYCOPY`, default ON, `0` kill):
            // the context copies the longest prefix whose elements it can
            // prove assignable cheaply (null, or a plain object of the
            // component class or a subclass), with the arrays validated once;
            // this loop resumes at the first element it did not prove, so the
            // full predicate, the ArrayStoreException and its partial commit
            // are unchanged.
            let mut first: i32 = 0;
            if {
                static CHECKED_BULK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
                *CHECKED_BULK.get_or_init(|| {
                    cratonvm_types::flags::runtime_flag_default_on(
                        "CRATONVM_ZGC_BULK_CHECKED_REF_ARRAYCOPY",
                    )
                })
            } {
                if let Some(n) = ctx.copy_reference_array_elements_checked(
                    src,
                    src_pos as usize,
                    dest,
                    dest_pos as usize,
                    length as usize,
                    dst_elem_class,
                ) {
                    first = i32::try_from(n).unwrap_or(length).min(length);
                }
            }
            // Forward direction is safe вЂ” check-then-write per element.
            for i in first..length {
                let val = ctx.get_array_element(src, (src_pos + i) as usize);
                if let Value::Object(Some(elem)) = val {
                    if !assignable_to_dst(ctx, elem) {
                        // Prefix [0, i) at positions `dest_pos..dest_pos+i`
                        // has already been written. This is the spec
                        // partial-commit behavior.
                        return Err(cratonvm_types::error::RuntimeError::ArrayStoreException {
                            message: element_type_mismatch_message(ctx, src, dst_elem_class),
                        }
                        .into());
                    }
                }
                ctx.set_array_element(dest, (dest_pos + i) as usize, val);
            }
        }
        return Ok(None);
    }

    // Primitive-array fast path вЂ” element types already verified equal.
    //
    // Use the `bulk_array_copy` intrinsic exposed by `NativeContext`
    // (added round 3 вЂ” see `native-api/src/registry.rs:270`). The VM
    // override implements this with `copy_within` / `copy_nonoverlapping`
    // on the underlying primitive backing, which is dramatically faster
    // than per-element trips through the trait object. The intrinsic
    // handles same-array overlap internally.
    //
    // Falls back to the per-element loop if the intrinsic refuses (e.g.
    // an unexpected element-type mismatch the pre-check above didn't
    // catch). Element types are already verified equal at this point so
    // failure is not expected, but the fallback preserves the original
    // semantics defensively.
    if ctx.bulk_array_copy(
        src,
        src_pos as usize,
        dest,
        dest_pos as usize,
        length as usize,
    ) {
        return Ok(None);
    }

    // Fallback per-element loop (only reached on intrinsic failure).
    if same_array && src_pos < dest_pos {
        // Copy backward to handle overlap
        for i in (0..length).rev() {
            let val = ctx.get_array_element(src, (src_pos + i) as usize);
            ctx.set_array_element(dest, (dest_pos + i) as usize, val);
        }
    } else {
        // Copy forward
        for i in 0..length {
            let val = ctx.get_array_element(src, (src_pos + i) as usize);
            ctx.set_array_element(dest, (dest_pos + i) as usize, val);
        }
    }

    Ok(None)
}

fn is_abstract_string_builder_capacity_copy(ctx: &mut dyn NativeContext) -> bool {
    let trace = ctx.capture_stack_trace();
    let mut string_ctor = false;
    let mut string_builder_to_string = false;

    for entry in &trace {
        let class_name = entry.class_name.as_ref();
        let method_name = entry.method_name.as_ref();
        if class_name == "java/lang/AbstractStringBuilder"
            && method_name == "ensureCapacityNewCoder"
        {
            return true;
        }
        if class_name == "java/lang/String" && method_name == "<init>" {
            string_ctor = true;
        }
        if class_name == "java/lang/StringBuilder" && method_name == "toString" {
            string_builder_to_string = true;
        }
    }

    string_ctor && string_builder_to_string
}

fn is_abstract_string_builder_append_copy(ctx: &mut dyn NativeContext) -> bool {
    let trace = ctx.capture_stack_trace();
    let mut string_get_bytes = false;
    let mut asb_compact_copy = false;

    for entry in &trace {
        let class_name = entry.class_name.as_ref();
        let method_name = entry.method_name.as_ref();
        if class_name == "java/lang/String" && method_name == "getBytes" {
            string_get_bytes = true;
        }
        // `String.getBytes(byte[], ?)` feeds both append and insert in JDK 25.
        // Our synthetic builders intentionally retain a char[] backing, so either
        // compact-string helper needs the same byte-to-char bridge.
        if class_name == "java/lang/AbstractStringBuilder"
            && matches!(method_name, "append" | "insert")
        {
            asb_compact_copy = true;
        }
    }

    string_get_bytes && asb_compact_copy
}

pub(crate) fn native_thread_current_thread(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let thread_obj = ctx.current_thread_object();
    Ok(Some(Value::Object(Some(thread_obj))))
}

/// T2.2.21: `Thread.sleep(long millis, int nanos)`.
///
/// The public `Thread.sleep(long, int)` overload validates its arguments
/// and rounds sub-millisecond `nanos` up by one millisecond (matching
/// HotSpot's behavior вЂ” the JDK's java-side implementation does the same
/// rounding before delegating to the single-argument native). We expose
/// this as its own native so the real JDK class file's ACC_NATIVE slot
/// for `sleep(JI)V` is satisfied in NEW-11 default mode.
pub(crate) fn native_thread_sleep_millis_nanos(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Interpreter round i5 wave 108, lane L6: the long as `sleep0`,
    // `sleepNanos0` and `Unsafe.park` decode it -- a category-2 argument may
    // arrive tag-lost as a `Double` (`CompactValue::to_value`), which read as
    // 0 here: `Thread.sleep(-1, 0)` slept nothing instead of throwing, and
    // `Thread.sleep(50, 0)` returned at once.
    let millis = match args.first() {
        Some(Value::Long(v)) => *v,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Int(i)) => i64::from(*i),
        _ => 0,
    };
    let nanos = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    // Round 14 wave 7 (lane trace6; TW6-3): which of `sleep(long, int)`'s two
    // checks failed, for the capture's `Thread.sleep` frame.
    if millis < 0 {
        return served_two_site_argument_check(ctx, false, "timeout value is negative");
    }
    if !(0..=999_999).contains(&nanos) {
        return served_two_site_argument_check(ctx, true, "nanosecond timeout value out of range");
    }
    // RD.9: combine millis + nanos into a single nanosecond value and delegate
    // to `sleepNanos0` so sub-millisecond sleeps honour the requested
    // precision (the previous round-up-to-millis path lost precision for
    // sub-ms sleeps вЂ” Thread.sleep(0, 500_000) used to block for 1ms).
    let total_nanos = (millis as i128)
        .saturating_mul(1_000_000)
        .saturating_add(nanos as i128);
    // Wave 24 (L7): a zero sleep still goes to `sleepNanos0`, which checks
    // the interrupt first (HotSpot: a pre-interrupted `sleep(0, 0)` throws).
    let clamped = total_nanos.clamp(0, i64::MAX as i128) as i64;
    let delegate_args = [Value::Long(clamped)];
    native_thread_sleep_nanos(ctx, &delegate_args)
}

/// `InterruptedException("sleep interrupted")`, which is what HotSpot throws
/// out of `Thread.sleep` — and the message is the whole reason this exists.
///
/// `RuntimeError::InterruptedException` is a UNIT variant: it can carry no
/// text, and every thrower of it therefore produced a null message. That is
/// right for `Object.wait` and `Thread.join`, which is presumably why nobody
/// noticed, and wrong for `sleep` alone. Materialising the exception with its
/// `String` constructor is the same route `G68-1` used for
/// `InstantiationException`; falling back to the unit variant keeps the throw
/// even if the class cannot be built.
///
/// Interpreter round i1 wave 48, lane L1: on a virtual thread of a VM that
/// asks for HotSpot's DEFAULT virtual-thread shape
/// (`NativeContext::note_virtual_sleep_interrupt`,
/// `CRATONVM_VIRTUAL_SLEEP_CONTINUATION_SHAPE`), the exception is the one
/// `VirtualThread.sleepNanos` throws there, `new InterruptedException()` with
/// no message, built by the JDK's own constructor; `during` says whether the
/// interrupt came before the sleep (`false`) or during it (`true`), which is
/// which of that method's two throws the capture lists.
fn sleep_interrupted(
    ctx: &mut dyn NativeContext,
    during: bool,
) -> cratonvm_types::error::MethodCallFailed {
    // Interpreter round i1 wave 25, lane L1: the interrupt that woke this
    // sleep may be a debugger's `ThreadReference.Stop`, whose throwable
    // replaces the `InterruptedException` (HotSpot's asynchronous exception).
    if let Some(stop) = ctx.take_debugger_stop() {
        return cratonvm_types::error::MethodCallFailed::ExceptionThrown(stop);
    }
    let built = if ctx.note_virtual_sleep_interrupt(during) {
        ctx.new_object_initialized("java/lang/InterruptedException", "()V", &[])
    } else {
        let msg = ctx.create_string("sleep interrupted");
        ctx.new_object_initialized(
            "java/lang/InterruptedException",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(msg))],
        )
    };
    if let Ok(Some(Value::Object(Some(exc)))) = built {
        return cratonvm_types::error::MethodCallFailed::ExceptionThrown(exc);
    }
    // Wave 48 (lane L1): `CRATONVM_DBG_SLEEP_TRACE=1` names why the
    // construction failed (the fallback below carries no message).
    if crate::nbflags().dbg_sleep_trace {
        eprintln!(
            "[SLEEP-TRACE] sleep interrupted: InterruptedException not built \
             (virtual={}): {built:?}",
            ctx.is_current_virtual()
        );
    }
    cratonvm_types::error::MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
        cratonvm_types::error::RuntimeError::InterruptedException,
    ))
}

pub(crate) fn native_thread_sleep(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // WP4.5 вЂ” invokestatic uses generic `pop()` which type-erases the Long
    // bit-pattern down to `Value::Double` via `CompactValue::to_value()`.
    // Until the interpreter does descriptor-aware popping for native args,
    // accept the bit-reinterpreted Double and convert back to long bits.
    let millis = match args.first() {
        Some(Value::Long(ms)) => *ms,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Int(i)) => *i as i64,
        _ => 0,
    };
    // G72-1: same missing check as `Object.wait(long)` had, and the same
    // sibling-knew-better shape -- `native_thread_sleep_millis_nanos` above has
    // it. A negative sleep silently succeeded here because the whole body is
    // guarded by `millis > 0` and nothing else looked at the value.
    if millis < 0 {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: "timeout value is negative".to_string(),
            }
            .into(),
        );
    }
    // Interpreter round i1 wave 24 (L7): HotSpot's `JVM_SleepNanos` checks the
    // interrupt BEFORE it looks at the duration, so a pre-interrupted
    // `Thread.sleep(0)` throws `InterruptedException("sleep interrupted")` and
    // clears the flag. This check used to sit inside `millis > 0`, and
    // `sleep(0)` returned with the flag still set
    // (`tools/probes/interp/L7/L7W23Interrupts.java`, row `pre sleep0`).
    if ctx.is_interrupted(true) {
        return Err(sleep_interrupted(ctx, false));
    }
    if millis > 0 {
        // Keycloak Gap 9 localization (CRATONVM_DBG_SLEEP_TRACE): a worker is
        // stuck in a Thread.sleep poll-loop; sample the Java caller chain so we
        // can identify which loop and what it polls. Sampled + capped to avoid
        // flooding; off by default (one env check per real sleep call).
        if crate::nbflags().dbg_sleep_trace {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static N: AtomicUsize = AtomicUsize::new(0);
            static PRINTED: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            if n % 16 == 0 && PRINTED.fetch_add(1, Ordering::Relaxed) < 60 {
                let st = ctx.capture_stack_trace();
                let frames: Vec<String> = st
                    .iter()
                    .take(12)
                    .map(|e| format!("{}.{}:{}", e.class_name, e.method_name, e.line_number))
                    .collect();
                eprintln!("[SLEEP-TRACE #{n} millis={millis}] {}", frames.join(" <- "));
            }
        }
        // CompletableFuture.runAsync users commonly use short sleeps as
        // scheduling barriers. CratonVM's workers start quickly, but cold Java
        // proxy/linkage and real CompletableFuture submission overhead can make
        // 10 ms handoff sleeps and 100 ms worker sleeps too tight. When a sleep
        // immediately follows async submission, give those short sleeps a small
        // scheduling floor; Thread.sleep only promises to sleep at least the
        // requested duration.
        let effective_millis = crate::async_handoff_sleep_millis(millis);
        let target = std::time::Duration::from_millis(effective_millis as u64);
        // Interpreter round i3 wave 76, lane L6 (page
        // `i75-L6-proposal-one-sleep-body-20261108`): the one sleep body,
        // `sleepNanos0`'s too; this one pumps the scheduled-task registry.
        return sleep_body(
            ctx,
            target,
            millis.saturating_mul(1_000_000),
            // Wave 104 (lane L6): the `jdk.VirtualThreadPinned` `blockingOperation`
            // HotSpot posts for a pinned virtual-thread sleep (`VirtualThread.sleepNanos`
            // -> `parkOnCarrierThread` -> `postPinnedEvent("LockSupport.park")`).
            "LockSupport.park",
            true,
        );
    }
    Ok(None)
}

/// `Thread.getState()` / `Thread.threadState()` for real-JDK mode.
///
/// The JDK bytecode computes the state from `holder.threadStatus`, but the VM
/// never advances that field past 0 (NEW) вЂ” so the real bytecode reports NEW
/// for every thread, including ones that have finished. Strict thread-leak
/// detectors (randomizedtesting's `ThreadLeakControl`, used by the Elasticsearch
/// RestClient suite) then see a finished worker as a live NEW thread and fail
/// with a spurious `ThreadLeakError`. We compute the state from the
/// authoritative VM thread registry instead and return the **canonical**
/// `Thread$State` enum constant (callers compare it with `==`).
pub(crate) fn native_thread_get_state(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let name = match ctx.thread_run_state(this) {
        1 => "RUNNABLE",
        2 => "TERMINATED",
        3 => "WAITING",
        4 => "BLOCKED",
        5 => "TIMED_WAITING",
        _ => "NEW",
    };
    let cid = match ctx.ensure_class_initialized("java/lang/Thread$State") {
        Ok(c) => c,
        Err(_) => return Ok(Some(Value::Object(None))),
    };
    if let Some(idx) = ctx.static_field_index_by_name(cid, name) {
        return Ok(Some(ctx.get_static_field(cid, idx)));
    }
    Ok(Some(Value::Object(None)))
}

/// The canonical constant that `class_name`'s own static field `name` holds,
/// or `None` when the loaded class does not declare it / has not published it.
///
/// A native that answers an enum constant must hand back **the object the
/// class's own `<clinit>` stored**, never a fresh allocation. Enum identity is
/// the whole contract: `==` comparison, the `tableswitch` an enum `switch`
/// compiles to, `EnumSet`/`EnumMap`'s ordinal indexing and `Enum.compareTo` all
/// assume exactly one instance per constant. A minting native satisfies every
/// null-check and every `name()`/`ordinal()` check while breaking all of them —
/// see W7-93 §"Second cause".
///
/// `None` is the honest answer for a fabricated synthetic-JDK stand-in, which
/// has no static field to read; callers fall back to their own construction
/// there. `ensure_class_initialized` can itself fabricate rather than fail (it
/// returns `Ok` for a class it invented), so the static-field lookup — not the
/// `Result` — is what decides whether a real class answered.
pub(crate) fn canonical_enum_constant(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    name: &str,
) -> Option<Value> {
    let cid = ctx.ensure_class_initialized(class_name).ok()?;
    let idx = ctx.static_field_index_by_name(cid, name)?;
    match ctx.get_static_field(cid, idx) {
        v @ Value::Object(Some(_)) => Some(v),
        _ => None,
    }
}

/// A fresh array holding `class_name`'s canonical enum constants in ordinal
/// order — what the real `values()` bytecode (`$VALUES.clone()`) returns.
///
/// **Ask the class, never a hard-coded list.** A javac-generated enum declares
/// one static field of its OWN type per constant in source order, which is
/// ordinal order; the synthetic `$VALUES` has the ARRAY descriptor and so
/// filters itself out. That is the same derivation
/// `stack_walker::option_constant_names` uses, and it is what keeps this
/// version-proof when a JDK adds a constant.
///
/// Returns `None` — so the caller keeps its existing behaviour — unless the
/// class declares at least one constant AND every one of them reads back
/// non-null, because a partially initialised class must not be turned into an
/// array with null holes (`ImmutableCollections$Set12.<init>` NPEs on one).
///
/// GC-SAFETY: `new_ref_array` is the only allocation, and it happens before the
/// array reference is live; `get_static_field` and `set_array_element` do not
/// allocate, so nothing can move underneath the fill loop. Each element is read
/// out of its static — a GC root — rather than cached from the scan above.
pub(crate) fn canonical_enum_values(
    ctx: &mut dyn NativeContext,
    class_name: &str,
) -> Option<ObjectRef> {
    let cid = ctx.ensure_class_initialized(class_name).ok()?;
    let self_descriptor = format!("L{class_name};");
    let names: Vec<String> = ctx
        .declared_fields(cid)
        .into_iter()
        .filter(|f| f.is_static && f.descriptor == self_descriptor)
        .map(|f| f.name)
        .collect();
    if names.is_empty() {
        return None;
    }
    let mut slots = Vec::with_capacity(names.len());
    for name in &names {
        let idx = ctx.static_field_index_by_name(cid, name)?;
        if !matches!(ctx.get_static_field(cid, idx), Value::Object(Some(_))) {
            return None;
        }
        slots.push(idx);
    }
    let arr = ctx.new_ref_array(cid, slots.len());
    for (i, idx) in slots.into_iter().enumerate() {
        let published = ctx.get_static_field(cid, idx);
        ctx.set_array_element(arr, i, published);
    }
    Some(arr)
}

pub(crate) fn native_thread_is_alive(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let alive = if ctx.thread_is_alive(this) { 1 } else { 0 };
    Ok(Some(Value::Int(alive)))
}

/// Armed by the launcher immediately before the application's `main`; consumed
/// by the first application `Thread.start()` (see [`run_armed_logging_prewarm`]).
///
/// gci i1/wr (`gch-h1ak-first-thread-start-initialises-java-util-logging-FIXED-20261012`):
/// per VM, keyed by `vm_identity`. It was one process-wide `AtomicBool`, so with
/// two VMs in a process the first VM's thread start consumed the second VM's
/// arming (AGENTS.md: no process globals for compatibility state). Dropped by
/// [`forget_vm_system_singletons`].
static LOGGING_PREWARM_ARMED: VmScoped<bool> = VmScoped::new();

/// Arm the one-shot `java.util.logging` pre-warm for the first application
/// thread start.
///
/// `java.util.logging`'s lazy first use (`LogManager.<clinit>` <->
/// `Logger.<clinit>`) can deadlock two threads that each make their first
/// `Logger.getLogger` call at once (the Tomcat WebSocket suite did, reliably;
/// `docs/feature-designs/native-dispatch-architecture.md`). The launcher used to defuse it
/// by calling `Logger.getLogger("global")` BEFORE `main`. That also ran
/// `LogManager.<clinit>` at boot, which reads `java.util.logging.manager`
/// exactly once: an application or test fixture that selects its manager with
/// `System.setProperty("java.util.logging.manager", ...)` after start-up (Tomcat's
/// `LoggingBaseTest` does) found the default manager already installed, and
/// `TestAsyncContextImpl.testAsyncIoEnd*` failed on `LogManager.getLogger(..)`
/// returning null. HotSpot initialises `LogManager` lazily, so it is not an
/// observable difference there.
///
/// The race needs a second thread, so the pre-warm now waits for the first
/// `Thread.start0` after `main` begins: still single-threaded, still before any
/// racing thread exists, but after the property has had its chance to be set.
pub fn arm_logging_prewarm_on_first_thread_start(vm_identity: usize) {
    if vm_identity == 0 {
        return;
    }
    LOGGING_PREWARM_ARMED.with(vm_identity, |armed| *armed = true);
}

/// Take this VM's arming, once: `true` for exactly one caller after
/// [`arm_logging_prewarm_on_first_thread_start`]. A VM that was never armed
/// gets no row (the peek answers first).
fn take_logging_prewarm_arming(vm_identity: usize) -> bool {
    if vm_identity == 0 || LOGGING_PREWARM_ARMED.peek(vm_identity, |armed| *armed) != Some(true) {
        return false;
    }
    LOGGING_PREWARM_ARMED.with(vm_identity, |armed| std::mem::take(armed))
}

/// Consume the arming, once, and drive the logging first-use path on the
/// calling thread. Best effort, exactly like the launcher's old boot-time call.
/// The name is `global`, a logger the JDK itself registers, so the logger list
/// an application sees is unchanged.
fn run_armed_logging_prewarm(ctx: &mut dyn NativeContext, starting: ObjectRef) {
    // A JDK-internal service thread (`Common-Cleaner`, the process reaper, ...) is
    // an `InnocuousThread`, and the first one starts from a file-system or
    // zip call long before an application has chosen its `LogManager`; it must
    // neither consume the arming nor run the pre-warm. Measured: the Cleaner
    // thread started from `sun.nio.fs.NativeBuffer` under JUnit, ahead of the
    // Tomcat fixture's `@BeforeClass` that sets `java.util.logging.manager`.
    let name = ctx.class_name_arc_of_id(ctx.class_id_of_object(starting));
    if name.as_deref() == Some("jdk/internal/misc/InnocuousThread") {
        return;
    }
    if !take_logging_prewarm_arming(ctx.vm_identity()) {
        return;
    }
    let name = ctx.create_string("global");
    let _ = ctx.invoke(
        "java/util/logging/Logger",
        "getLogger",
        "(Ljava/lang/String;)Ljava/util/logging/Logger;",
        &[Value::Object(Some(name))],
    );
}

pub(crate) fn native_thread_start0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // gen r4w3/rooting: `this` crosses the `childValue` upcalls inside
    // `snapshot_inheritable_tl_entries` and the `createInheritedMap` invoke
    // below; both can run a moving GC. Pin it for the whole body and re-read
    // after each GC point.
    let this_pin = ctx.pin_native_root(this);
    // JVMS / `java.lang.Thread`: `start()` on a thread that has already been
    // started must throw `IllegalThreadStateException`. We did not, and the
    // consequence was worse than a missing exception — MEASURED 2026-08-12, a
    // second `start()` RE-RAN the body and spawned a second OS thread for a
    // retired `Runnable`.
    //
    // The real `Thread.start()` branches on `holder.threadStatus`, and that
    // field is INERT on this VM: measured NEW=0 / terminated=0 here against
    // HotSpot's NEW=0 / terminated=2. A guard reading it would never fire.
    // `native_thread_get_state` says the same in its own doc comment and
    // computes from the registry instead, which retains dead entries
    // (`mark_dead` only flips `alive`), so present-but-not-alive is
    // TERMINATED and missing is NEW.
    //
    // The two-reader predicate lives in `thread_already_started` (this file):
    // W7-92's shutdown runner has to ask the identical question before it
    // `start()`s a hook, and two copies of "has this thread already been
    // started" would be a twin pair with nothing keeping them in step. This
    // native is the convergence point of all four registrations and of the
    // container route, including the `--jdk-only` case where the real
    // `start()` bytecode runs; a refusal here does not leak a container
    // registration, because that bytecode's `finally` calls
    // `container.onExit(this)`. See W7-27-thread-exit-java-cleanup.md §13.
    if thread_already_started(ctx, this) {
        ctx.unpin_native_roots(this_pin);
        return Err(RuntimeError::IllegalThreadStateException {
            // HotSpot throws the no-arg constructor here: the message is
            // NULL, not a description. Measured.
            message: String::new(),
        }
        .into());
    }
    // The one-shot `java.util.logging` first-use pre-warm the launcher armed
    // (`arm_logging_prewarm_on_first_thread_start`): run it here, on the
    // starting thread, while the VM is still single-threaded as far as the
    // application is concerned.
    run_armed_logging_prewarm(ctx, this);
    // Round-7 CRIT fix #3: snapshot the parent's `InheritableThreadLocal`
    // entries and queue them against the child's Java Thread identity
    // hash. The child's first `ThreadLocal.get/set/remove` will drain
    // the snapshot into its own TL_MAP (see
    // `drain_inherited_for_current_thread` in phases_early.rs). We do
    // this *before* spawning so there's no race between parent's
    // post-start mutations and the child's drain.
    //
    // G5-1: this is a start0-time capture, and HotSpot captures at
    // CONSTRUCTION time — MEASURED on 25.0.3+9, `ItlProbe` case 1: with
    // `ITL.set(a); new Thread(r); ITL.set(b); start()` the child sees `a`,
    // not `b`. A caller that already took the construction-time snapshot
    // through `capture_inheritable_tl_at_construction` has queued an entry
    // for this child (possibly an EMPTY one, which is the whole point: the
    // parent may have `remove()`d before constructing, and `ItlProbe` case 9
    // measures the child seeing `null` while the parent holds a later value).
    // Re-snapshotting here would overwrite that with the parent's *current*
    // map and reintroduce the divergence, so only capture when nothing was
    // captured at construction.
    //
    // gc-common w5-b: the queue is keyed by the child's ADDRESS, so both the
    // lookup and the queue use `this` as re-read from its pin -- the queue
    // AFTER the `childValue` upcalls, which can move it.
    let this = ctx.read_native_pin(this_pin, this);
    let captured_at_construction = inheritable_tl_captured_at_construction(ctx, this);
    let inherited = if captured_at_construction {
        None
    } else {
        crate::phases_early::snapshot_inheritable_tl_entries(ctx)
    };
    // gen r4w3/rooting: `childValue` upcalls above may have moved `this`.
    let this = ctx.read_native_pin(this_pin, this);
    if let Some(snap) = inherited {
        crate::phases_early::queue_inherited_tl_for_child(ctx, this, snap);
    }
    // TC0622: inherit the parent (creating) thread's context classloader into
    // the child, mirroring real JDK's `Thread.<init>`, which assigns
    // `this.contextClassLoader = parent.getContextClassLoader()`. CratonVM's
    // construction path does not propagate it: the synthetic `<init>` overrides
    // (synthetic-JDK mode) only set name/priority/target/group, and the real
    // `Thread.<init>` bytecode (real-JDK mode) leaves the child's field null.
    // A null `contextClassLoader` makes `Thread.getContextClassLoader()` fall
    // back to the app loader, so Tomcat's
    // `WebappClassLoaderBase.clearReferencesThreads` вЂ” which only stops a thread
    // when `thread.getContextClassLoader() == webappLoader` вЂ” skips leaked
    // app-spawned threads (e.g. `java.util.TimerThread`) and they stay alive.
    // Do this here, on the parent thread, before the child runs, and only when
    // the child has no CCL of its own (preserve an explicit
    // `setContextClassLoader` issued before `start()`). Opt-out:
    // `CRATONVM_INHERIT_THREAD_CCL=0` restores the prior (no-inherit) behavior.
    let inherit_ccl = crate::nbflags().inherit_thread_ccl;
    // Round 15 wave 4 (lane threadvm): `contextClassLoader` and
    // `inheritableThreadLocals` below are the fields `java.lang.Thread`
    // declares (`lang_misc::thread_field` / `set_thread_field`), not a
    // subclass's own of that name.
    if inherit_ccl
        && !matches!(
            crate::lang_misc::thread_field(ctx, this, "contextClassLoader"),
            Value::Object(Some(_))
        )
    {
        let parent = ctx.current_thread_object();
        if let Value::Object(Some(parent_ccl)) =
            crate::lang_misc::thread_field(ctx, parent, "contextClassLoader")
        {
            crate::lang_misc::set_thread_field(
                ctx,
                this,
                "contextClassLoader",
                Value::Object(Some(parent_ccl)),
            );
        }
    }
    // test-context-round2 (ORIGINAL NOTE, CORRECTED BELOW — G5-1, 2026-08-16):
    // "real JDK `Thread.<init>`'s InheritableThreadLocal copy silently doesn't
    // take effect for a still-unexplained interpreter reason specifically when
    // BOTH the ThreadGroup and name constructor arguments are explicitly
    // non-null at the same time … `Thread(ThreadGroup, Runnable, String[,
    // long])` — exactly what `Executors.defaultThreadFactory()` uses for every
    // pooled worker — does not … root-causing it further needs
    // interpreter-level bytecode tracing, out of scope here."
    //
    // That mechanism is impossible, SOURCE-VERIFIED against the oracle's own
    // bytecode (`javap -p -c java.lang.Thread`, Temurin 25.0.3+9):
    //
    //   * ALL EIGHT public constructors are three-to-five instruction
    //     forwarders. Every one of them ends in the SAME
    //     `invokespecial Thread.<init>:(Ljava/lang/ThreadGroup;
    //     Ljava/lang/String;ILjava/lang/Runnable;J)V` — the package-private
    //     master constructor. There is no overload that skips it, and none
    //     that reaches a different copy of the code.
    //   * In that master constructor the ITL block is pc 164..204, and it
    //     reads exactly three things: `attaching` (`currentThread() == this`),
    //     `characteristics & 4` (the NO_INHERIT_THREAD_LOCALS bit that
    //     `Thread(g,r,n,ss,false)` sets), and `parent.inheritableThreadLocals`
    //     (null- and size-checked at pc 182/189). `group` (local 1) and `name`
    //     (local 2) are NOT read anywhere between pc 164 and the `putfield` at
    //     201. The bytecode cannot branch on them.
    //
    // The real reason the copy is inert on this VM is one line away:
    // `ThreadLocal`/`InheritableThreadLocal` `get`/`set`/`remove`/`<init>` are
    // registered with `NativeKind::Intrinsic`
    // (`phases_early::register_thread_local_natives`), and §1.4 admits an
    // `Intrinsic` over real bytecode even under `--jdk-only`
    // (`resolve_native_dispatch_wave1`, vm/src/vm/vm_exec.rs). Their store is
    // the Rust-side `TL_MAP`, so NOTHING in this process ever writes
    // `Thread.inheritableThreadLocals` — for any constructor overload. The
    // master constructor's `ifnull` at pc 182 therefore always takes the skip
    // branch, and the block below, which asks the same question from the
    // native side, is DEAD for the same reason: `get_field_by_name(parent,
    // "inheritableThreadLocals")` cannot be `Object(Some(_))`. It is retained
    // (gated) rather than deleted as a defence-in-depth fallback, NOT because
    // demoting the Intrinsics would ever make it live: `G36-2` §1.1 measured
    // that plan incoherent — a `Bridge` preempts real bytecode too
    // (`G34-1` §0), so demoting `ThreadLocal`'s natives would only move rows
    // between censuses, not change which body answers. The real divergence
    // this comment used to point at is closed a different way: the nine
    // `Thread.<init>` bridges in `lib.rs` capture inheritable values at
    // CONSTRUCTION time via `capture_inheritable_tl_at_construction`
    // (`G23-1`/`1eb5f8346`), and `G36-2` §2 measured
    // `Thread.inheritableThreadLocals` empty on both this VM and HotSpot's own
    // reflection view — so this block staying dead is the correct, matching
    // state, not a residual defect.
    //
    // Known trade-off: `characteristics` (which flags an explicit
    // `Thread(group, target, name, stackSize, false)` opt-out of
    // inheritance) isn't retained anywhere observable post-construction, so
    // this can't distinguish "buggy" from "intentionally opted out". A
    // legitimate opt-out combined with non-null group+name would incorrectly
    // regain inheritance. That combination is rare in practice (the 5-arg
    // opt-out constructor itself is rarely used); documented pending a real
    // interpreter-level fix. Opt-out: `CRATONVM_INHERIT_TL_WORKAROUND=0`.
    //
    // SBR round 2 (lane `thread`): NEVER under `--jdk-only`. There the
    // `ThreadLocal` Intrinsics are not registered and the nine constructors are
    // retired, so `Thread.inheritableThreadLocals` is REAL: the JDK master
    // constructor has already decided, from `characteristics`, whether this
    // child inherits (`NO_INHERIT_THREAD_LOCALS` leaves the field null on
    // purpose). This block would then read the parent's real map and hand it to
    // a child that opted out (`Thread(g, r, n, 0, false)`, and every virtual
    // thread built with `inheritInheritableThreadLocals(false)`), and would
    // re-capture at START time a child whose construction-time capture was
    // legitimately empty. Measured: `Sbr2W7Thread` rows `itl.noinherit`,
    // `itl.removed.before.ctor`, `itl.virtual.noinherit`.
    let apply_itl_workaround = crate::nbflags().inherit_tl_workaround && !ctx.is_jdk_only();
    // The buggy path doesn't leave this field as `Object(None)` (the normal
    // "never written" value real bytecode `getfield` would observe) — a raw
    // native heap read here sees `Int(0)` instead, matching CratonVM's
    // zero-fill representation for a slot the constructor's `putfield`
    // (offset 201 in the real master constructor) never actually reached.
    // Treat either as "not yet inherited".
    let child_itl_unset = matches!(
        crate::lang_misc::thread_field(ctx, this, "inheritableThreadLocals"),
        Value::Object(None) | Value::Int(0)
    );
    // G5-1: and never when the construction-time capture already answered for
    // this child — `capture_inheritable_tl_at_construction` (`G23-1`) already
    // does the copy at construction; a start0-time copy on top of it would be
    // exactly the timing divergence this record exists to remove.
    if apply_itl_workaround && child_itl_unset && !captured_at_construction {
        let parent = ctx.current_thread_object();
        if let Value::Object(Some(parent_map)) =
            crate::lang_misc::thread_field(ctx, parent, "inheritableThreadLocals")
        {
            if let Ok(Some(new_map)) = ctx.invoke(
                "java/lang/ThreadLocal",
                "createInheritedMap",
                "(Ljava/lang/ThreadLocal$ThreadLocalMap;)Ljava/lang/ThreadLocal$ThreadLocalMap;",
                &[Value::Object(Some(parent_map))],
            ) {
                // gen r4w3/rooting: `createInheritedMap` may have moved `this`.
                let this = ctx.read_native_pin(this_pin, this);
                crate::lang_misc::set_thread_field(ctx, this, "inheritableThreadLocals", new_map);
            }
        }
    }
    // gen r4w3/rooting: hand `thread_start` the current address.
    let this = ctx.read_native_pin(this_pin, this);
    let started = ctx.thread_start(this);
    ctx.unpin_native_roots(this_pin);
    started
}

pub(crate) fn native_thread_join(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // Round 14 wave 7 (lane trace6; TW6-2): the served `join()` tells the
    // capture of its `InterruptedException` whether the census's `wait` chain
    // is what the JDK body runs for this target (`false`) or not (`true`: a
    // `java.lang.VirtualThread`, see `join_target_runs_join_nanos`). Always
    // set on that return, so a stale hint never reaches this capture.
    let declines = join_target_runs_join_nanos(ctx, this);
    let result = join_until_dead(ctx, this);
    if is_interrupted_join(&result) {
        ctx.set_served_timed_join_hint(served_join_declines(declines));
    }
    result
}

/// Round 14 wave 7 (lane trace6; TW6-2 of `jit-r14-trace5w6-proposals-RETIRED-20260930.md`):
/// does the JDK's `Thread.join(long)` take its `VirtualThread.joinNanos`
/// branch (`this instanceof VirtualThread`, JDK 21+) for `target`, rather than
/// the `synchronized` `wait` loops the stand-in census rebuilds? Only for a
/// `java.lang.VirtualThread` (a `final` class). A "virtual" thread on this VM is
/// a `ThreadBuilders$BoundVirtualThread` (`ContinuationSupport.isSupported0`
/// answers `false`), for which the JDK body runs the platform loops: the
/// census frames are then exactly what `--jdk-only` executes (and what HotSpot
/// prints under `-XX:-VMContinuations`), and are kept.
fn join_target_runs_join_nanos(ctx: &dyn NativeContext, target: ObjectRef) -> bool {
    let name = ctx.class_name_arc_of_id(ctx.class_id_of_object(target));
    name.as_deref() == Some("java/lang/VirtualThread")
}

/// Did a served join end with the `InterruptedException` of an interrupt?
fn is_interrupted_join(result: &MethodCallResult) -> bool {
    matches!(
        result,
        Err(cratonvm_types::error::MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Runtime(
                cratonvm_types::error::RuntimeError::InterruptedException,
            ),
        ))
    )
}

/// The registry id a served join polls ([`NativeContext::thread_registry_id`]),
/// or `None` to poll the mirror itself each slice.
///
/// Interpreter round i5 wave 103, lane L6 (page
/// `i101-L6-main-thread-stays-alive-during-destroy-java-vm`): the launcher's
/// main entry carries two mirrors once `main` returned, the ended `main` and
/// the listed `DestroyJavaVM`, and the id cannot tell them apart:
/// `thread_registry_id_is_alive` answers the entry dead to Java (right for
/// `main`, whose joiners must return), so a join on the `DestroyJavaVM`
/// mirror returned at once where HotSpot's waits. When the id answers dead
/// while the mirror reads alive, poll the mirror. One extra query only when
/// the id already answers dead; a target that ends after this read is still
/// seen dead through the id.
fn join_poll_registry_id(ctx: &dyn NativeContext, this: ObjectRef) -> Option<u64> {
    let id = ctx.thread_registry_id(this)?;
    if !ctx.thread_registry_id_is_alive(id) && ctx.thread_is_alive(this) {
        return None;
    }
    Some(id)
}

/// The interruptible untimed join shared by the served `join()` and
/// `join(long)` (the latter through [`join_with_served_hint`]); leaves no hint.
fn join_until_dead(ctx: &mut dyn NativeContext, mut this: ObjectRef) -> MethodCallResult {
    // Interpreter round i1 wave 24 (L7): HotSpot's `join()` is
    // `while (isAlive()) wait(0)`, so an interrupt ends it with
    // `InterruptedException` (flag cleared) while the target lives, and a dead
    // target returns at once whatever the flag says. `thread_join` blocks in
    // the OS join, which nothing interrupts; wait for the death here the way
    // `native_thread_join_timed` does, then let `thread_join` reap the handle
    // (immediate once the target is dead).
    //
    // Interpreter round i1 wave 50, lane L1: with the target's registry id
    // (`thread_registry_id`) the poll reads no heap, so the joiner stays in
    // ONE blocking region for the whole join and never reads `RUNNABLE` (JDWP
    // `RUNNING`) between two slices, as the sleeps do since wave 49
    // (`interpreter-L1-a-served-timed-join-reads-runnable-between-its-slices-FIXED`).
    let target = join_poll_registry_id(&*ctx, this);
    // Interpreter round i2 wave 64, lane L1 (`CRATONVM_PUBLISHED_ALL_WAIT_KINDS`):
    // the region says what the JDK body it stands for waits in -- a `wait(0)`
    // loop (JVMTI `IN_OBJECT_WAIT`), or `VirtualThread.joinNanos`'s park
    // (`PARKED`) for a `java.lang.VirtualThread` target. WAITING either way.
    let kind = join_region_kind(ctx, this, false);
    let mut in_region = false;
    let mut interrupted = false;
    loop {
        let alive = match target {
            Some(id) => ctx.thread_registry_id_is_alive(id),
            None => ctx.thread_is_alive(this),
        };
        if !alive {
            break;
        }
        if ctx.is_interrupted(true) {
            interrupted = true;
            break;
        }
        if !in_region {
            begin_join_region(ctx, this, kind);
            in_region = true;
        }
        // JIT round 20, lane walkcompat (section 2 of
        // `r17-takeover-signal-residual-eintr-windows-CLOSED-20261012.md`): the slice
        // inside the blocked-wait trampoline, as the sleeps' are, so the
        // helper-window pass can read a joiner unparked
        // (`CRATONVM_XT_HELPER_UNPARKED_READ`; off, the plain sleep).
        ctx.run_blocked_wait(&mut || std::thread::sleep(std::time::Duration::from_millis(2)));
        if target.is_none() {
            this = leave_join_region(ctx, this);
            in_region = false;
        }
    }
    if in_region {
        this = leave_join_region(ctx, this);
    }
    if interrupted {
        return Err(cratonvm_types::error::MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Runtime(
                cratonvm_types::error::RuntimeError::InterruptedException,
            ),
        ));
    }
    ctx.thread_join(this)
}

/// The [`cratonvm_native_api::BlockKind`] of a served join's blocking region
/// (interpreter round i2 wave 64, lane L1, `CRATONVM_PUBLISHED_ALL_WAIT_KINDS`):
/// HotSpot's `Thread.join` is a `wait` loop on the target (JVMTI
/// `IN_OBJECT_WAIT`), except for a `java.lang.VirtualThread` target, whose
/// `joinNanos` parks (`PARKED`; [`join_target_runs_join_nanos`]). `timed`:
/// the `join(ms)` loop, TIMED_WAITING. Without the switch every kind opens
/// the plain WAITING / TIMED_WAITING region it opened before.
fn join_region_kind(
    ctx: &dyn NativeContext,
    target: ObjectRef,
    timed: bool,
) -> cratonvm_native_api::BlockKind {
    use cratonvm_native_api::BlockKind;
    match (join_target_runs_join_nanos(ctx, target), timed) {
        (false, false) => BlockKind::ObjectWait,
        (false, true) => BlockKind::TimedObjectWait,
        (true, false) => BlockKind::Parked,
        (true, true) => BlockKind::TimedParked,
    }
}

/// Open a served join's blocking region (interpreter round i2 wave 68, lane
/// L1, `CRATONVM_STAND_IN_WAIT_OBJECTS`): a platform target's join is the JDK's
/// `synchronized (this) { while (isAlive()) wait(delay); }`, so the region
/// publishes the target `Thread` as the monitor it waits on and JMX names it
/// (`lock=java.lang.Thread`), as HotSpot does. A `java.lang.VirtualThread`
/// target's `joinNanos` parks on something else, so its park region publishes
/// nothing. `target` is current here (the caller refreshes it at every region
/// end).
fn begin_join_region(
    ctx: &mut dyn NativeContext,
    target: ObjectRef,
    kind: cratonvm_native_api::BlockKind,
) {
    use cratonvm_native_api::BlockKind;
    match kind {
        BlockKind::ObjectWait | BlockKind::TimedObjectWait => {
            ctx.begin_blocking_region_as_on(target, kind)
        }
        _ => ctx.begin_blocking_region_as(kind),
    }
}

/// End a served join's blocking region, answering where the target's `Thread`
/// object is now (a collection may have moved it while the joiner was blocked).
fn leave_join_region(ctx: &mut dyn NativeContext, this: ObjectRef) -> ObjectRef {
    let mut blocked_refs = [Value::Object(Some(this))];
    ctx.end_blocking_region_refs(&mut blocked_refs);
    match blocked_refs[0] {
        Value::Object(Some(cur)) => cur,
        _ => this,
    }
}

/// Round 14 wave 6 (lane trace5; TR5-3): the interruptible untimed join for a
/// served `join(long)`, leaving the throwable capture the hint of which
/// HotSpot `wait` loop an interrupt ends it in (`timed`: the `millis > 0`
/// loop). Set only on the `InterruptedException` return, so no hint outlives
/// a join that returned normally.
///
/// Round 14 wave 7 (lane trace6; TW6-2): no hint for a target whose JDK
/// `join(long)` runs `VirtualThread.joinNanos` ([`join_target_runs_join_nanos`]):
/// without one the capture adds no `wait` frames (`join(long)` is no census
/// entry). `CRATONVM_THROWABLE_STANDIN_VIRTUAL_JOIN_DECLINE=0` leaves it.
fn join_with_served_hint(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    timed: bool,
) -> MethodCallResult {
    let declines = join_target_runs_join_nanos(ctx, this);
    let result = join_until_dead(ctx, this);
    if is_interrupted_join(&result) && !served_join_declines(declines) {
        ctx.set_served_timed_join_hint(timed);
    }
    result
}

/// `declines` ([`join_target_runs_join_nanos`]) under its switch; the switch is
/// read only for such a target.
fn served_join_declines(declines: bool) -> bool {
    declines
        && cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_THROWABLE_STANDIN_VIRTUAL_JOIN_DECLINE",
        )
}

pub(crate) fn native_thread_join_timed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // Interpreter round i5 wave 108, lane L6: decoded as `sleep(J)` decodes
    // it (WP4.5: a long popped generically may come back tagged `Double`
    // with its bits intact). Read as 0, a tag-lost `join(50)` on a live
    // thread was `join(0)` -- wait for its death -- and `join(-1)` did not
    // throw.
    let millis = match args.get(1) {
        Some(Value::Long(ms)) => *ms,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Int(i)) => i64::from(*i),
        _ => 0,
    };
    if millis < 0 {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: "timeout value is negative".to_string(),
            }
            .into(),
        );
    }
    if millis == 0 {
        // join(0) means wait forever (same as join()). Round 14 wave 4 (lane
        // trace3): and, as HotSpot's `while (isAlive()) wait(0)`, an interrupt
        // ends it with `InterruptedException`; the OS join below ignored it
        // (`r14w4-trace3-compatible-join-zero-is-not-interruptible`).
        // `CRATONVM_COMPAT_JOIN_ZERO_INTERRUPTIBLE=0` restores the OS join.
        if cratonvm_types::flags::runtime_flag_default_on("CRATONVM_COMPAT_JOIN_ZERO_INTERRUPTIBLE")
        {
            // Round 14 wave 6 (lane trace5; TR5-3): HotSpot's `join(0)` is
            // interrupted in its `wait(0)` loop.
            return join_with_served_hint(ctx, this, false);
        }
        return ctx.thread_join(this);
    }
    // RD.10: timed join вЂ” return after the specified timeout even if the
    // target thread is still alive. Poll isAlive at a small cadence so we
    // don't block beyond the deadline.
    // Wave 24 (L7): `Instant + Duration` panics on overflow, and
    // `join(Long.MAX_VALUE)` is a real spelling of "wait for it"; a deadline
    // past what `Instant` can hold is the untimed join.
    let Some(deadline) =
        std::time::Instant::now().checked_add(std::time::Duration::from_millis(millis as u64))
    else {
        // HotSpot runs `join(Long.MAX_VALUE)` in its timed loop.
        return join_with_served_hint(ctx, this, true);
    };
    if crate::nbflags().dbg_sleep_trace {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        if N.fetch_add(1, Ordering::Relaxed) % 64 == 0 {
            let st = ctx.capture_stack_trace();
            let frames: Vec<String> = st
                .iter()
                .take(10)
                .map(|e| format!("{}.{}:{}", e.class_name, e.method_name, e.line_number))
                .collect();
            eprintln!("[JOIN-TIMED-TRACE millis={millis}] {}", frames.join(" <- "));
        }
    }
    // Round 14 wave 7 (lane trace6; TW6-2): read while `this` is the
    // argument (the loop below may see it moved).
    let declines = join_target_runs_join_nanos(ctx, this);
    // Interpreter round i1 wave 50, lane L1: polled through the target's
    // registry id, which reads no heap, so the joiner stays in ONE timed
    // blocking region for the whole join and never reads `RUNNABLE` (JDWP
    // `RUNNING`) between two slices (`interpreter-L1-a-served-timed-join-reads-runnable-between-its-slices-FIXED`).
    // Without an id (a context that cannot answer one) each slice leaves the
    // region to read `this`, as before.
    let target = join_poll_registry_id(&*ctx, this);
    // Interpreter round i2 wave 64, lane L1: the `wait(delay)` loop it stands
    // for ([`join_region_kind`]).
    let kind = join_region_kind(ctx, this, true);
    let mut in_region = false;
    let mut interrupted = false;
    loop {
        let alive = match target {
            Some(id) => ctx.thread_registry_id_is_alive(id),
            None => ctx.thread_is_alive(this),
        };
        if !alive {
            break;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        // Honour an interrupt that arrived while we were waiting вЂ” throw
        // InterruptedException so caller code behaves like HotSpot.
        if ctx.is_interrupted(true) {
            interrupted = true;
            break;
        }
        let sleep_time = remaining.min(std::time::Duration::from_millis(2));
        // A timed join is TIMED_WAITING to `Thread.getState()`, as in HotSpot
        // (its `join(ms)` is `wait(delay)`); this used the untimed region,
        // which reports WAITING (interpreter round i1 wave 23, lane L7).
        if !in_region {
            begin_join_region(ctx, this, kind);
            in_region = true;
        }
        // JIT round 20, lane walkcompat: in the blocked-wait trampoline, as
        // `join_until_dead`'s slice.
        ctx.run_blocked_wait(&mut || std::thread::sleep(sleep_time));
        if target.is_none() {
            this = leave_join_region(ctx, this);
            in_region = false;
        }
    }
    if in_region {
        leave_join_region(ctx, this);
    }
    if interrupted {
        // Round 14 wave 6 (lane trace5; TR5-3): the timed `wait(delay)`.
        // Wave 7 (lane trace6; TW6-2): none for a `joinNanos` target.
        if !served_join_declines(declines) {
            ctx.set_served_timed_join_hint(true);
        }
        return Err(cratonvm_types::error::MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Runtime(
                cratonvm_types::error::RuntimeError::InterruptedException,
            ),
        ));
    }
    Ok(None)
}

/// RD.10: `Thread.join(long millis, int nanos)`.
///
/// Validates nanosecond range and rounds sub-millisecond values up by one ms
/// (matching HotSpot's behaviour), then delegates to `join(long)`.
pub(crate) fn native_thread_join_millis_nanos(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Wave 108 (lane L6): the long decoded as `native_thread_join_timed`'s.
    let millis = match args.get(1) {
        Some(Value::Long(ms)) => *ms,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Int(i)) => i64::from(*i),
        _ => 0,
    };
    let nanos = match args.get(2) {
        Some(Value::Int(n)) => *n,
        _ => 0,
    };
    if millis < 0 {
        return served_two_site_argument_check(ctx, false, "timeout value is negative");
    }
    if !(0..=999_999).contains(&nanos) {
        return served_two_site_argument_check(ctx, true, "nanosecond timeout value out of range");
    }
    let effective_ms = if nanos > 0 {
        millis.saturating_add(1)
    } else {
        millis
    };
    let this = args.first().copied().unwrap_or(Value::Object(None));
    native_thread_join_timed(ctx, &[this, Value::Long(effective_ms)])
}

/// Round 14 wave 7 (lane trace6; TW6-3 of `jit-r14-trace5w6-proposals-RETIRED-20260930.md`):
/// the `IllegalArgumentException` of a served `(long, int)` overload whose JDK
/// body has TWO construction sites, `millis < 0` first and the `nanos` range
/// second (`Thread.join(JI)V`, `Thread.sleep(JI)V`; JDK 17/21/25). The
/// capture's one-site rule refuses such a body, so the native says which
/// check failed (`second_check`) through the same one-shot hint the served
/// joins leave (`set_served_timed_join_hint`): the capture reads that bit as
/// the site ordinal for an `IllegalArgumentException` standing at one of
/// these calls (`stackwalker::served_arg_check_standin_frames`), and as the
/// `wait` loop only for an `InterruptedException`.
fn served_two_site_argument_check(
    ctx: &mut dyn NativeContext,
    second_check: bool,
    message: &str,
) -> MethodCallResult {
    ctx.set_served_timed_join_hint(second_check);
    Err(RuntimeError::IllegalArgumentException {
        message: message.to_string(),
    }
    .into())
}

pub(crate) fn native_thread_interrupt(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // Set the VM-side interrupt atomic (what LockSupport.park / Object.wait /
    // Condition.await poll to wake).
    ctx.thread_interrupt(this);
    // Mirror onto the real `java.lang.Thread.interrupted` boolean FIELD. In
    // real-JDK mode `Thread.isInterrupted()` and the static `Thread.interrupted()`
    // run real bytecode that reads this field (not the VM atomic) вЂ” and AQS's
    // ConditionObject.checkInterruptWhileWaiting calls the static
    // `Thread.interrupted()`. Without mirroring, a thread woken from
    // LockSupport.park by an interrupt sees `interrupted == false`, so the AQS
    // await loop never detects the interrupt and re-parks forever
    // (LinkedBlockingQueue.take inside ThreadPoolExecutor.getTask в†’ shutdownNow
    // can't stop the worker в†’ leaked non-daemon thread hangs the VM). The
    // clear side is handled by `clearInterruptEvent` (which static
    // `Thread.interrupted()` calls right after clearing the field). A synthetic
    // Thread without this field resolves to a no-op set.
    //
    // SBR wave 6 (lane `thread`): the field write is `thread_interrupt`'s now,
    // made BEFORE the flag is raised. Writing it again here, after the flag,
    // raced with a target that had already woken and consumed the interrupt:
    // the late write left the field true after `InterruptedException`.
    Ok(None)
}

pub(crate) fn native_thread_get_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(Some(ctx.create_string("main"))))),
    };
    // Round 15 wave 4 (lane threadvm;
    // `r15w3-trace4-thread-name-tid-by-name-FIXED-20260930.md`): `Thread`'s own
    // `name` (`lang_misc::thread_field`), not a subclass's `String name`
    // field. Raw slot 0 is `name` in the SYNTHETIC layout only (on the real
    // one it is `eetop`), under the same switch as `setName`'s raw write.
    match crate::lang_misc::thread_field(ctx, this, "name") {
        Value::Object(Some(str_ref)) => Ok(Some(Value::Object(Some(str_ref)))),
        _ => {
            let num_fields = ctx.object_num_fields(this);
            let raw_slot_ok = crate::is_synthetic_thread_layout(num_fields)
                || !crate::thread_raw_slots_synthetic_only();
            match (num_fields > 0 && raw_slot_ok).then(|| ctx.get_field(this, 0)) {
                Some(Value::Object(Some(str_ref))) => Ok(Some(Value::Object(Some(str_ref)))),
                _ => {
                    let name = ctx.create_string("main");
                    Ok(Some(Value::Object(Some(name))))
                }
            }
        }
    }
}

pub(crate) fn native_thread_is_interrupted(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = this, args[1] = boolean clearInterrupted
    let clear = matches!(args.get(1), Some(Value::Int(1)));
    let interrupted = if ctx.is_interrupted(clear) { 1 } else { 0 };
    Ok(Some(Value::Int(interrupted)))
}

// ---------------------------------------------------------------------------
// Step 4: System properties + utilities
// ---------------------------------------------------------------------------

pub(crate) fn native_system_get_property(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let key_obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = crate::property_key_from_java_string(ctx, key_obj);
    if crate::sysprops_merge::jdk_bodies_own_system_props(&*ctx)
        && crate::properties_sidetable::removed_through_receiver(ctx, &key)
    {
        return Ok(Some(Value::Object(None)));
    }
    match ctx
        .get_system_property(&key)
        .or_else(|| crate::system_property_fallback(ctx, &key))
    {
        Some(val) => {
            let result = ctx.create_string(&val);
            Ok(Some(Value::Object(Some(result))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

pub(crate) fn native_system_get_property_default(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let default = args.get(1).cloned().unwrap_or(Value::Object(None));
    let key_obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = crate::property_key_from_java_string(ctx, key_obj);
    if crate::sysprops_merge::jdk_bodies_own_system_props(&*ctx)
        && crate::properties_sidetable::removed_through_receiver(ctx, &key)
    {
        return Ok(Some(default));
    }
    match ctx
        .get_system_property(&key)
        .or_else(|| crate::system_property_fallback(ctx, &key))
    {
        Some(val) => {
            let result = ctx.create_string(&val);
            Ok(Some(Value::Object(Some(result))))
        }
        None => Ok(Some(default)),
    }
}

pub(crate) fn native_system_set_property(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let key_obj = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let val_obj = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = ctx.read_string(key_obj).unwrap_or_default();
    let value = ctx.read_string(val_obj).unwrap_or_default();
    match ctx.set_system_property(&key, &value) {
        Some(old) => {
            let result = ctx.create_string(&old);
            Ok(Some(Value::Object(Some(result))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

/// Fixed offset added to every reading of this file's monotonic clocks —
/// `System.nanoTime` and `jdk.internal.perf.Perf.highResCounter` — so that
/// neither clock's origin is zero.
///
/// `Instant` has no fixed epoch, so each clock's origin is its own first
/// call rather than process start — which means the FIRST reading measures
/// the gap between `Instant::now()` and the very next statement. On Windows
/// that gap is under the clock's resolution, so the first call returned
/// exactly 0 or 100 ns, roughly half and half.
///
/// A zero-valued reading is not merely ugly, it is wrong in a way Java code
/// notices, because the sentinel idiom
///
/// ```java
/// long started = 0;
/// ...
/// if (started == 0) started = System.nanoTime();
/// ```
///
/// is everywhere in the JDK and its dependents. A clock that can answer 0
/// leaves such a timer permanently disarmed, re-latching on every pass and
/// never measuring anything. HotSpot's origins are arbitrary but in practice
/// large, so the idiom is sound there and callers are entitled to assume it.
///
/// 2^40 ns is about 18 minutes: comfortably past any first-call jitter, and
/// nowhere near `i64` overflow (which is ~292 years of nanoseconds), so
/// arithmetic on deltas and deadlines is unaffected. Neither clock promises
/// more than monotonicity and meaningful differences, both of which a
/// constant offset preserves.
///
/// The two clocks share this constant but NOT their `START`, so they remain
/// independent clocks with independent origins. Nothing here makes them
/// comparable to each other, and no caller may assume it.
const MONOTONIC_ORIGIN_NANOS: i64 = 1 << 40;

pub(crate) fn native_system_nano_time(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Long(java_time_nanos())))
}

/// `System.nanoTime`'s clock, HotSpot's `os::javaTimeNanos`: the elapsed nanos
/// since the first call, biased by a fixed non-zero origin. Rust's Instant
/// doesn't have a fixed epoch, but nano deltas work — and the bias keeps the
/// first reading positive, which the zero-sentinel idiom depends on. See
/// `MONOTONIC_ORIGIN_NANOS`. JVMTI `GetTime` reads it too, as HotSpot's does,
/// so an agent's timestamps and the program's compare.
pub fn java_time_nanos() -> i64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    MONOTONIC_ORIGIN_NANOS + start.elapsed().as_nanos() as i64
}

pub(crate) fn native_system_exit(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let code = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let trace = ctx.capture_stack_trace();

    // CRATONVM_DBG_EXIT=1 вЂ” capture and log the Java caller chain BEFORE we
    // either soft-return or terminate. Helps identify which class/method in
    // the upstream code invoked System.exit. Env-gated so default output is
    // unchanged.
    if crate::nbflags().dbg_exit {
        let mut rendered = String::new();
        for (i, entry) in trace.iter().take(20).enumerate() {
            use std::fmt::Write as _;
            let _ = write!(
                rendered,
                "\n  #{i} {cls}.{m} (bci={bci})",
                cls = entry.class_name,
                m = entry.method_name,
                bci = entry.byte_code_index,
            );
        }
        tracing::warn!(
            target: "cratonvm::system_exit",
            "[CRATONVM_DBG_EXIT] System.exit({code}) caller chain:{rendered}"
        );
    }

    // SportMe/Surefire bootstrap guard:
    // during early fork setup we can reach ForkedBooter.exit(1) before any
    // tests execute. Soft-return this specific callsite so boot can continue.
    if code == 1
        && trace
            .first()
            .map(|f| {
                f.class_name.as_ref() == "org/apache/maven/surefire/booter/ForkedBooter"
                    && f.method_name.as_ref() == "exit"
            })
            .unwrap_or(false)
    {
        tracing::warn!(
            target: "cratonvm::system_exit",
            "[cratonvm] Soft-returning ForkedBooter.exit(1) guard"
        );
        return Ok(None);
    }

    // CRATONVM_SOFT_EXIT=1 вЂ” opt-in. Convert ANY System.exit(I)V into a soft
    // return so the calling Java frame keeps executing (and `main` can reach
    // further). Used to expose downstream failures hidden behind an explicit
    // upstream exit. Default behaviour (env unset) is unchanged: terminate.
    if crate::nbflags().soft_exit {
        tracing::warn!(
            target: "cratonvm::system_exit",
            "[cratonvm] System.exit({code}) soft-returned (CRATONVM_SOFT_EXIT=1)"
        );
        return Ok(None);
    }

    // B6: Surface System.exit calls вЂ” Kotlin/Scala programs often reach exit
    // via an uncaught-exception handler after some earlier failure that would
    // otherwise be invisible. Log to stderr directly since tracing may not be
    // flushed before process::exit.
    claim_exit_or_block_forever(ctx);
    // nio21 wave 8 lane L4: HotSpot prints nothing on `System.exit`, so the
    // line is `CRATONVM_DBG=exit-census` (or `CRATONVM_DBG_EXIT=1`, whose
    // caller-chain dump it introduces) only.
    if exit_census_on() || cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_EXIT").is_some() {
        eprintln!("[cratonvm] System.exit({code}) called вЂ” process terminating");
    }
    // W7-92: hooks first, and BEFORE the slot-map sweep — a hook is Java code
    // that can load classes and allocate, so sweeping first would census a
    // heap the hooks are about to change. `System.exit` from a non-main thread
    // reaches this same native on that thread's ctx, so this one line covers
    // two of the five exit paths (measured on HotSpot: rc 3 and rc 4, hooks
    // ran on both).
    run_shutdown_hooks(ctx, "System.exit");
    sweep_declared_slot_maps_before_exit(&*ctx, "System.exit");
    invoke_pre_exit_hook_from(ctx, code);
    // LAST, and after the pre-exit hook, because that hook's own job is to
    // dump diagnostics. `std::process::exit` runs no destructors, so anything
    // still sitting in the fd table's stdout/stderr buffers dies here. `code`
    // is untouched: this call cannot fail out and cannot change it.
    flush_console_streams(&*ctx);
    cratonvm_native_api::process_exit::exit_process(code);
}

// ---------------------------------------------------------------------------
// Phase 13 Step 4: Runtime + System.lineSeparator
// ---------------------------------------------------------------------------

/// `System.lineSeparator()`.
///
/// The JDK's whole body is `return lineSeparator;` -- a private static that
/// `initPhase1` captures ONCE from the `line.separator` property. A later
/// `System.setProperty("line.separator", ...)`, or a write straight into the
/// `System.props` object, therefore does NOT change what this method answers
/// on HotSpot, and the returned String is the same object every time.
///
/// Both of this VM's registrations disagreed with that, in opposite
/// directions, and the one that won was the wrong one. MEASURED on
/// `apps/probes/SysPropsVmConsumers.java` against HotSpot 25.0.4+7:
///
/// ```text
///                                                  HotSpot   before   after
/// API write to line.separator then lineSeparator   unchanged changed  unchanged
/// OBJECT write to line.separator then lineSeparator unchanged changed unchanged
/// after both writes line.separator restored        true      false*   true
///                                              (* --jdk-only only)
/// ```
///
/// The `--jdk-only` row is the one worth reading twice: an OBJECT write
/// reached the separator `println` uses while `lineSeparator()` went on
/// reporting the old value, so the two internal consumers of one property
/// disagreed and the restore could not put either back. That is the
/// two-authority split the retired `the-system-props-static-field-is-null`
/// write-up's section 12 predicted -- and its prescription (make the Rust
/// store a cache of the `Properties` object) is not what fixes it. The JDK
/// does not read a live store here at all; it reads a value captured at boot.
///
/// So: the static first, which is what `native_system_init_phase1` step 3
/// already sets and what real bytecode would read. The live store is the
/// fallback for the window before that step runs, and the platform default the
/// fallback for an image whose `java/lang/System` declares no such field
/// (synthetic-JDK mode).
pub(crate) fn native_system_line_separator(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // The captured static, returned AS the same object HotSpot hands back.
    if let Some(cid) = ctx.class_id_by_name("java/lang/System") {
        if let Some(idx) = ctx.static_field_index_by_name(cid, "lineSeparator") {
            if let Value::Object(Some(s)) = ctx.get_static_field(cid, idx) {
                return Ok(Some(Value::Object(Some(s))));
            }
        }
    }
    let sep = ctx
        .get_system_property("line.separator")
        .unwrap_or_else(|| if cfg!(windows) { "\r\n" } else { "\n" }.to_string());
    let s = ctx.create_string(&sep);
    Ok(Some(Value::Object(Some(s))))
}

pub(crate) fn register_runtime_natives(registry: &mut NativeMethodRegistry) {
    // gci i12/td: the JDK 25 thread dumper's per-thread snapshot (a genuine
    // `native` declaration, so a Bridge serves it in both modes; there is no
    // bytecode under it). Registered here because this registrar runs on the
    // real-JDK boot path (`register_essential_natives_with_shims`).
    registry.register_with_kind(
        "jdk/internal/vm/ThreadSnapshot",
        "create",
        "(Ljava/lang/Thread;)Ljdk/internal/vm/ThreadSnapshot;",
        native_thread_snapshot_create,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Runtime",
        "getRuntime",
        "()Ljava/lang/Runtime;",
        native_runtime_get_runtime,
    );
    registry.register_with_kind(
        "java/lang/Runtime",
        "availableProcessors",
        "()I",
        native_runtime_available_processors,
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        "java/lang/Runtime",
        "maxMemory",
        "()J",
        native_runtime_max_memory,
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        "java/lang/Runtime",
        "totalMemory",
        "()J",
        native_runtime_total_memory,
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        "java/lang/Runtime",
        "freeMemory",
        "()J",
        native_runtime_free_memory,
        NativeKind::Bridge,
    );
    // JDK 9+ / WildFly: `Runtime.version()` and `Runtime.Version.feature()`.
    registry.register(
        "java/lang/Runtime",
        "version",
        "()Ljava/lang/Runtime$Version;",
        native_runtime_version,
    );
    registry.register(
        "java/lang/Runtime$Version",
        "feature",
        "()I",
        native_runtime_version_feature,
    );
    registry.register(
        "java/lang/Runtime$Version",
        "build",
        "()Ljava/util/Optional;",
        native_runtime_version_build,
    );
    registry.register(
        "java/lang/Runtime",
        "addShutdownHook",
        "(Ljava/lang/Thread;)V",
        |ctx, args| {
            // args[0] = the `Runtime` receiver, args[1] = the hook `Thread`.
            //
            // The null arm is DELIBERATELY not decided here. It used to be
            // (`if let Some(Value::Object(Some(hook)))`, else fall through to
            // `Ok(None)`), which accepted `addShutdownHook(null)` and dropped
            // it — and HotSpot throws NPE there. Worse, deciding it here would
            // put the null check ahead of the shutdown-in-progress check, and
            // the JDK's bytecode orders them the other way round (see
            // `shutdown_hook_add`). `Option` is threaded through so ONE
            // function owns the whole refusal ladder in the measured order.
            let Some(hook) = shutdown_hook_argument(args, "addShutdownHook") else {
                return Ok(None);
            };
            shutdown_hook_add(ctx, hook)?;
            Ok(None)
        },
    );
    registry.register(
        "java/lang/Runtime",
        "removeShutdownHook",
        "(Ljava/lang/Thread;)Z",
        |ctx, args| {
            let Some(hook) = shutdown_hook_argument(args, "removeShutdownHook") else {
                return Ok(Some(Value::Int(0)));
            };
            let removed = shutdown_hook_remove(ctx, hook)?;
            Ok(Some(Value::Int(i32::from(removed))))
        },
    );
    registry.register_with_kind(
        "java/lang/Runtime",
        "gc",
        "()V",
        |ctx, _args| {
            // zs s7/jf: on ZGC `jdk.SystemGC` is this call's, as HotSpot's
            // `JVM_GC` commits it (calling thread, its stack, the call's span).
            crate::jfr_vm_more::force_gc_as_system_gc(ctx);
            Ok(None)
        },
        NativeKind::Bridge,
    );
    registry.register("java/lang/Runtime", "exit", "(I)V", native_runtime_exit);

    // `Runtime.halt(int)` is NOT intercepted — its real bytecode runs, and it
    // calls two `java.lang.Shutdown` natives in order:
    //
    //     Shutdown.beforeHalt();   // notify JFR/agents; nothing observable
    //     Shutdown.halt(status);   // -> synchronized { halt0(status); }
    //
    // Neither was registered, so the FIRST threw
    // `UnsatisfiedLinkError: java/lang/Shutdown.beforeHalt()V` and the process
    // kept running: a caller asking to die immediately got a linkage error out
    // of a method that cannot legally return.
    registry.register_with_kind(
        "java/lang/Shutdown",
        "beforeHalt",
        "()V",
        |_ctx, _args| {
            // HotSpot's does nothing an application can observe.
            Ok(None)
        },
        NativeKind::Bridge,
    );

    // W7-92: `java.lang.Shutdown.runHooks()` is `private static void` WITH a
    // `Code` attribute in the real JDK, so this is an interception of the same
    // kind as the two `exit` natives — and it is the right one while the hooks
    // live Rust-side, because the JDK's own `Shutdown.hooks` array is empty:
    // `Runtime.addShutdownHook` is intercepted here too, so
    // `ApplicationShutdownHooks.hooks` is never populated and the JDK's slot 1
    // hook is never installed. Running the real body would run nothing.
    //
    // This registration is what lets any JDK-side route into shutdown
    // (`Shutdown.exit`, a future signal handler, an agent) land on the same
    // drain as the launcher and the `exit` natives. It is NOT how the launcher
    // reaches the hooks — `vm-cli` calls `run_shutdown_hooks` directly, because
    // a `vm.invoke` of this triple would depend on `java/lang/Shutdown`
    // resolving, which is a real-JDK-mode assumption.
    registry.register_with_kind(
        "java/lang/Shutdown",
        "runHooks",
        "()V",
        |ctx, _args| {
            run_shutdown_hooks(ctx, "Shutdown.runHooks");
            Ok(None)
        },
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        "java/lang/Shutdown",
        "halt0",
        "(I)V",
        native_shutdown_halt0,
        NativeKind::Bridge,
    );

    // Runtime.loadLibrary(String) / Runtime.load(String) вЂ” JNI library loading
    //
    // JDK-ONLY-WAVE2 (loader-scoped `loadedLibraryNames`): these four triples
    // are registered TWICE over, once per compatibility mode, and each triple
    // exactly once per registry — not re-registered on top of itself, which
    // would leave a self-shadow row in the duplicate-registration census for a
    // change that shadows nothing.
    //
    // The mode is readable here because `vm_init.rs` calls
    // `set_compatibility_mode` BEFORE the `register_*` population pass, and
    // says so structurally (the call sits above the `#[cfg(feature =
    // "synthetic-jdk")]` fork so there is no reachable point between `new()`
    // and the first `register_*` where the mode is unset). Registration is also
    // where this campaign's own precedent puts a mode decision — §1.4's lever
    // is registration, not dispatch. A native BODY cannot ask: `NativeCallback`
    // is a bare `fn` pointer, so it captures nothing, and `NativeContext`
    // exposes no policy accessor (deliberately — `CompatibilityMode` is a
    // per-registry FIELD, not a process global).
    //
    // The `else` arm keeps `LoaderScoping::Off`, which makes every success arm
    // of `load_library_or_throw` return the `Ok(None)` it returns today — the
    // cross-loader `loadedLibraryNames` rule stays strict-only. What it no
    // longer keeps is the `args.get(1)` read on the two `Runtime` sites: since
    // 2026-08-12 both arms decode the argument vector through
    // `runtime_load_args`. See that function for the measurement.
    if registry.compatibility_mode().is_jdk_only() {
        registry.register(
            "java/lang/Runtime",
            "loadLibrary0",
            "(Ljava/lang/Class;Ljava/lang/String;)V",
            |ctx, args| {
                let (from_class, name) = runtime_load_args(&*ctx, args);
                crate::security_manager::check_host_native_access_or_throw(ctx, &name)?;
                // Map bare library name to platform-specific filename.
                // resolve_library_path() in NativeContextImpl will search java.library.path.
                load_library_or_throw(
                    ctx,
                    &name,
                    LibrarySpelling::BareName,
                    from_class,
                    LoaderScoping::On,
                )
            },
        );
        registry.register(
            "java/lang/Runtime",
            "load0",
            "(Ljava/lang/Class;Ljava/lang/String;)V",
            |ctx, args| {
                let (from_class, path) = runtime_load_args(&*ctx, args);
                crate::security_manager::check_host_native_access_or_throw(ctx, &path)?;
                load_library_or_throw(
                    ctx,
                    &path,
                    LibrarySpelling::AbsolutePath,
                    from_class,
                    LoaderScoping::On,
                )
            },
        );
        // System.loadLibrary / System.load вЂ” delegate to the same machinery.
        // Both are STATIC and `@CallerSensitive`: the real bytecode would pass
        // `Reflection.getCallerClass()` down to `Runtime.load*0`, but the
        // interception is above that, so there is no `fromClass` argument here
        // and the caller is recovered from the frame stack instead.
        registry.register(
            "java/lang/System",
            "loadLibrary",
            "(Ljava/lang/String;)V",
            |ctx, args| {
                let name_obj = obj_arg(args, 0)?;
                let name = ctx.read_string(name_obj).unwrap_or_default();
                crate::security_manager::check_host_native_access_or_throw(ctx, &name)?;
                load_library_or_throw(
                    ctx,
                    &name,
                    LibrarySpelling::BareName,
                    None,
                    LoaderScoping::On,
                )
            },
        );
        registry.register(
            "java/lang/System",
            "load",
            "(Ljava/lang/String;)V",
            |ctx, args| {
                let path_obj = obj_arg(args, 0)?;
                let path = ctx.read_string(path_obj).unwrap_or_default();
                crate::security_manager::check_host_native_access_or_throw(ctx, &path)?;
                load_library_or_throw(
                    ctx,
                    &path,
                    LibrarySpelling::AbsolutePath,
                    None,
                    LoaderScoping::On,
                )
            },
        );
    } else {
        registry.register(
            "java/lang/Runtime",
            "loadLibrary0",
            "(Ljava/lang/Class;Ljava/lang/String;)V",
            |ctx, args| {
                // `args[2]`, not `args[1]` — instance method, so `args[0]` is
                // the `Runtime` receiver and `args[1]` the `fromClass` mirror.
                // `_from_class` is discarded rather than threaded because
                // `LoaderScoping::Off` makes `loaded_by` early-return before it
                // is read; keeping the discard makes the mode difference one
                // axis wide.
                let (_from_class, name) = runtime_load_args(&*ctx, args);
                crate::security_manager::check_host_native_access_or_throw(ctx, &name)?;
                // Map bare library name to platform-specific filename.
                // resolve_library_path() in NativeContextImpl will search java.library.path.
                load_library_or_throw(
                    ctx,
                    &name,
                    LibrarySpelling::BareName,
                    None,
                    LoaderScoping::Off,
                )
            },
        );
        registry.register(
            "java/lang/Runtime",
            "load0",
            "(Ljava/lang/Class;Ljava/lang/String;)V",
            |ctx, args| {
                // `args[2]` — see the `loadLibrary0` note directly above.
                let (_from_class, path) = runtime_load_args(&*ctx, args);
                crate::security_manager::check_host_native_access_or_throw(ctx, &path)?;
                load_library_or_throw(
                    ctx,
                    &path,
                    LibrarySpelling::AbsolutePath,
                    None,
                    LoaderScoping::Off,
                )
            },
        );

        // System.loadLibrary / System.load вЂ” delegate to the same machinery
        registry.register(
            "java/lang/System",
            "loadLibrary",
            "(Ljava/lang/String;)V",
            |ctx, args| {
                let name_obj = obj_arg(args, 0)?;
                let name = ctx.read_string(name_obj).unwrap_or_default();
                crate::security_manager::check_host_native_access_or_throw(ctx, &name)?;
                load_library_or_throw(
                    ctx,
                    &name,
                    LibrarySpelling::BareName,
                    None,
                    LoaderScoping::Off,
                )
            },
        );
        registry.register(
            "java/lang/System",
            "load",
            "(Ljava/lang/String;)V",
            |ctx, args| {
                let path_obj = obj_arg(args, 0)?;
                let path = ctx.read_string(path_obj).unwrap_or_default();
                crate::security_manager::check_host_native_access_or_throw(ctx, &path)?;
                load_library_or_throw(
                    ctx,
                    &path,
                    LibrarySpelling::AbsolutePath,
                    None,
                    LoaderScoping::Off,
                )
            },
        );
    }
}

/// Decode `Runtime.load0(Class,String)` / `Runtime.loadLibrary0(Class,String)`
/// into the JDK's own `(fromClass, name)` pair.
///
/// **Both are INSTANCE methods.** `javap -p java.lang.Runtime` on JDK 25.0.3:
///
/// ```text
///   public void load(java.lang.String);
///   void load0(java.lang.Class<?>, java.lang.String);
///   public void loadLibrary(java.lang.String);
///   void loadLibrary0(java.lang.Class<?>, java.lang.String);
/// ```
///
/// so a native body sees `args[0]` = the `Runtime` receiver, `args[1]` = the
/// `fromClass` mirror `Runtime.loadLibrary(String)` resolved with
/// `Reflection.getCallerClass()`, and `args[2]` = the library name. That
/// convention is stated all over this crate for the same registry — the
/// `Runtime.addShutdownHook` registration forty lines above says "args[0] = the
/// `Runtime` receiver, args[1] = the hook `Thread`", and `vm_exec.rs`'s native
/// argument marshalling describes its inline buffer as "receiver plus a couple
/// of operands".
///
/// The pre-existing bodies read `args.get(1)` as the NAME. That is the
/// `fromClass` mirror, and `read_string` of a non-`String` object is `None`
/// (`vm_object.rs::read_string_non_string_object`), so the name arrives empty
/// and `Runtime.getRuntime().loadLibrary(x)` fails for every `x` with
/// `no  in java.library.path` — a wrong answer with a right *shape*, which is
/// why no vector caught it: the regression suite reaches library loading only
/// through `System.load`/`System.loadLibrary`
/// (`RJdkJni.java:189-217`, `RJdkFailure.java:257`), never through `Runtime`.
///
/// **The index was not deduced, it was measured** — 2026-08-12, one binary at
/// dev `87809196b`, HotSpot 25.0.3 beside it, `Runtime.getRuntime()
/// .loadLibrary(x)` for a name that cannot exist and for `"net"` which does:
///
/// ```text
/// HotSpot          no cratonvm_probe_zzz in java.library.path: ...   /  net LOADED
/// --jdk-only       no cratonvm_probe_zzz in java.library.path       /  net LOADED
/// --real-jdk       no  in java.library.path                         /  net "no  in java.library.path"
/// ```
///
/// The strict arm reads `args[2]` and the name arrives; the `Compatible` arm
/// read `args[1]` and the name arrived EMPTY, with the double space that is the
/// signature of the bug, for every argument including one HotSpot loads. That
/// is the discriminator: a probe asserting only "it threw" passes against
/// either index, because the wrong index throws too.
///
/// Corrected on the strict arm 2026-08-11 and on the `Compatible` arm
/// 2026-08-12 — a `loadLibrary` that fails for EVERY argument is a
/// HotSpot-parity bug rather than a compatibility-layer substitution, which is
/// the one thing the `Compatible` freeze admits. Blast radius and the callers
/// that could depend on the always-failing behaviour are in
/// W7-79-loadlibrary-compatible-arm.md.
fn runtime_load_args(ctx: &dyn NativeContext, args: &[Value]) -> (Option<ObjectRef>, String) {
    let from_class = match args.get(1) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let name = match args.get(2) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    (from_class, name)
}

/// Which of the two JDK spellings the caller used. `System.loadLibrary("zip")`
/// passes a BARE name that has to be mapped through `System.mapLibraryName`
/// before it can be opened; `System.load("/x/libzip.so")` passes a complete
/// path that must be used verbatim (JLS: "the filename argument must be an
/// absolute path name").
#[derive(Clone, Copy, PartialEq, Eq)]
enum LibrarySpelling {
    BareName,
    AbsolutePath,
}

// ---------------------------------------------------------------------------
// Per-class-loader `loadedLibraryNames`.
//
// THE JDK RULE, from `jdk.internal.loader.NativeLibraries` in JDK 25.0.3's
// `lib/src.zip` (Eclipse Adoptium build; the same file `javap -p
// jdk.internal.loader.NativeLibraries` describes structurally). The class doc
// on `newInstance(ClassLoader)` states it as a numbered restriction:
//
//     3. Restriction on a native library that can only be loaded by one class
//        loader. Each class loader manages its own set of native libraries.
//        The same JNI native library cannot be loaded into more than one class
//        loader.
//
// and `loadLibrary(Class,String,boolean)` enforces it in two steps, in this
// order, under `acquireNativeLibraryLock(name)`:
//
//     NativeLibrary cached = libraries.get(name);       // THIS loader's map
//     if (cached != null) return cached;                //  -> silent success
//     if (loadedLibraryNames.contains(name)) {          // ANY loader's names
//         throw new UnsatisfiedLinkError("Native Library " + name +
//                 " already loaded in another classloader");
//     }
//
// The two structures are not the same shape: `libraries` is an INSTANCE field
// of a `NativeLibraries` that `ClassLoader` holds one of per loader
// (`private final NativeLibraries libraries = NativeLibraries.newInstance(this)`,
// reached by `ClassLoader.nativeLibrariesFor(loader)`, with
// `BootLoader.getNativeLibraries()` standing in for the null loader), while
// `loadedLibraryNames` is a static `Set<String>`. A same-loader repeat is a
// success that returns the SAME library; a cross-loader repeat is an error.
// One process-wide set of names cannot tell those apart, because it answers
// with a string key and no loader identity — which is precisely the
// bind-by-NAME species this repository has now paid for five separate times
// (the `docs/jdk-only/README.md` loader-identity cluster).
//
// WHY THIS HAS TO LIVE HERE AT ALL. On HotSpot the rule is enforced by
// `ClassLoader.loadLibrary` bytecode, which is where W6-6 correctly says the
// `NativeLibraries.load` native must NOT duplicate it. But CratonVM intercepts
// the whole road above that: `System.load`, `System.loadLibrary`,
// `Runtime.load0` and `Runtime.loadLibrary0` are all registered natives, so
// `ClassLoader.loadLibrary` -> `NativeLibraries.loadLibrary` never runs for
// them and the JDK's own bookkeeping is never consulted OR populated. A VM that
// replaces the bytecode that enforced a rule inherits the rule.
//
// SHAPE. One `VmScoped` table, `loader id -> keys`, and both JDK queries are
// answered from it: "does THIS loader hold it" is a row lookup, "does ANY
// loader hold it" is the union over rows. Not two structures, and not a process
// global — jdk-only-mode.md §2 forbids process globals for this feature's
// state, and this repo has already shipped the failure that protects against
// (native caches leaking across two `SharedVm`s in one test process; see
// `cratonvm_native_api::vm_scoped`). Keyed by `vm_identity` and torn down from
// `forget_vm_system_singletons`, like `SYSTEM_ENV`/`SYSTEM_PROPS` below.
//
// No `ObjectRef` is stored — a loader id is an `i32` and a key is a `String` —
// so unlike those two this table needs no GC root scan and no post-collection
// remap. Storing the loader MIRROR would have needed both, and would have
// keyed loader identity on an object whose address moves.
// ---------------------------------------------------------------------------

/// Whether the per-loader rule above is in force at this registration.
///
/// `Off` on every `Compatible` registration: contract §5/§10 requires
/// `Compatible` to stay byte-for-byte, and this rule can only ever turn a
/// success into an `UnsatisfiedLinkError`, which is a behaviour change for
/// every caller written around a `catch (UnsatisfiedLinkError)` fallback.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LoaderScoping {
    Off,
    On,
}

/// `loader id -> the library keys that loader has been told it holds`, one row
/// per VM. `BTreeMap`/`BTreeSet` rather than hash maps because the rows are
/// tiny (a JVM loads a handful of JNI libraries) and a deterministic iteration
/// order makes the "which loader owns it" answer reproducible when a key has
/// somehow been recorded twice.
static LOADED_LIBRARIES: VmScoped<
    std::collections::BTreeMap<i32, std::collections::BTreeSet<String>>,
> = VmScoped::new();

/// Claim `key` for `loader`, or report the loader that already holds it.
///
/// ONE table acquisition, deliberately: `contains` followed by a separate
/// `insert` lets two threads loading the same library concurrently both see
/// "unclaimed" and both claim it, and HotSpot closes exactly that window by
/// running its `libraries.get` / `loadedLibraryNames.contains` / put sequence
/// inside `acquireNativeLibraryLock(name)`.
///
/// `Some(owner)` means the key was already held — by `loader` itself (a
/// same-loader repeat, which the JDK answers with the cached library) or by
/// another loader (the `UnsatisfiedLinkError`). `None` means it is now
/// `loader`'s.
///
/// The closure touches only this map: no allocation, no Java dispatch, no
/// second `VmScoped` lock, per the lock discipline in
/// `cratonvm_native_api::vm_scoped`.
fn claim_library(vm_identity: usize, loader: i32, key: &str) -> Option<i32> {
    LOADED_LIBRARIES.with(vm_identity, |rows| {
        if let Some((owner, _)) = rows.iter().find(|(_, keys)| keys.contains(key)) {
            return Some(*owner);
        }
        rows.entry(loader).or_default().insert(key.to_string());
        None
    })
}

/// Which class loader is asking for this library, as a loader IDENTITY —
/// `NativeContext::loader_id_of_class`'s `0` bootstrap / `1` platform /
/// `2` application / `3+` user-defined — never a loader name. Two loaders can
/// share a name; that is the whole species this record belongs to.
///
/// Two sources, in the JDK's own order of authority:
///
/// 1. `from_class`, when the entry point had one. `Runtime.load0` and
///    `Runtime.loadLibrary0` are handed the caller by `Reflection
///    .getCallerClass()` in real `Runtime` bytecode, and `ClassLoader
///    .loadLibrary(Class,String)` derives the loader from exactly that:
///    `ClassLoader loader = (fromClass == null) ? null : fromClass.getClassLoader()`.
///    Taking the argument means agreeing with the JDK by construction.
///
/// 2. Otherwise the frame stack. `System.load`/`System.loadLibrary` are static
///    and `@CallerSensitive`, and the interception sits above the point where
///    the JDK would have resolved the caller, so the caller has to be recovered
///    the same way `latest_user_defined_loader_class` and
///    `class_for_name_one_arg_caller_loader` do: `frame_class_ids()`, which
///    hands back each live frame's already-resolved `ClassId` innermost first.
///    NOT a name re-resolution of a captured stack trace — that collapses two
///    same-named classes from different loaders onto whichever the global class
///    table saw first, which would answer this exact question with the wrong
///    loader.
///
///    The `java/lang/System` and `java/lang/Runtime` frames skipped here are
///    the intercepted native's own frame and the `Runtime.loadLibrary` ->
///    `loadLibrary0` hop; they are the frames `Reflection.getCallerClass()`
///    skips for the same reason, both methods being `@CallerSensitive`.
///
/// `None` is the honest answer when neither source names a class — a library
/// load with no Java frame under it (an embedder or a JNI `JNI_OnLoad`
/// re-entry). See `loaded_by` for what is done with it, which is deliberately
/// nothing.
fn requesting_loader_id(ctx: &mut dyn NativeContext, from_class: Option<ObjectRef>) -> Option<i32> {
    if let Some(mirror) = from_class {
        if let Some(class_id) = ctx.class_id_from_mirror(mirror) {
            return Some(ctx.loader_id_of_class(class_id));
        }
    }
    for class_id in ctx.frame_class_ids() {
        match ctx.class_name_arc_of_id(class_id).as_deref() {
            Some("java/lang/System") | Some("java/lang/Runtime") => continue,
            Some(_) => return Some(ctx.loader_id_of_class(class_id)),
            None => return None,
        }
    }
    None
}

/// The success return of `load_library_or_throw`, after the per-loader rule
/// has had its say.
///
/// `key` is the library identity. HotSpot's is `file.getCanonicalPath()` for
/// anything not statically linked into libjvm (`NativeLibraries.loadLibrary
/// (Class,File)` sets `name = file.getCanonicalPath()` before handing it to the
/// checks above), so on HotSpot `System.loadLibrary("zip")` and
/// `System.load("<java.home>/bin/zip.dll")` collide. Here the key is the string
/// the caller spelled. That is narrower, and NOT rounded up to a fabricated
/// path: `NativeContext::load_native_library` returns a library-table INDEX,
/// not the path it resolved, so a bare name that was found somewhere on
/// `java.library.path` cannot be canonicalised back to a file without redoing
/// the search — and a key invented by redoing it would not be the file that was
/// actually opened. Named residual in
/// W5-1-loadlibrary-allowlist-too-wide.md; two spellings of one file are
/// two keys here.
fn loaded_by(
    ctx: &mut dyn NativeContext,
    key: &str,
    from_class: Option<ObjectRef>,
    scoping: LoaderScoping,
) -> MethodCallResult {
    if scoping == LoaderScoping::Off {
        return Ok(None);
    }
    // No caller, no claim. Recording this under a stand-in loader id would make
    // the NEXT load — the one from the real owner — throw, manufacturing the
    // very error this models; throwing here would manufacture it immediately.
    // Both are fabrications, and a missed error is the one that leaves a
    // caller's own `catch (UnsatisfiedLinkError)` fallback reachable.
    let Some(loader) = requesting_loader_id(ctx, from_class) else {
        return Ok(None);
    };
    match claim_library(ctx.vm_identity(), loader, key) {
        // Freshly claimed, or a same-loader repeat: the JDK returns the cached
        // `NativeLibrary` for the repeat, which is a plain success here since
        // nothing above consumes the handle on this road.
        None => Ok(None),
        Some(owner) if owner == loader => Ok(None),
        // Message reproduced verbatim from `NativeLibraries.loadLibrary`. The
        // exception carries no other detail, so the text is the whole of what a
        // caller — or a HotSpot-versus-CratonVM transcript diff — can see.
        Some(_) => Err(RuntimeError::UnsatisfiedLinkError {
            message: format!("Native Library {key} already loaded in another classloader"),
        }
        .into()),
    }
}

/// The bare library names whose ENTIRE native surface this VM supplies from
/// Rust, so a caller that asks for them has genuinely got what it asked for
/// even though no shared object was opened.
///
/// These are the java.base-adjacent JNI libraries that ship inside the JDK
/// image. CratonVM never successfully dlopen()s them — they are linked against
/// `libjvm`/`jvm.dll`, which this process does not have, so the real load fails
/// with `ERROR_MOD_NOT_FOUND` (measured on Windows: `LoadLibraryW` on
/// `<java.home>/bin/{java,zip,net,nio,jimage,verify,management,management_ext,
/// instrument,extnet,prefs}.dll` all fail with 126 outside a JVM process) —
/// while every `Java_java_util_zip_*` / `Java_java_net_*` entry point they
/// exist to provide is registered in this process as a Rust native. On HotSpot
/// a *cold* `System.loadLibrary` of them SUCCEEDS, so answering
/// `UnsatisfiedLinkError` for them would be a fresh divergence in the opposite
/// direction. Anything NOT on this list is a library this VM has no
/// implementation of, and the JDK contract for that is an error, not a silent
/// return.
///
/// MEASURED (JDK 25.0.3 Windows x64, `System.loadLibrary` from the app class
/// loader, nothing pre-loaded). Three names that used to be on this list are
/// NOT loadable on HotSpot at all, because the file does not exist on
/// `java.library.path`:
///
///   * `sunec`  — deleted from the JDK image; SunEC's native ECC was replaced
///     by a Java implementation, which is why this crate ships
///     `sunec_intpoly`/`sunec_point` intrinsics instead of a library.
///   * `jvm`    — lives in `<java.home>/bin/server` (`lib/server` on Linux),
///     which is on neither `java.library.path` nor `sun.boot.library.path`.
///   * `jsig`   — no `jsig.dll` in the Windows image. `libjsig.so` DOES exist
///     in `<java.home>/lib` on Linux/macOS, so this one is platform-split
///     rather than deleted (see `PLATFORM_ONLY`).
///
/// `zip` is deliberately absent for a different, DYNAMIC reason. HotSpot's real
/// rule is not "does the file exist" but "has the BOOT loader already loaded
/// it": `NativeLibraries` rejects a second load of the same file from a
/// different class loader with `UnsatisfiedLinkError: Native Library
/// <path> already loaded in another classloader`. Measured, same image:
///
///   cold, directory classpath          loadLibrary("zip") -> LOADS
///   after any java.util.zip native use  ->  THROWS (already loaded)
///   cold, but classpath is a JAR        ->  THROWS (already loaded)
///
/// java.base itself boot-loads it (`ZipUtils.loadLibrary()` ->
/// `BootLoader.loadLibrary("zip")` from `Inflater.<clinit>`), so ANY program
/// that has touched `java.util.zip` — or that was merely launched from a jar —
/// is in the throwing state before its own `loadLibrary("zip")` runs. CratonVM
/// is permanently in the equivalent state: its zip natives are bound in-process
/// from VM init and no shared object is ever opened. `RJdkJni` measures exactly
/// this: `zipNatives()` runs immediately before `libraryLoading()`, so HotSpot's
/// oracle prints `loadedLibrary=net` — `zip` must FAIL and fall through to the
/// `net` probe (`RJdkJni.java:189-202`). This is a test-produced state, not a
/// file-layout fact, so it holds identically on Linux.
///
/// THE DYNAMIC RULE IS NO LONGER UNMODELLED, but this list is still not what
/// models it. Under `LoaderScoping::On` (strict mode) [`loaded_by`] keeps the
/// per-class-loader record HotSpot keeps, so a second load of the same library
/// from a DIFFERENT loader now throws. What that record cannot see is a library
/// the BOOT loader holds, because `jdk/internal/loader/BootLoader.loadLibrary`
/// is registered as a no-op in `native-builtins/src/lib.rs` — the one event
/// that would tell us `java.base` had taken `net`/`nio`/`prefs` for itself
/// never reaches any bookkeeping. See [`record_boot_loader_library`], which is
/// written and deliberately unarmed. `net` therefore stays on this list, and
/// the measured oracle still needs it there: `RJdkJni` never touches
/// `java.net` before line 195.
pub(crate) fn is_vm_provided_jdk_library(name: &str) -> bool {
    // Ships as a real, separately-present shared object in the JDK 25 image on
    // every platform, and cold-loads on HotSpot.
    const EVERY_PLATFORM: &[&str] = &[
        "java",
        "net",
        "nio",
        "jimage",
        "verify",
        "management",
        "management_ext",
        "instrument",
        "extnet",
        "prefs",
        "j2pkcs11",
    ];
    // Present in one platform's image only. `sunmscapi.dll` is the Windows
    // SunMSCAPI crypto provider and has no Unix counterpart; `libjsig.so` is
    // the Unix signal-chaining shim and has no Windows counterpart.
    #[cfg(windows)]
    const PLATFORM_ONLY: &[&str] = &["sunmscapi"];
    #[cfg(not(windows))]
    const PLATFORM_ONLY: &[&str] = &["jsig"];

    EVERY_PLATFORM.contains(&name) || PLATFORM_ONLY.contains(&name)
}

/// Names HotSpot refuses NOT because the file is missing but because the BOOT
/// loader already holds it (`UnsatisfiedLinkError: Native Library <path>
/// already loaded in another classloader`). File presence cannot model that, so
/// they are excluded from [`jdk_image_ships_library`] and keep throwing.
///
/// `zip` is the one the corpus pins: `RJdkJni.zipNatives()` runs immediately
/// before `libraryLoading()`, so by the time the test asks, java.base has
/// boot-loaded it and the HotSpot oracle prints `loadedLibrary=net`. Cold, with
/// a directory classpath, `zip` LOADS on both platforms -- the exclusion is
/// about the state that test creates, not about the image.
const DYNAMIC_ALREADY_LOADED: &[&str] = &["zip"];

/// Does the running JDK image ship `name` as a JNI library, in the directory
/// the JDK loads its own from?
///
/// HotSpot decides this by FILE PRESENCE, not by an allowlist, so a hardcoded
/// list can never keep up with it. MEASURED on the JDK 25.0.3 Linux x64 image,
/// one cold `System.loadLibrary` per name from the app class loader:
///
///   LOADS : awt awt_xawt fontmanager javajpeg lcms jsound splashscreen
///           freetype mlib_image jsig zip net nio management instrument
///   THROWS: sunec, jvm, harfbuzz, and a made-up name -- none of which has a
///           file (`libjvm.so` lives one level down in `lib/server`, which is
///           on neither `java.library.path` nor `sun.boot.library.path`)
///
/// Every LOADS name has a `<java.home>/lib/lib<name>.so`; no THROWS name does.
/// So the presence test reproduces the measured oracle exactly, and it covers
/// the java.desktop libraries too, which [`is_vm_provided_jdk_library`] never
/// listed.
///
/// `awt` is why this exists. Once `load_library_or_throw` began actually
/// raising the `UnsatisfiedLinkError` it used to compute and drop,
/// `java.awt.Toolkit.<clinit>` started dying on it -- and `Toolkit.<clinit>` is
/// reached by merely CONSTRUCTING a `java.awt.event.ActionEvent`, on a VM that
/// otherwise models AWT well enough to answer `HeadlessException` for
/// `new java.awt.Button()` exactly as headless HotSpot does. H2
/// `TestTools.testConsole` went from running for 8 minutes to dying in under a
/// second on that one constructor.
///
/// Only a BARE name reaches here. `System.load("/abs/path/libfoo.so")` names a
/// FILE, and a file that is not there is an error however it is spelled.
fn jdk_image_ships_library(ctx: &dyn NativeContext, name: &str) -> bool {
    if DYNAMIC_ALREADY_LOADED.contains(&name) {
        return false;
    }
    let home = match ctx.get_system_property("java.home") {
        Some(h) if !h.is_empty() => h,
        _ => return false,
    };
    // The JDK image keeps its JNI libraries in `bin` on Windows and `lib`
    // everywhere else; `platform_lib_name` supplies the decorated file name.
    let dir = if cfg!(windows) { "bin" } else { "lib" };
    std::path::Path::new(&home)
        .join(dir)
        .join(platform_lib_name(name))
        .is_file()
}

/// Open a native library, or raise the `UnsatisfiedLinkError` the JDK
/// specifies.
///
/// `System.load`, `System.loadLibrary`, `Runtime.load` and `Runtime.loadLibrary`
/// are all documented to throw `UnsatisfiedLinkError` when "the library does not
/// exist, or the library cannot be mapped". All four bodies used to end in
/// `let _ = ctx.load_native_library(..)` — the error was computed and dropped,
/// so a request for a library that does not exist RETURNED NORMALLY. That is a
/// fabricated success where the spec mandates a failure, and it is worse than a
/// missing feature: a caller like Netty's `NativeLibraryLoader` or Tomcat's
/// `AprLifecycleListener` is written to catch this error and fall back to a pure
/// -Java path, so swallowing it left them believing a native backend was armed
/// and failing much later, far from the cause. Measured:
/// `regression-suite/src/RJdkFailure.java:261` ("loading an absent library must
/// raise UnsatisfiedLinkError") failed in BOTH `--real-jdk` and `--jdk-only`
/// while HotSpot 25 passed.
///
/// The load is still attempted first, so a library that really is on
/// `java.library.path` still loads and `JNI_OnLoad` still runs. Only the failure
/// path changed, and only for names outside [`is_vm_provided_jdk_library`].
///
/// `scoping` selects whether the per-class-loader rule above [`LoaderScoping`]
/// applies. All three SUCCESS arms below route through [`loaded_by`], not
/// through a bare `Ok(None)`: on HotSpot the "already loaded in another
/// classloader" check sits above the load attempt, so it governs a JDK-image
/// library reported loaded from the allowlist exactly as it governs a
/// third-party `.so` this VM really opened. Two web applications with their own
/// loaders probing one `tcnative` is the shape that reaches this first, and it
/// needs no boot-loader bookkeeping to fire.
fn load_library_or_throw(
    ctx: &mut dyn NativeContext,
    requested: &str,
    spelling: LibrarySpelling,
    from_class: Option<ObjectRef>,
    scoping: LoaderScoping,
) -> MethodCallResult {
    // The two PRE-CHECKS `Runtime.load0`/`loadLibrary0` run before they look
    // for anything, and which decide the message a caller sees:
    //
    //   System.load("relative.so")
    //     HotSpot  UnsatisfiedLinkError: Expecting an absolute path of the
    //              library: relative.so
    //     was      UnsatisfiedLinkError: no relative.so in java.library.path
    //   System.loadLibrary("a/b")
    //     HotSpot  UnsatisfiedLinkError: Directory separator should not appear
    //              in library name: a/b
    //     was      UnsatisfiedLinkError: no a/b in java.library.path
    //
    // Both were reported as a missing library, which sends a reader looking for
    // a file rather than at the spelling of the argument. `LibrarySpelling` was
    // already the right distinction -- it just was not asked this question.
    // MEASURED by `apps/probes/SystemRuntimeObjectSweep.java`.
    match spelling {
        LibrarySpelling::AbsolutePath => {
            if !std::path::Path::new(requested).is_absolute() {
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/lang/UnsatisfiedLinkError",
                    &format!("Expecting an absolute path of the library: {requested}"),
                ));
            }
        }
        LibrarySpelling::BareName => {
            if requested.contains(std::path::MAIN_SEPARATOR) {
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/lang/UnsatisfiedLinkError",
                    &format!("Directory separator should not appear in library name: {requested}"),
                ));
            }
        }
    }
    let target = match spelling {
        LibrarySpelling::BareName => platform_lib_name(requested),
        LibrarySpelling::AbsolutePath => requested.to_string(),
    };
    if ctx.load_native_library(&target).is_ok() {
        return loaded_by(ctx, requested, from_class, scoping);
    }
    // A JDK-image library whose natives this VM already provides is not a
    // failure — see `is_vm_provided_jdk_library`. Only a bare name can name
    // one; `System.load("/some/path/libzip.so")` names a FILE, and a file that
    // is not there is an error however it is spelled.
    if spelling == LibrarySpelling::BareName && is_vm_provided_jdk_library(requested) {
        return loaded_by(ctx, requested, from_class, scoping);
    }
    // Same reasoning, decided by measurement instead of by list: a library that
    // SHIPS IN THE JDK IMAGE is one HotSpot loads, so refusing it here would be
    // a divergence in the opposite direction from the one this function exists
    // to fix. Restricted to the image directory on purpose -- a library the
    // user put on `java.library.path` that we failed to open is a real failure.
    if spelling == LibrarySpelling::BareName && jdk_image_ships_library(&*ctx, requested) {
        return loaded_by(ctx, requested, from_class, scoping);
    }
    // NOT memoised as a failure: the JDK re-attempts the lookup on every call
    // (`RJdkFailure.java:269` asserts the second attempt throws too), and a
    // library can legitimately appear on `java.library.path` between calls.
    Err(RuntimeError::UnsatisfiedLinkError {
        message: format!("no {requested} in java.library.path"),
    }
    .into())
}

pub(crate) fn native_runtime_get_runtime(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let class_id = match ctx.ensure_class_initialized("java/lang/Runtime") {
        Ok(id) => id,
        // Fallible since 2026-08-10 (JDK-only wave 2, step 3): `java.lang.Runtime`
        // is in every image, so the `Ok` arm is what runs; handing back a
        // fabricated `Runtime` on a broken one substitutes for `java.base`
        // rather than reporting that it is missing.
        Err(_) => crate::util_concurrent_ext::refused_class(ctx, "java/lang/Runtime", 8)?,
    };
    // `Runtime.getRuntime()` MUST answer the same object every time -- the real
    // class is `private static final Runtime currentRuntime = new Runtime()`
    // and `getRuntime()` is a bare `getstatic`. Allocating a fresh object per
    // call made `Runtime.getRuntime() == Runtime.getRuntime()` FALSE (MEASURED
    // against HotSpot 25.0.3+9, `probes/TailFamilySweep.java`, both modes).
    //
    // That is not a cosmetic identity: `addShutdownHook` and
    // `removeShutdownHook` are instance methods, so a hook registered on one
    // instance is invisible to every later caller's instance, and any code that
    // caches the Runtime and compares it later disagrees with itself.
    //
    // Reading and writing the REAL `currentRuntime` static rather than a
    // Rust-side cache means the object this hands back is the same one the
    // JDK's own bytecode sees, and it is rooted by the class rather than needing
    // a global root of ours.
    if let Some(idx) = ctx.static_field_index_by_name(class_id, "currentRuntime") {
        if let Value::Object(Some(existing)) = ctx.get_static_field(class_id, idx) {
            return Ok(Some(Value::Object(Some(existing))));
        }
        let obj = ctx.alloc_object(class_id, 0);
        ctx.set_static_field(class_id, idx, Value::Object(Some(obj)));
        return Ok(Some(Value::Object(Some(obj))));
    }
    // No such static (a synthetic-JDK image): fall back to the previous
    // behaviour rather than inventing a side table for a shape that has no
    // `currentRuntime` to be consistent with.
    let obj = ctx.alloc_object(class_id, 0);
    Ok(Some(Value::Object(Some(obj))))
}

pub(crate) fn native_runtime_available_processors(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Container-aware: respects the cgroup CPU quota under
    // `-XX:+UseContainerSupport` (falls back to the host thread count).
    Ok(Some(Value::Int(ctx.available_processor_count())))
}

pub(crate) fn native_runtime_max_memory(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Report the heap's real ceiling instead of a hardcoded 256 MB, matching
    // HotSpot's contract that `maxMemory()` is `CollectedHeap::max_capacity()`:
    // the configured `-Xmx` (container-aware once sized from a cgroup limit),
    // less what the collector can never allocate — since gen r5w2/obs6 the
    // Generational young copy reserve, as HotSpot Serial leaves one survivor
    // space out (`NativeContext::max_heap_bytes` in the VM).
    Ok(Some(Value::Long(ctx.max_heap_bytes())))
}

/// `Runtime.totalMemory()` — bytes currently committed for the Java heap.
///
/// These two were hardcoded 64 MiB / 32 MiB and never moved: not on
/// allocation, not across `Runtime.gc()`. That left the VM giving two different
/// answers for one quantity — `maxMemory()` already returned the real `-Xmx`,
/// and the JMX heap `MemoryUsage` already reported a real `used` from
/// `heap_allocated_bytes()` — and any application that sizes a cache or buffer
/// from the free heap got a constant. H2's `Utils.getMemoryUsed()` is
/// `totalMemory() - freeMemory()`, so `TestLIRSMemoryConsumption` printed a
/// memory delta of exactly 0 on every row where HotSpot prints real numbers.
///
/// `committed_heap_bytes` is the heap's COMMITTED size, HotSpot's meaning
/// (`-Xms64m -Xmx512m` starts at 64m and grows) — since 2026-09-23. Before that
/// the Generational and ZGC backends answered with their reservation, so this
/// returned `-Xmx` from the first instruction on the default collector. It is
/// never an OCCUPANCY: see `gc::vm_heap::VmHeap::committed_bytes` for why a
/// value that moved on every allocation would be a behavioural change for
/// callers that gc until `totalMemory()` settles.
///
/// gen r5w2/obs6 (2026-09-26): on Generational it no longer counts the young
/// copy reserve (the empty to-space, never allocatable while mutators run),
/// as HotSpot Serial's `capacity()` does not; the JMX pools moved with it, so
/// `totalMemory()`, the heap `MemoryUsage.getCommitted()` and the sum of the
/// three pools' `committed` are one number.
pub(crate) fn native_runtime_total_memory(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Long(runtime_committed_heap(ctx))))
}

/// `Runtime.freeMemory()` — committed minus used, both from the live heap.
///
/// Saturating: `heap_allocated_bytes` and `committed_heap_bytes` are sampled
/// separately and without a lock, so a concurrent allocation can make used
/// exceed the committed figure read a moment earlier. HotSpot never reports a
/// negative free heap; report 0 rather than a wrapped `Long`.
///
/// `used` is heap OCCUPANCY and must stay occupancy. It answered with the
/// generational young arena's raw bump cursor until 2026-09-08, and that cursor
/// never retreats under the in-place non-moving sweep — so `freeMemory()`
/// reported a heap that filled once and never emptied, however much the
/// collector reclaimed. See `NativeContext::heap_allocated_bytes`, whose doc
/// carries the H2 measurement.
pub(crate) fn native_runtime_free_memory(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let committed = runtime_committed_heap(ctx);
    let used = ctx.heap_allocated_bytes() as i64;
    Ok(Some(Value::Long(committed.saturating_sub(used).max(0))))
}

/// Committed heap for the `Runtime` accessors, clamped into `[used, -Xmx]`.
///
/// Two clamps, both for the same reason — a report that contradicts one of the
/// VM's OWN other answers is worse than a coarse one:
///
/// * never below `used`, so `freeMemory()` is never negative (the two are
///   sampled separately, and a heap growing into a fresh granule or region
///   between the samples can read `used` a moment ahead of `committed`);
/// * never above `maxMemory()`, which is the heap's ceiling (the configured
///   `-Xmx`, less the Generational copy reserve since gen r5w2/obs6) and is
///   the ceiling every caller compares against.
fn runtime_committed_heap(ctx: &mut dyn NativeContext) -> i64 {
    let used = ctx.heap_allocated_bytes() as i64;
    let max = ctx.max_heap_bytes();
    let committed = ctx.committed_heap_bytes() as i64;
    committed.max(used).min(max.max(used))
}

/// The four real `java.lang.Runtime$Version` field values, parsed out of a
/// version string without the JDK's regex engine.
struct RuntimeVersionParts {
    /// `version` — the dot-separated `$VNUM` sequence.
    numbers: Vec<i32>,
    /// `pre` — the `-$PRE` pre-release identifier.
    pre: Option<String>,
    /// `build` — the `+$BUILD` build number.
    build: Option<i32>,
    /// `optional` — the trailing `-$OPT` build information.
    optional: Option<String>,
}

/// Hand-parse `$VNUM(-$PRE)?(\+$BUILD)?(-$OPT)?` — the JDK's `VSTR_FORMAT`.
///
/// `Runtime.Version.parse` compiles a `java.util.regex` pattern for this
/// grammar; running it is exactly what `native_runtime_version` exists to
/// avoid (see its doc comment). The grammar is small enough to split by hand:
/// `+` separates `$VNUM(-$PRE)` from `$BUILD(-$OPT)`, and without a `+` the
/// only `-`-introduced parts are `$PRE` then `$OPT`.
fn parse_runtime_version_str(s: &str) -> Option<RuntimeVersionParts> {
    let s = s.trim();
    let (head, tail) = match s.split_once('+') {
        Some((head, tail)) => (head, Some(tail)),
        None => (s, None),
    };
    let (vnum, mut pre) = match head.split_once('-') {
        Some((vnum, pre)) => (vnum, Some(pre.to_string())),
        None => (head, None),
    };
    let mut build = None;
    let mut optional = None;
    match tail {
        // `$BUILD` is present (or empty, in the `+-$OPT` spelling the JDK uses
        // for optional-without-build); `$OPT` is whatever follows its `-`.
        Some(tail) => match tail.split_once('-') {
            Some((b, opt)) => {
                build = b.parse::<i32>().ok();
                optional = Some(opt.to_string());
            }
            None => build = tail.parse::<i32>().ok(),
        },
        // No `+$BUILD`, so a second `-` inside what we took for `$PRE` is
        // really the `$OPT` separator (`$PRE` itself is `[a-zA-Z0-9]+`).
        None => {
            if let Some((p, opt)) = pre.as_deref().and_then(|p| p.split_once('-')) {
                optional = Some(opt.to_string());
                pre = Some(p.to_string());
            }
        }
    }
    let numbers = vnum
        .split('.')
        .map(|part| part.parse::<i32>().ok())
        .collect::<Option<Vec<i32>>>()?;
    if numbers.is_empty() {
        return None;
    }
    Some(RuntimeVersionParts {
        numbers,
        pre,
        build,
        optional,
    })
}

/// The version string this VM reports, as `java.runtime.version` would spell
/// it, falling back through the other version properties.
fn runtime_version_parts(ctx: &dyn NativeContext) -> RuntimeVersionParts {
    for key in [
        "java.runtime.version",
        "java.vm.version",
        "java.version",
        "java.specification.version",
    ] {
        if let Some(parts) = ctx
            .get_system_property(key)
            .as_deref()
            .and_then(parse_runtime_version_str)
        {
            return parts;
        }
    }
    RuntimeVersionParts {
        numbers: vec![runtime_feature_version(ctx)],
        pre: None,
        build: None,
        optional: None,
    }
}

/// `Optional.ofNullable(value)` for a `String` field.
fn optional_of_str(ctx: &mut dyn NativeContext, value: Option<&str>) -> Option<Value> {
    match value {
        Some(text) => {
            let text = ctx.create_string(text);
            ctx.invoke(
                "java/util/Optional",
                "of",
                "(Ljava/lang/Object;)Ljava/util/Optional;",
                &[Value::Object(Some(text))],
            )
        }
        None => ctx.invoke("java/util/Optional", "empty", "()Ljava/util/Optional;", &[]),
    }
    .ok()
    .flatten()
}

/// `Optional.ofNullable(value)` for the boxed-`Integer` `build` field.
fn optional_of_int(ctx: &mut dyn NativeContext, value: Option<i32>) -> Option<Value> {
    match value {
        Some(n) => {
            let boxed = ctx
                .invoke(
                    "java/lang/Integer",
                    "valueOf",
                    "(I)Ljava/lang/Integer;",
                    &[Value::Int(n)],
                )
                .ok()
                .flatten()?;
            ctx.invoke(
                "java/util/Optional",
                "of",
                "(Ljava/lang/Object;)Ljava/util/Optional;",
                &[boxed],
            )
        }
        None => ctx.invoke("java/util/Optional", "empty", "()Ljava/util/Optional;", &[]),
    }
    .ok()
    .flatten()
}

/// `List.of(Integer...)` — the `version` field's declared shape.
fn boxed_int_list(ctx: &mut dyn NativeContext, numbers: &[i32]) -> Option<Value> {
    let object_cid = ctx.class_id_by_name("java/lang/Object")?;
    let array = ctx.new_ref_array(object_cid, numbers.len());
    let array_pin = ctx.pin_native_root(array);
    let mut array = array;
    for (index, n) in numbers.iter().enumerate() {
        let boxed = ctx
            .invoke(
                "java/lang/Integer",
                "valueOf",
                "(I)Ljava/lang/Integer;",
                &[Value::Int(*n)],
            )
            .ok()
            .flatten();
        array = ctx.read_native_pin(array_pin, array);
        match boxed {
            Some(boxed) => ctx.set_array_element(array, index, boxed),
            None => {
                ctx.unpin_native_roots(array_pin);
                return None;
            }
        }
    }
    let list = ctx
        .invoke(
            "java/util/List",
            "of",
            "([Ljava/lang/Object;)Ljava/util/List;",
            &[Value::Object(Some(array))],
        )
        .ok()
        .flatten();
    // `List.of` is unavailable in synthetic-JDK mode; `Arrays.asList` has a
    // native bridge there and satisfies every `version` field reader
    // (`stream()`, `get(int)`, `size()`, `equals`, `hashCode`).
    let list = match list {
        Some(Value::Object(Some(_))) => list,
        _ => {
            let array = ctx.read_native_pin(array_pin, array);
            ctx.invoke(
                "java/util/Arrays",
                "asList",
                "([Ljava/lang/Object;)Ljava/util/List;",
                &[Value::Object(Some(array))],
            )
            .ok()
            .flatten()
        }
    };
    ctx.unpin_native_roots(array_pin);
    list
}

/// `Runtime.version()` returns a lightweight real-layout `Runtime$Version`.
///
/// Calling the JDK parser here routes every first `Runtime.version()` through
/// the regex engine.  That engine is prohibitively slow in interpreted real-
/// JDK mode and blocks signed-jar opening before any loader work begins.
/// Instead we hand-parse the reported version string (no regex) and populate
/// the same four fields the parser would have written, so every real-bytecode
/// accessor on the result — `toString`, `version`, `interim`, `update`,
/// `patch`, `pre`, `optional`, `equals`, `hashCode`, `compareTo` — reads real
/// values instead of `null`. Population is best-effort: during very early
/// bootstrap `List`/`Optional` may not be usable yet, and a bare object (the
/// historical behaviour) is still better than failing `Runtime.version()`.
///
/// The result is memoised for the life of the VM, exactly as the real
/// `Runtime.version()` body memoises into its `private static Runtime.version`
/// field. That restores the `Runtime.version() == Runtime.version()` identity
/// HotSpot callers see, and keeps building the object a once-per-process cost
/// rather than a per-call one (measured 14µs/call to build).
pub(crate) fn native_runtime_version(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    if let Some(cached) = runtime_version_cached(ctx) {
        return Ok(Some(Value::Object(Some(cached))));
    }
    let version = try_alloc_concurrent_synthetic(ctx, "java/lang/Runtime$Version", 4)?;
    let parts = runtime_version_parts(ctx);
    let pin = ctx.pin_native_root(version);
    let mut version = version;
    // Each helper re-enters the VM and can move `version`, so build one field
    // value at a time and re-read the pinned reference before storing it.
    let numbers = boxed_int_list(ctx, &parts.numbers);
    version = ctx.read_native_pin(pin, version);
    if let Some(numbers) = numbers {
        ctx.set_field_by_name(version, "version", numbers);
    }
    let pre = optional_of_str(ctx, parts.pre.as_deref());
    version = ctx.read_native_pin(pin, version);
    if let Some(pre) = pre {
        ctx.set_field_by_name(version, "pre", pre);
    }
    let build = optional_of_int(ctx, parts.build);
    version = ctx.read_native_pin(pin, version);
    if let Some(build) = build {
        ctx.set_field_by_name(version, "build", build);
    }
    let optional = optional_of_str(ctx, parts.optional.as_deref());
    version = ctx.read_native_pin(pin, version);
    if let Some(optional) = optional {
        ctx.set_field_by_name(version, "optional", optional);
    }
    ctx.unpin_native_roots(pin);
    Ok(Some(Value::Object(Some(runtime_version_publish(
        ctx, version,
    )))))
}

/// Global-root handle for the `Runtime.version()` singleton.
///
/// A `ctx.set_static_field` into the JDK's own `Runtime.version` field does not
/// stick (the write is accepted and the next read returns null), so the
/// singleton lives in the JNI global-ref table instead — a persistent root the
/// moving collector remaps, which is why this holds a *handle* and never a raw
/// `ObjectRef`.
///
/// gc-common w16-e: one handle PER VM. It was one process-wide
/// `Mutex<Option<usize>>`, and a handle is a slot number in one VM's
/// global-root table: every later VM in the process resolved the first VM's
/// slot in its own table and answered `Runtime.version()` with whatever object
/// sat there (or nothing, and rebuilt on every call). Dropped at VM teardown
/// (`forget_vm_system_singletons`).
static RUNTIME_VERSION_ROOT: VmScoped<Option<usize>> = VmScoped::new();

/// The already-built `Runtime.version()` singleton, if there is one.
fn runtime_version_cached(ctx: &dyn NativeContext) -> Option<ObjectRef> {
    let handle = RUNTIME_VERSION_ROOT
        .peek(ctx.vm_identity(), |slot| *slot)
        .flatten()?;
    ctx.resolve_global_root(handle)
}

/// Publish `version` as the singleton and return whichever instance won — a
/// concurrent first call may have published its own, and identity is the whole
/// point of memoising.
///
/// The row lock is held across the global-root lookup and insert (as the old
/// process-wide lock was) so two first calls cannot both publish; neither
/// allocates on the heap nor runs Java.
fn runtime_version_publish(ctx: &mut dyn NativeContext, version: ObjectRef) -> ObjectRef {
    let vm = ctx.vm_identity();
    RUNTIME_VERSION_ROOT.with(vm, |slot| {
        if let Some(winner) = slot.and_then(|handle| ctx.resolve_global_root(handle)) {
            return winner;
        }
        let handle = ctx.add_global_root(version);
        // Handle 0 means "no global-root table" (mock contexts): leave the slot
        // empty so the next call rebuilds rather than caching a dead handle.
        if handle != 0 {
            *slot = Some(handle);
        }
        version
    })
}

/// `Runtime.Version.feature()` вЂ” major Java specification version (e.g. 25).
pub(crate) fn native_runtime_version_feature(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // `Runtime.Version.parse("8")` produces a fully initialized real JDK
    // Version object (not the lightweight metadata fallback below). Its
    // `version` list is authoritative: returning the host VM feature for it
    // makes `JarFile.baseVersion()` incorrectly report 25, disabling every
    // multi-release lookup in Spring Boot's NestedJarFile.
    if let Ok(this) = obj_arg(args, 0) {
        if let Value::Object(Some(parts)) = ctx.get_field_by_name(this, "version") {
            let parts_pin = ctx.pin_native_root(parts);
            let parts = ctx.read_native_pin(parts_pin, parts);
            let first = ctx.invoke_virtual(parts, "get", "(I)Ljava/lang/Object;", &[Value::Int(0)]);
            ctx.unpin_native_roots(parts_pin);
            if let Ok(Some(Value::Object(Some(first)))) = first {
                let first_pin = ctx.pin_native_root(first);
                let first = ctx.read_native_pin(first_pin, first);
                let value = ctx.invoke_virtual(first, "intValue", "()I", &[]);
                ctx.unpin_native_roots(first_pin);
                if let Ok(Some(Value::Int(value))) = value {
                    return Ok(Some(Value::Int(value)));
                }
            }
        }
    }
    Ok(Some(Value::Int(runtime_feature_version(ctx))))
}

/// The host VM's feature (major) version, from system properties alone.
///
/// This is `Runtime.Version.feature()`'s fallback, factored out so callers that
/// only want the number don't have to build a whole `Runtime$Version` object
/// to ask for it.
pub(crate) fn runtime_feature_version(ctx: &dyn NativeContext) -> i32 {
    ctx.get_system_property("java.specification.version")
        .and_then(|s| s.trim().parse::<i32>().ok())
        .or_else(|| {
            ctx.get_system_property("java.version").and_then(|s| {
                let t = s.trim();
                if let Some(rest) = t.strip_prefix("1.") {
                    rest.split('.').next()?.parse().ok()
                } else {
                    t.split('.').next()?.parse().ok()
                }
            })
        })
        .unwrap_or(25)
}

/// `Runtime.Version.build()` - optional build number.
///
/// This override is forced for EVERY `Runtime$Version` receiver, including the
/// fully parsed ones `Runtime.Version.parse(String)` produces, so it must read
/// the real `build` field rather than assume the receiver is a lightweight
/// `Runtime.version()` object. Returning a blanket `Optional.empty()` made a
/// parsed version disagree with itself: `compareTo`/`equalsIgnoreOptional`
/// read the `build` FIELD on the receiver but the `build()` ACCESSOR on the
/// argument, so `v.compareTo(v)` returned 1 for any version with a build
/// number. `Optional.empty()` remains the fallback for a receiver whose field
/// is genuinely absent (synthetic-JDK mode, where the class has no real
/// layout to populate).
pub(crate) fn native_runtime_version_build(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Ok(this) = obj_arg(args, 0) {
        if let build @ Value::Object(Some(_)) = ctx.get_field_by_name(this, "build") {
            return Ok(Some(build));
        }
    }
    ctx.invoke("java/util/Optional", "empty", "()Ljava/util/Optional;", &[])
}

pub(crate) fn native_runtime_exit(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let code = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => match args.first() {
            Some(Value::Int(v)) => *v,
            _ => 0,
        },
    };

    // Mirror native_system_exit: env-gated caller-chain dump and soft-return.
    if crate::nbflags().dbg_exit {
        let trace = ctx.capture_stack_trace();
        let mut rendered = String::new();
        for (i, entry) in trace.iter().take(20).enumerate() {
            use std::fmt::Write as _;
            let _ = write!(
                rendered,
                "\n  #{i} {cls}.{m} (bci={bci})",
                cls = entry.class_name,
                m = entry.method_name,
                bci = entry.byte_code_index,
            );
        }
        tracing::warn!(
            target: "cratonvm::system_exit",
            "[CRATONVM_DBG_EXIT] Runtime.exit({code}) caller chain:{rendered}"
        );
    }

    if crate::nbflags().soft_exit {
        tracing::warn!(
            target: "cratonvm::system_exit",
            "[cratonvm] Runtime.exit({code}) soft-returned (CRATONVM_SOFT_EXIT=1)"
        );
        return Ok(None);
    }

    // B6: Surface Runtime.exit calls so silent shutdowns are visible.
    claim_exit_or_block_forever(ctx);
    // Interpreter round i4 wave 84: HotSpot prints nothing on exit, in either
    // mode, so the line is the exit census's alone (as `native_system_exit`).
    if exit_census_on() || cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_EXIT").is_some() {
        eprintln!("[cratonvm] Runtime.exit({code}) called - process terminating");
    }
    // W7-92: see `native_system_exit` for why the hooks run before the sweep.
    run_shutdown_hooks(ctx, "Runtime.exit");
    sweep_declared_slot_maps_before_exit(&*ctx, "Runtime.exit");
    invoke_pre_exit_hook_from(ctx, code);
    // See `native_system_exit` — same reason, same position, same `code`.
    flush_console_streams(&*ctx);
    cratonvm_native_api::process_exit::exit_process(code);
}

/// `java.lang.Shutdown.halt0(int)` — the bottom of `Runtime.halt(int)`.
///
/// Terminates with the requested status and, unlike the `exit` path, does NOT
/// run Java shutdown hooks: `Runtime.halt` is specified as forcible
/// termination, and the JDK runs hooks from `Shutdown.exit`, which halt
/// bypasses. The VM-internal pre-exit hook (staged-archive cleanup, JFR
/// dump-on-exit) still fires — it is not a Java shutdown hook, and its own
/// comment already claims to cover `Runtime.halt`.
///
/// W7-92 made the `exit` paths run hooks and deliberately left this one alone.
/// MEASURED on HotSpot 25.0.3+9: `ShutdownProbe halt` produced no hook output
/// and rc=5, and `HookContract haltinhook` — `Runtime.halt(9)` called from
/// INSIDE a running hook — terminated immediately at rc=9 with the remaining
/// hooks unrun. The absence of a `run_shutdown_hooks` call below is the
/// behaviour, not an oversight; a later sweep tidying the asymmetry away would
/// be a regression.
pub(crate) fn native_shutdown_halt0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let code = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };

    // Same opt-in escape hatch the two `exit` natives honour: turn the
    // termination into a return so whatever the caller was hiding behind it
    // becomes visible. Off by default.
    if crate::nbflags().soft_exit {
        tracing::warn!(
            target: "cratonvm::system_exit",
            "[cratonvm] Runtime.halt({code}) soft-returned (CRATONVM_SOFT_EXIT=1)"
        );
        return Ok(None);
    }

    // Plain ASCII dash on purpose: the neighbouring exit messages carry a
    // mojibake em-dash from an old encoding mishap and print as garbage.
    //
    // SBR5 sysprops: under `--jdk-only` this native is also the bottom of the JDK's own
    // `System.exit` -> `Runtime.exit` -> `Shutdown.exit` -> `Shutdown.halt`, which is
    // retired to bytecode. HotSpot prints nothing on exit, so the line stays
    // `--compatible`'s alone (where the exit natives print their own).
    // Interpreter round i4 wave 84: `--compatible` printed it too, a byte
    // HotSpot never writes (`L6W84DeadlockDetection --compatible` DIFF(1)).
    if exit_census_on() || cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_EXIT").is_some() {
        eprintln!("[cratonvm] Runtime.halt({code}) called - process terminating");
    }
    sweep_declared_slot_maps_before_exit(&*ctx, "Runtime.halt");
    invoke_pre_exit_hook_from(ctx, code);
    // NO `run_shutdown_hooks` here, and that is the whole point of `halt`
    // (MEASURED, `HookProbe halt`: `HOOK-OUT h-1 MUST-NOT-APPEAR` does not
    // appear, rc 5). The FLUSH is a different question and HotSpot answers it
    // the other way: `HookProbe haltnoflush` writes an unterminated,
    // unflushed `System.out.print` and then halts, and the bytes are still
    // delivered. So flush, run nothing.
    flush_console_streams(&*ctx);
    cratonvm_native_api::process_exit::exit_process(code);
}

// ---------------------------------------------------------------------------
// Runtime.exec вЂ” spawn subprocesses via std::process::Command
// Process synthetic: 3-field (exit_code=0 Int, stdout=1 String, stderr=2 String)
// ---------------------------------------------------------------------------

/// Consult the installed `java.lang.SecurityManager`, if any, before
/// spawning a host process. Mirrors HotSpot's behaviour: every
/// `ProcessBuilder.start` and `Runtime.exec*` overload must call
/// `SecurityManager.checkExec(command[0])` before the spawn syscall
/// (fork/exec/CreateProcess) is issued. If the SM throws
/// `SecurityException`, the error is propagated to the Java caller and
/// the spawn MUST NOT happen.
///
/// `command_first` is the program path (`command[0]`) as it will be
/// handed to `std::process::Command::new`. An empty string is rejected
/// up-front so a misuse on the SM side (treating `""` as "allow
/// nothing") can't be bypassed by passing an empty argv.
///
/// With no SecurityManager installed this is a no-op вЂ” matching JDK
/// behaviour where `Runtime.exec` is unrestricted until `System.setSecurityManager`
/// is called.
///
/// Audit TODO (Panama): host-call sites that go through `jdk.internal.foreign`
/// / `java.lang.foreign.Linker` can invoke `execve`/`CreateProcessW`
/// without ever transiting `ProcessBuilder.start` or `Runtime.exec`.
/// That bypass is not addressed here вЂ” gating it requires intercepting
/// every Panama downcall, tracked as a separate task. See
/// `native-builtins::panama` for the FFI entry points.
/// Install `check_exec_or_throw` as `native-io`'s pre-spawn policy gate.
///
/// `native-io` owns every real spawn (`ProcessBuilder.start`, `Runtime.exec`,
/// `ProcessImpl.create`, `forkAndExec` all funnel into `spawn_and_wrap`) but
/// cannot reach the SecurityManager, which lives in this crate. This hands it
/// the function pointer. Idempotent, so calling it from more than one
/// registration path is safe.
///
/// Must be called from a path that runs in EVERY mode: the gate is a security
/// control, and a mode that skips the install silently spawns ungated.
pub(crate) fn install_spawn_policy_hook() {
    cratonvm_native_io::process::set_spawn_policy_hook(check_exec_or_throw);
}

pub(crate) fn check_exec_or_throw(
    ctx: &mut dyn NativeContext,
    command_first: &str,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    // Pass ctx so the singleton read re-fetches the CURRENT (post-GC) address
    // via the var-handle-root registry (raw static copies are never remapped).
    if crate::security_manager::get_security_manager(&*ctx).is_none() {
        return Ok(());
    }

    // Allocate a Java String for command[0] and call sm.checkExec(String).
    // The synthetic SecurityManager.checkExec native (security_manager.rs)
    // routes through checkPermission в†’ policy_allows_full_generic; a
    // denial surfaces as RuntimeError::SecurityException, which we
    // propagate verbatim so the Java caller observes a SecurityException
    // and the spawn does NOT happen.
    //
    // `invoke_virtual` takes (receiver, method, descriptor, args) where
    // `args` lists ONLY the explicit method parameters вЂ” the receiver is
    // not duplicated in the args slice (see other call sites such as
    // `AccessController.doPrivileged` in security_manager.rs).
    let cmd_obj = ctx.create_string(command_first);
    // gc-common w16-e: the String's allocation is a GC point, so the manager
    // is read AFTER it (it used to be the receiver read before it).
    let Some(sm_ref) = crate::security_manager::get_security_manager(&*ctx) else {
        return Ok(());
    };
    let args = [Value::Object(Some(cmd_obj))];
    ctx.invoke_virtual(sm_ref, "checkExec", "(Ljava/lang/String;)V", &args)?;
    Ok(())
}

/// Read a String[] from an object reference into a Vec<String>.
///
/// A null element is dropped. Callers that must distinguish "absent" from
/// "null" (`Runtime.exec`'s cmdarray, where HotSpot throws
/// `NullPointerException`) use `checked_cmdarray` instead.
fn read_string_array(ctx: &mut dyn NativeContext, arr_val: &Value) -> Vec<String> {
    let arr = match arr_val {
        Value::Object(Some(a)) => *a,
        _ => return Vec::new(),
    };
    let len = ctx.array_length(arr);
    let mut result = Vec::with_capacity(len);
    for i in 0..len {
        if let Value::Object(Some(s)) = ctx.get_array_element(arr, i) {
            result.push(ctx.read_string(s).unwrap_or_default());
        }
    }
    result
}

/// Validate and read a `Runtime.exec` cmdarray the way `ProcessBuilder.start`
/// does: `NullPointerException` for a null array or any null element, then
/// `ArrayIndexOutOfBoundsException` for an empty one (that is `cmdarray[0]`
/// failing, which is why the reported index is 0).
///
/// `read_string_array` DROPS a null element, so `exec(["/bin/echo", null])`
/// used to run a different command line than the caller wrote instead of
/// throwing. Verified against HotSpot 25 in probes/ProcSurfaceProbe.java
/// (T10/T11).
fn checked_cmdarray(
    ctx: &mut dyn NativeContext,
    arr_val: &Value,
) -> Result<Vec<String>, MethodCallFailed> {
    let arr = match arr_val {
        Value::Object(Some(a)) => *a,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("Runtime.exec: null command array".to_string()),
            }
            .into())
        }
    };
    let len = ctx.array_length(arr);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        match ctx.get_array_element(arr, i) {
            Value::Object(Some(s)) => out.push(ctx.read_string(s).unwrap_or_default()),
            _ => {
                return Err(RuntimeError::NullPointerException {
                    message: Some("Runtime.exec: null element in command array".to_string()),
                }
                .into())
            }
        }
    }
    if out.is_empty() {
        return Err(RuntimeError::aioobe(0, 0).into());
    }
    Ok(out)
}

/// Execute a command and hand back a LIVE `Process`.
///
/// Delegates to `native-io`'s `spawn_and_wrap`, the SAME spawn the
/// `ProcessBuilder.start` native uses. That keeps the `std::process::Child`
/// alive in the process table, registers its three pipes in the `FdTable`, and
/// wraps them in a `cratonvm/synthetic/Process` whose layout every
/// `java.lang.Process` native in `native-io` already reads.
///
/// It used to run `Command::output()` here instead, storing the child's whole
/// stdout and stderr as two Java Strings on a 3-slot object allocated under
/// `java/lang/Process`. Three defects fell out of that, and the delegation
/// fixes all three:
///
///   * **The object had nowhere to put those slots.** In real-JDK mode
///     `java.lang.Process` is a REAL loaded class with six fields of its own,
///     so `try_alloc_concurrent_synthetic("java/lang/Process", 3)?` produced a
///     real-layout six-slot object and slots 0..2 aliased `java.lang.Process`'s
///     own reader/writer caches.
///   * **Nothing downstream spoke that layout.** `getInputStream()` and its
///     siblings are answered by `native-io::process`, which reads an
///     fd-carrying layout offset PAST those six real fields. It found nothing,
///     fell through to a pipe stream on fd -1, and every read returned EOF, so
///     a `Runtime.exec` child's output was unreachable. That is what emptied
///     Tomcat's CGI response body (`TestSecurity2019.testCVE_2019_0232`:
///     `rc == 200`, body `null`).
///   * **`exec` blocked until the child exited.** `Runtime.exec` must return
///     immediately with a running child. Blocking meant `isAlive()` was never
///     true and `exitValue()` never threw `IllegalThreadStateException` (the
///     exact signal `CGIServlet` polls on), and a child that reads its stdin
///     could never be fed, because the caller only receives the pipe once
///     `exec` has returned.
///
/// `redirect_error_stream` is false: no `Runtime.exec` overload asks for a
/// merged stream, so stdout and stderr stay separate pipes.
fn runtime_spawn_process(
    ctx: &mut dyn NativeContext,
    cmd: &[String],
    env: Option<&[String]>,
    work_dir: Option<&str>,
) -> MethodCallResult {
    if cmd.is_empty() {
        return Err(RuntimeError::aioobe(0, 0).into());
    }
    let program = cmd[0].clone();

    // SECURITY: `SecurityManager.checkExec(command[0])` is NOT called here.
    // `spawn_and_wrap` runs it as its first act, via the policy hook installed
    // by `install_spawn_policy_hook`, so that the gate covers every spawn
    // route rather than only this one. Calling it here as well would consult
    // the SecurityManager twice per `exec` -- observable to any policy that
    // counts or logs checks, and not what HotSpot does.

    // HotSpot: a null `envp` inherits this process's environment unchanged; a
    // non-null one REPLACES it wholesale. `clear_env` carries that
    // distinction, which is why it is derived from the presence of `env` and
    // not from the pair count: an empty-but-present `envp` must give the child
    // an empty environment, not an inherited one.
    let env_pairs: Option<Vec<(String, String)>> = env.map(|vars| {
        vars.iter()
            .filter_map(|var| {
                var.split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()))
            })
            .collect()
    });
    let clear_env = env_pairs.is_some();

    cratonvm_native_io::process::spawn_and_wrap(
        ctx,
        &program,
        &cmd[1..],
        work_dir,
        env_pairs.as_deref(),
        clear_env,
        false,
    )
}

/// Read the `File` working-directory argument of the three-arg `exec`
/// overloads.
///
/// Reads `path` BY NAME first: in real-JDK mode `java.io.File`'s slot 0 is not
/// necessarily its `path` field, and the previous slot-0-only read then
/// produced `None` -- a working directory the caller asked for, silently
/// dropped. Same order `native-io::process::file_path_of` uses.
fn exec_dir_path(ctx: &mut dyn NativeContext, arg: Option<&Value>) -> Option<String> {
    let file_obj = match arg {
        Some(Value::Object(Some(f))) => *f,
        _ => return None,
    };
    match ctx.get_field_by_name(file_obj, "path") {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => match ctx.get_field(file_obj, 0) {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        },
    }
}

/// Runtime.exec(String) вЂ” parse command line split by whitespace.
pub(crate) fn native_runtime_exec_string(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = Runtime instance, args[1] = command string
    // NULL is an NPE and EMPTY is an IllegalArgumentException -- two different
    // doors, and this VM had them swapped and missing respectively.
    //
    // MEASURED in COMPATIBLE mode against HotSpot 25.0.3+9
    // (`probes/Phase1Sweep.java`, the P1-I lane). Strict mode already agreed on
    // both rows, because it drops these `SyntheticStub`s and runs the JDK's own
    // `exec` -- so this is a defect the strict work exposed in the DEFAULT mode:
    //
    //   Runtime.exec("")     HotSpot IllegalArgumentException  CratonVM ArrayIndexOutOfBounds
    //   Runtime.exec(null)   HotSpot NullPointerException      CratonVM IllegalArgumentException
    //
    // The JDK's `exec(String, String[], File)` opens with
    // `if (command.isEmpty()) throw new IllegalArgumentException("Empty
    // command")`, so a null receiver NPEs on `isEmpty()` one instruction before
    // the emptiness test can run. That ordering is the whole of the behaviour.
    let cmd_str = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
    };
    let parts: Vec<String> = cmd_str.split_whitespace().map(String::from).collect();
    if parts.is_empty() {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Empty command".to_string(),
        }
        .into());
    }
    runtime_spawn_process(ctx, &parts, None, None)
}

/// Runtime.exec(String[])
pub(crate) fn native_runtime_exec_array(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let arg_val = args.get(1).copied().unwrap_or(Value::Object(None));
    let cmd = checked_cmdarray(ctx, &arg_val)?;
    runtime_spawn_process(ctx, &cmd, None, None)
}

/// Runtime.exec(String, String[])
pub(crate) fn native_runtime_exec_string_env(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // NULL is an NPE and EMPTY is an IllegalArgumentException -- two different
    // doors, and this VM had them swapped and missing respectively.
    //
    // MEASURED in COMPATIBLE mode against HotSpot 25.0.3+9
    // (`probes/Phase1Sweep.java`, the P1-I lane). Strict mode already agreed on
    // both rows, because it drops these `SyntheticStub`s and runs the JDK's own
    // `exec` -- so this is a defect the strict work exposed in the DEFAULT mode:
    //
    //   Runtime.exec("")     HotSpot IllegalArgumentException  CratonVM ArrayIndexOutOfBounds
    //   Runtime.exec(null)   HotSpot NullPointerException      CratonVM IllegalArgumentException
    //
    // The JDK's `exec(String, String[], File)` opens with
    // `if (command.isEmpty()) throw new IllegalArgumentException("Empty
    // command")`, so a null receiver NPEs on `isEmpty()` one instruction before
    // the emptiness test can run. That ordering is the whole of the behaviour.
    let cmd_str = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
    };
    let parts: Vec<String> = cmd_str.split_whitespace().map(String::from).collect();
    if parts.is_empty() {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Empty command".to_string(),
        }
        .into());
    }
    let env_val = args.get(2).copied().unwrap_or(Value::Object(None));
    let env = if matches!(env_val, Value::Object(None)) {
        None
    } else {
        Some(read_string_array(ctx, &env_val))
    };
    runtime_spawn_process(ctx, &parts, env.as_deref(), None)
}

/// Runtime.exec(String[], String[])
pub(crate) fn native_runtime_exec_array_env(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let arg_val = args.get(1).copied().unwrap_or(Value::Object(None));
    let cmd = checked_cmdarray(ctx, &arg_val)?;
    let env_val = args.get(2).copied().unwrap_or(Value::Object(None));
    let env = if matches!(env_val, Value::Object(None)) {
        None
    } else {
        Some(read_string_array(ctx, &env_val))
    };
    runtime_spawn_process(ctx, &cmd, env.as_deref(), None)
}

/// Runtime.exec(String, String[], File)
pub(crate) fn native_runtime_exec_string_env_dir(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // NULL is an NPE and EMPTY is an IllegalArgumentException -- two different
    // doors, and this VM had them swapped and missing respectively.
    //
    // MEASURED in COMPATIBLE mode against HotSpot 25.0.3+9
    // (`probes/Phase1Sweep.java`, the P1-I lane). Strict mode already agreed on
    // both rows, because it drops these `SyntheticStub`s and runs the JDK's own
    // `exec` -- so this is a defect the strict work exposed in the DEFAULT mode:
    //
    //   Runtime.exec("")     HotSpot IllegalArgumentException  CratonVM ArrayIndexOutOfBounds
    //   Runtime.exec(null)   HotSpot NullPointerException      CratonVM IllegalArgumentException
    //
    // The JDK's `exec(String, String[], File)` opens with
    // `if (command.isEmpty()) throw new IllegalArgumentException("Empty
    // command")`, so a null receiver NPEs on `isEmpty()` one instruction before
    // the emptiness test can run. That ordering is the whole of the behaviour.
    let cmd_str = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
    };
    let parts: Vec<String> = cmd_str.split_whitespace().map(String::from).collect();
    if parts.is_empty() {
        return Err(RuntimeError::IllegalArgumentException {
            message: "Empty command".to_string(),
        }
        .into());
    }
    let env_val = args.get(2).copied().unwrap_or(Value::Object(None));
    let env = if matches!(env_val, Value::Object(None)) {
        None
    } else {
        Some(read_string_array(ctx, &env_val))
    };
    let dir = exec_dir_path(ctx, args.get(3));
    runtime_spawn_process(ctx, &parts, env.as_deref(), dir.as_deref())
}

/// Runtime.exec(String[], String[], File)
pub(crate) fn native_runtime_exec_array_env_dir(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let arg_val = args.get(1).copied().unwrap_or(Value::Object(None));
    let cmd = checked_cmdarray(ctx, &arg_val)?;
    let env_val = args.get(2).copied().unwrap_or(Value::Object(None));
    let env = if matches!(env_val, Value::Object(None)) {
        None
    } else {
        Some(read_string_array(ctx, &env_val))
    };
    let dir = exec_dir_path(ctx, args.get(3));
    runtime_spawn_process(ctx, &cmd, env.as_deref(), dir.as_deref())
}

// ---------------------------------------------------------------------------
// System.getenv
// ---------------------------------------------------------------------------

pub(crate) fn native_system_getenv(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let key_ref = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        // `System.getenv(null)` reaches `ProcessEnvironment.getenv`, which
        // encodes the name before it looks anything up, so the JDK's answer is
        // that method's own helpful NPE rather than a null. Answering null made
        // a caller's bug look like an unset variable, which is the same failure
        // `System.getProperty(null)` was fixed for. The text is a site-specific
        // literal: both halves are fixed by `ProcessEnvironment`'s source.
        // MEASURED by `apps/probes/SystemRuntimeObjectSweep.java`.
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                message: Some(
                    "Cannot invoke \"String.getBytes(java.nio.charset.Charset)\" because \"str\" is null"
                        .to_string(),
                ),
            }
            .into())
        }
    };
    let key_str = ctx.read_string(key_ref).unwrap_or_default();
    match cratonvm_types::flags::runtime_var(&key_str) {
        Ok(val) => {
            let str_obj = ctx.create_string(&val);
            Ok(Some(Value::Object(Some(str_obj))))
        }
        Err(_) => Ok(Some(Value::Object(None))),
    }
}

// ---------------------------------------------------------------------------
// Process-global singletons for `System.getenv()` (no-arg) and
// `System.getProperties()`.
//
// HotSpot returns the SAME object on every call:
//   - `System.getenv()` в†’ the cached unmodifiable
//     `ProcessEnvironment.theUnmodifiableEnvironment` Map, and
//   - `System.getProperties()` в†’ the `System.props` singleton `Properties`.
// so `System.getenv() == System.getenv()` and
// `System.getProperties() == System.getProperties()` hold (Spring's
// `StandardEnvironmentTests.getSystemEnvironment` / `.getSystemProperties`
// assert this via `isSameAs`). CratonVM allocated a fresh object on every call,
// so identity failed (SC-env-classreading RC-A).
//
// Cache the built object the first time and return it thereafter. The cached
// `ObjectRef`s live ONLY in these process-global mutexes (a Rust side-table,
// invisible to the field/stack/static root scans), so they must be reported as
// GC roots and remapped after a moving collection вЂ” exactly like the singleton
// class loaders (`classloader::gc_scan_loader_singleton_roots`). The matching
// hooks are `gc_scan_system_singleton_roots` (wired into `roots.rs`) and
// `gc_update_system_singleton_refs` (wired into `gc.rs`); `reset_system_singletons`
// clears them when a new VM is created (mirrors `reset_loader_singletons`).
// ---------------------------------------------------------------------------
use cratonvm_native_api::vm_scoped::VmScoped;

/// One cell per VM, not per process. `reset_system_singletons` was correct
/// only while VMs were created strictly in sequence — a `Vm::new` on one test
/// thread wiped the cell another live VM was using, and the loser then read
/// back a `Map`/`Properties` object allocated in the other VM's heap. Keyed by
/// `vm_identity` and torn down from `release_vm_native_state`; see
/// `cratonvm_native_api::vm_scoped`.
static SYSTEM_ENV: VmScoped<Option<ObjectRef>> = VmScoped::new();
static SYSTEM_PROPS: VmScoped<Option<ObjectRef>> = VmScoped::new();

/// The cached no-arg `System.getenv()` Map singleton, if already built.
fn system_env_singleton(vm: usize) -> Option<ObjectRef> {
    SYSTEM_ENV.peek(vm, |cell| *cell).flatten()
}

/// Publish `obj` as the `System.getenv()` singleton (double-checked, like
/// [`set_system_props_singleton`]); returns the canonical singleton.
fn set_system_env_singleton(vm: usize, obj: ObjectRef) -> ObjectRef {
    SYSTEM_ENV.with(vm, |cell| match *cell {
        Some(existing) => existing,
        None => {
            *cell = Some(obj);
            obj
        }
    })
}

/// Return the OpenJDK-shaped read-only wrapper used by `System.getenv()`.
///
/// The backing object is the real-layout `java/util/HashMap` built below.
/// HotSpot exposes the no-arg environment as a
/// `java.util.Collections$UnmodifiableMap` whose private field `m` points at
/// that backing map. System Rules reflects on that field by name, so the
/// CratonVM unmodifiable-map stand-in deliberately keeps the backing in slot 0,
/// matching the JDK's `m` field slot.
///
/// # Why this asks `java.util.Collections` first (P1-B, 2026-08-12)
///
/// This used to go straight to `try_ensure_synthetic_class(
/// "cratonvm/internal/UnmodifiableMap", 2)`. That stand-in is a compatibility
/// fabrication, so under `--jdk-only` it is refused — correctly. What was wrong
/// was the *caller*: `System.getenv` is registered from
/// `register_essential_natives_with_shims`, so it survives strict mode, runs,
/// asks for a fabricated receiver, and dies as `NoClassDefFoundError:
/// cratonvm/internal/UnmodifiableMap` at the application's call site. Spring
/// takes that in `AbstractEnvironment.<init>`, before bean one.
///
/// The native already holds a REAL `java/util/HashMap`, so the real
/// `java.util.Collections.unmodifiableMap(Map)` can wrap it and there is no
/// need to fabricate anything. `vm_init.rs::ensure_bootstrap_compat_class`
/// claims these stand-ins "exist for the synthetic collection shims, which
/// strict mode does not register" — that premise is FALSE for
/// `UnmodifiableMap`, and this site was the counter-example.
///
/// Per mode, what the `ctx.invoke` below reaches:
///
/// * `--jdk-only`: `native-collections`' `Collections.unmodifiableMap`
///   registration is `NativeKind::SyntheticStub` and is dropped at
///   registration, so the **real `java.base` bytecode** runs and the result is a
///   genuine `java.util.Collections$UnmodifiableMap` — HotSpot's own answer.
/// * `--real-jdk` (default): that same registration wins and
///   `alloc_unmod_wrapper` allocates `cratonvm/internal/UnmodifiableMap` with
///   the backing at slot 0 — bit-for-bit what this function used to build
///   itself, so compatible mode is unchanged.
/// * `--synthetic-jdk`: `phases_early::register_collections_extras_natives`
///   binds the same triple to `native_return_first_arg` and runs later, so the
///   raw `HashMap` comes back. That is the mutable-map degradation below, and it
///   is a pre-existing property of that mode's identity binding
///   (`native-collections/src/lib.rs::wrap_unmodifiable`'s "vacuous-green trap"
///   note documents the same shape for `unmodifiableSet`), not something this
///   change introduces.
///
/// # Boot ordering — `allow_java_call`
///
/// Running Java bytecode from a native is only safe once the class library can
/// actually answer. The caller says whether that holds:
///
/// * The REAL-LAYOUT path passes `true`. It has already resolved every
///   `java/util/HashMap` and `java/util/HashMap$Node` field index off the real
///   classes, so `java.util.HashMap` is loaded and initialized by then, and
///   `java.util.Collections.<clinit>` — three `EMPTY_LIST`/`EMPTY_MAP`/
///   `EMPTY_SET` allocations — cannot need more than that, cannot do I/O, and
///   cannot re-enter `System.getenv()`.
/// * The LEGACY 3-field fallback passes `false`. That arm exists precisely
///   *because* the real `HashMap` layout was not resolvable, i.e. the class
///   library is not usable yet; and a real `Collections$UnmodifiableMap`
///   delegating to a 3-field synthetic map would be worse than the stand-in
///   whose native shims read slot 0.
///
/// # Why it no longer returns `Result`
///
/// If every wrapper is unavailable the answer is the **raw `HashMap`**: a
/// mutable map is far less wrong than an unloadable class, and it keeps
/// `System.getenv()` answering instead of killing the caller. The refusal is
/// still *recorded* — `ClassManager::try_ensure_synthetic_class` records the
/// `CompatibilityClassRequested` violation before returning `Err`, so the
/// `--jdk-only-report` census still sees it; only the throw is dropped.
fn wrap_system_env_map(
    ctx: &mut dyn NativeContext,
    map: ObjectRef,
    allow_java_call: bool,
) -> ObjectRef {
    let pin = ctx.pin_native_root(map);

    // 1. The real `java.util.Collections.unmodifiableMap(Map)`.
    if allow_java_call
        && ctx
            .ensure_class_initialized("java/util/Collections")
            .is_ok()
    {
        let backing = ctx.read_native_pin(pin, map);
        // A failure here is deliberately swallowed rather than propagated: it
        // means the real wrapper is unavailable, which is what steps 2 and 3
        // are for. Same precedent as
        // `jca::provider_chain::wrap_unmodifiable`, which discards a failed
        // `Collections.unmodifiableSet` and returns the plain set.
        if let Ok(Some(Value::Object(Some(view)))) = ctx.invoke(
            "java/util/Collections",
            "unmodifiableMap",
            "(Ljava/util/Map;)Ljava/util/Map;",
            &[Value::Object(Some(backing))],
        ) {
            ctx.unpin_native_roots(pin);
            return view;
        }
    }

    // 2. The compatibility stand-in. Refused under `--jdk-only`, which is the
    //    whole point of the mode; `try_` rather than the infallible spelling so
    //    the refusal is a value and not a fabrication.
    let stand_in = ctx.try_ensure_synthetic_class("cratonvm/internal/UnmodifiableMap", 2);
    if let Ok(wrapper_class) = stand_in {
        let map = ctx.read_native_pin(pin, map);
        let wrapper = ctx.alloc_object(wrapper_class, 2);
        let map = ctx.read_native_pin(pin, map);
        ctx.set_field(wrapper, 0, Value::Object(Some(map)));
        ctx.unpin_native_roots(pin);
        return wrapper;
    }

    // 3. Degrade to the backing map itself. Mutable where HotSpot's is not —
    //    say so in the log rather than letting it pass silently — but a real
    //    `java.util.HashMap` that every caller can read.
    tracing::warn!(
        "System.getenv(): neither java.util.Collections.unmodifiableMap nor the \
         cratonvm/internal/UnmodifiableMap stand-in was available; returning the \
         backing HashMap, which is MUTABLE unlike HotSpot's"
    );
    let map = ctx.read_native_pin(pin, map);
    ctx.unpin_native_roots(pin);
    map
}

/// The cached `System.getProperties()` `Properties` singleton, if already built.
pub fn system_props_singleton(vm_identity: usize) -> Option<ObjectRef> {
    SYSTEM_PROPS.peek(vm_identity, |cell| *cell).flatten()
}

/// Publish `obj` as the `System.getProperties()` singleton, unless another
/// thread already won the race вЂ” in which case the existing one is returned and
/// `obj` is discarded (it becomes unreachable and is collected). Returns the
/// canonical singleton so all callers converge on one identity.
pub fn set_system_props_singleton(vm_identity: usize, obj: ObjectRef) -> ObjectRef {
    SYSTEM_PROPS.with(vm_identity, |cell| match *cell {
        Some(existing) => existing,
        None => {
            *cell = Some(obj);
            obj
        }
    })
}

/// Replace the cached `System.getProperties()` singleton. Used by
/// `System.setProperties(Properties)`, which HotSpot implements as a global
/// swap of `System.props`, not as a mutation of the previous Properties object.
/// Returns the previous singleton so callers can drop any system-props marker.
pub fn replace_system_props_singleton(
    vm_identity: usize,
    obj: Option<ObjectRef>,
) -> Option<ObjectRef> {
    SYSTEM_PROPS.with(vm_identity, |cell| std::mem::replace(cell, obj))
}

/// GC root scan for the `System.getenv()` / `System.getProperties()` singletons
/// (companion to [`gc_update_system_singleton_refs`]). Mirrors
/// `classloader::gc_scan_loader_singleton_roots`.
pub fn gc_scan_system_singleton_roots(vm_identity: usize, out: &mut Vec<ObjectRef>) {
    out.extend(system_env_singleton(vm_identity));
    out.extend(system_props_singleton(vm_identity));
}

/// Post-GC remap for the system singletons (companion to
/// [`gc_scan_system_singleton_roots`]). Repoints the cached `ObjectRef`s to
/// their relocated addresses after a moving collection.
pub fn gc_update_system_singleton_refs(
    vm_identity: usize,
    pointer_map: &cratonvm_types::PointerMap,
) {
    if pointer_map.is_empty() {
        return;
    }
    let remap = |slot: &mut Option<ObjectRef>| {
        if let Some(obj_ref) = slot.as_mut() {
            let old_addr = obj_ref.as_ptr() as usize;
            if let Some(&new_addr) = pointer_map.get(&old_addr) {
                debug_assert!(new_addr != 0, "GC pointer map contains null address");
                *obj_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    };
    SYSTEM_ENV.with(vm_identity, remap);
    SYSTEM_PROPS.with(vm_identity, remap);
}

/// Per-VM teardown for the cached system singletons. Called from
/// `release_vm_native_state`.
///
/// This replaces the old `reset_system_singletons()`, which `Vm::new` called
/// to clear a *previous* VM's leftovers. That only worked while VMs were
/// created in sequence: with two live at once it wiped a running VM's cell.
/// A fresh `vm_identity` starts with no row, so there is nothing to reset at
/// construction time any more.
pub fn forget_vm_system_singletons(vm_identity: usize) {
    SYSTEM_ENV.forget(vm_identity);
    SYSTEM_PROPS.forget(vm_identity);
    // Same reason, different table: a `vm_identity` is reused, and a second VM
    // inheriting the first's loaded-library rows would refuse a load the first
    // VM made — an `UnsatisfiedLinkError` naming a loader that no longer
    // exists. See `LOADED_LIBRARIES`.
    LOADED_LIBRARIES.forget(vm_identity);
    // The `System.initPhase1` property snapshot. A reused `vm_identity` must
    // start with no row rather than inherit a previous VM's boot-time values
    // — see `SAVED_SYSTEM_PROPERTIES`.
    SAVED_SYSTEM_PROPERTIES.forget(vm_identity);
    // SBR5 sysprops: the merged-property baseline is per VM too.
    crate::sysprops_merge::forget_vm(vm_identity);
    // gc-common w16-e: the shutdown-hook registry and the `Runtime.version()`
    // singleton handle. Both hold slot numbers in this VM's global-root
    // table, which dies with it.
    SHUTDOWN_STATE.forget(vm_identity);
    RUNTIME_VERSION_ROOT.forget(vm_identity);
    // gci i1/wr: the logging pre-warm arming, per VM since this round.
    LOGGING_PREWARM_ARMED.forget(vm_identity);
}

/// Rust-side twin of `jdk.internal.misc.VM.savedProps`: the system
/// properties as they stood at `System.initPhase1`, captured once per VM so
/// native callers can answer the way `jdk.internal.util.StaticProperty`
/// does — from the boot-time snapshot, not the live store a later
/// `System.setProperty` writes into. See [`saved_system_property`] and
/// `docs/feature-designs/native-dispatch-architecture.md`.
static SAVED_SYSTEM_PROPERTIES: VmScoped<std::collections::HashMap<String, String>> =
    VmScoped::new();

/// Take the boot-time snapshot [`SAVED_SYSTEM_PROPERTIES`] serves. Called
/// from `native_system_init_phase1`'s Step 4 — the shared body every
/// registration branch (`--jdk-only`, `--real-jdk`, plain `Compatible`)
/// runs unconditionally — which is what makes this different from
/// `publish_vm_saved_props`: that one is skipped in plain `Compatible`
/// mode (no need to build a live `java.util.HashMap` there, since real
/// bytecode never reads `VM.savedProps` in that mode), but a *native*
/// caller in exactly that mode is why this Rust-side memo exists.
///
/// Whole-map replace, not merge: a second boot of this process (a fresh
/// `vm_identity` reusing the row, as the test harnesses do) must not see a
/// stale key from a previous VM's snapshot survive under a new one.
fn capture_saved_system_properties(ctx: &mut dyn NativeContext) {
    let entries = ctx.list_system_properties();
    if entries.is_empty() {
        return;
    }
    let vm = ctx.vm_identity();
    SAVED_SYSTEM_PROPERTIES.with(vm, |map| {
        map.clear();
        map.extend(entries);
    });
}

/// Read `key` out of the real `jdk/internal/misc/VM.savedProps` map, if that
/// static field is already a populated `Map`.
///
/// **Why this has to be checked at all**, and checked FIRST: under
/// `--real-jdk`, `System.initPhase1()`'s own real bytecode runs rather than
/// [`native_system_init_phase1`] -- `register_inner`'s
/// `drop_real_layout_synthetic && class_name == "java/lang/System" &&
/// matches!(method_name, "initPhase1" | "initPhase2" | "initPhase3")` arm
/// deliberately leaves those three uninter­cepted (see
/// `native_vm_save_properties`'s doc comment) so the real bootstrap sequence
/// runs, backed by narrower natives. That means
/// [`capture_saved_system_properties`] -- reached only from inside
/// `native_system_init_phase1` -- never runs under `--real-jdk`, and its
/// Rust-side memo stays empty for the whole life of that VM. But the real
/// bytecode's own call to `VM.saveProperties(props)` IS intercepted (that is
/// what `native_vm_save_properties` is for), so `VM.savedProps` is a real,
/// correctly-populated `Map` by the time any application code runs. MEASURED
/// 2026-09-21: without this, `saved_system_property` fell through to the
/// live store under `--real-jdk` and the `java.io.tmpdir` fix below did
/// nothing in the one mode the defect was found in.
fn read_saved_props_map(ctx: &mut dyn NativeContext, key: &str) -> Option<String> {
    let cid = ctx.class_id_by_name("jdk/internal/misc/VM")?;
    let idx = ctx.static_field_index_by_name(cid, "savedProps")?;
    let Value::Object(Some(map)) = ctx.get_static_field(cid, idx) else {
        return None;
    };
    let k = ctx.create_string(key);
    match ctx.invoke_virtual(
        map,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(k))],
    ) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
        _ => None,
    }
}

/// Read `key` the way `jdk.internal.util.StaticProperty` does: a boot-time
/// snapshot, immune to a LATER `System.setProperty` call the way
/// `ctx.get_system_property` (the live store) is not. Two sources feed it,
/// tried in order, because which one is populated depends on the mode:
///
/// 1. [`read_saved_props_map`] — the real `VM.savedProps` `Map`, populated by
///    real `initPhase1` bytecode under `--real-jdk`.
/// 2. [`SAVED_SYSTEM_PROPERTIES`] — the Rust-side twin
///    [`capture_saved_system_properties`] took, populated by *this* native
///    running instead of real bytecode in plain `Compatible` mode.
///
/// Falls back to the live store when neither source has `key` — taken before
/// `System.initPhase1` ran, or a `vm_identity` this process never booted
/// through it at all (the inline unit-test harnesses that build a bare
/// `SharedVm` and never call `initPhase1`).
///
/// This is the fix for `java.io.tmpdir` being read live in compatible mode:
/// `jdk_temp_dir`'s callers used to read `ctx.get_system_property("java.io.tmpdir")`
/// directly, which is the live store and moves on every `System.setProperty`
/// call — a runtime write HotSpot's own `File$TempDirectory` never sees,
/// because it reads `StaticProperty.javaIoTmpDir()`, itself sourced from
/// `VM.savedProps`. Companion to `NativeContext::captured_line_separator`,
/// which solves the identical live-vs-snapshot split for `line.separator` by
/// reading back a Java static field real bytecode always sets; `java.io.tmpdir`
/// (and the rest of `StaticProperty`) has no field of its own to piggyback
/// on, so this reads `VM.savedProps` directly, with the Rust memo as a
/// second source for the one mode that never populates it.
pub(crate) fn saved_system_property(ctx: &mut dyn NativeContext, key: &str) -> Option<String> {
    if let Some(v) = read_saved_props_map(ctx, key) {
        return Some(v);
    }
    if let Some(v) = SAVED_SYSTEM_PROPERTIES
        .peek(ctx.vm_identity(), |m| m.get(key).cloned())
        .flatten()
    {
        return Some(v);
    }
    ctx.get_system_property(key)
}

/// The boot-time system properties as the Rust-side snapshot has them
/// (`--compatible`, whose native `initPhase1` took it); empty under
/// `--jdk-only`, where the JDK's own `initPhase1` ran and `VM.savedProps` is the
/// record.
pub(crate) fn boot_property_entries(ctx: &mut dyn NativeContext) -> Vec<(String, String)> {
    let mut entries: Vec<(String, String)> = SAVED_SYSTEM_PROPERTIES
        .peek(ctx.vm_identity(), |m| {
            m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
        })
        .unwrap_or_default();
    if entries.is_empty() {
        // `--compatible` runs the JDK's own `System.initPhase1` too (the
        // native one never ran, so no Rust-side snapshot): `VM.savedProps` is
        // the record.
        entries = saved_props_map_entries(ctx);
    }
    entries.sort();
    entries
}

/// The `String -> String` entries of the real `jdk.internal.misc.VM.savedProps`
/// map; empty when it is not a populated `Map`. Every object read across a Java
/// call is pinned.
fn saved_props_map_entries(ctx: &mut dyn NativeContext) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(vm_cid) = ctx.class_id_by_name("jdk/internal/misc/VM") else {
        return out;
    };
    let Some(idx) = ctx.static_field_index_by_name(vm_cid, "savedProps") else {
        return out;
    };
    let Value::Object(Some(map)) = ctx.get_static_field(vm_cid, idx) else {
        return out;
    };
    let map_pin = ctx.pin_native_root(map);
    let keys = match ctx.invoke_virtual(map, "keySet", "()Ljava/util/Set;", &[]) {
        Ok(Some(Value::Object(Some(set)))) => {
            ctx.invoke_virtual(set, "toArray", "()[Ljava/lang/Object;", &[])
        }
        _ => {
            ctx.unpin_native_roots(map_pin);
            return out;
        }
    };
    let Ok(Some(Value::Object(Some(keys)))) = keys else {
        ctx.unpin_native_roots(map_pin);
        return out;
    };
    let keys_pin = ctx.pin_native_root(keys);
    let len = ctx.array_length(keys);
    for i in 0..len {
        let keys_now = ctx.read_native_pin(keys_pin, keys);
        let Value::Object(Some(key)) = ctx.get_array_element(keys_now, i) else {
            continue;
        };
        let Some(key_text) = ctx.read_string(key) else {
            continue;
        };
        let map_now = ctx.read_native_pin(map_pin, map);
        if let Ok(Some(Value::Object(Some(value)))) = ctx.invoke_virtual(
            map_now,
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(key))],
        ) {
            if let Some(value_text) = ctx.read_string(value) {
                out.push((key_text, value_text));
            }
        }
    }
    ctx.unpin_native_roots(map_pin);
    out
}

/// A fresh `java.util.Properties` holding the boot-time system properties, for
/// `System.setProperties(null)` (SBR3 `refmisc`): HotSpot's
/// `System.setProperties(null)` installs `createProperties(SystemProps
/// .initProperties())`, a new object with the properties the VM booted with,
/// not the previous object and not an empty set. Only where the JDK bodies own
/// `System.props` (`--jdk-only`, or the `--compatible` `jdk-concurrent-hash-map`
/// row: `sysprops_merge::jdk_bodies_own_system_props`): the object
/// is built from the real `VM.savedProps` (the map `System.initPhase1` saved);
/// `None` anywhere else or when it cannot be built, and the caller then keeps
/// its previous behaviour. Every reference is re-read through a pin.
pub(crate) fn fresh_boot_properties(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    if !crate::sysprops_merge::jdk_bodies_own_system_props(&*ctx) {
        return None;
    }
    let vm_cid = ctx.class_id_by_name("jdk/internal/misc/VM")?;
    let idx = ctx.static_field_index_by_name(vm_cid, "savedProps")?;
    let Value::Object(Some(saved)) = ctx.get_static_field(vm_cid, idx) else {
        return None;
    };
    let saved_pin = ctx.pin_native_root(saved);
    let props = match ctx.new_object_initialized("java/util/Properties", "()V", &[]) {
        Ok(Some(Value::Object(Some(p)))) => p,
        _ => {
            ctx.unpin_native_roots(saved_pin);
            return None;
        }
    };
    let props_pin = ctx.pin_native_root(props);
    let saved = ctx.read_native_pin(saved_pin, saved);
    let props_now = ctx.read_native_pin(props_pin, props);
    let put = ctx.invoke_virtual(
        props_now,
        "putAll",
        "(Ljava/util/Map;)V",
        &[Value::Object(Some(saved))],
    );
    let out = ctx.read_native_pin(props_pin, props);
    ctx.unpin_native_roots(saved_pin);
    put.ok().map(|_| out)
}

/// Record that the BOOT loader holds `name`, so a later app-loader
/// `System.loadLibrary(name)` gets the JDK's "already loaded in another
/// classloader" error instead of a success.
///
/// **THIS HAS NO CALLER IN THE TREE. It is a written-down hand-off, not a live
/// path — do not read its presence as the feature being on.** The one call site
/// it is for is the `jdk/internal/loader/BootLoader.loadLibrary` registration in
/// `native-builtins/src/lib.rs`, which is a deliberate `|_ctx, _args| Ok(None)`
/// no-op (that short-circuit is what stops real `NativeLibraries` bytecode from
/// blocking on the JDK's native-library lock during Linux boot-class `<clinit>`,
/// so it must stay a no-op *for the load*; only the bookkeeping is missing).
/// The patch is one line inside that closure — `BootLoader.loadLibrary(String)`
/// is static, so `args[0]` is the name:
///
/// ```ignore
/// |ctx, args| {
///     if let Some(Value::Object(Some(o))) = args.first() {
///         let name = ctx.read_string(*o).unwrap_or_default();
///         crate::lang_system::record_boot_loader_library(ctx, &name);
///     }
///     Ok(None)
/// }
/// ```
///
/// UNARMED ON PURPOSE, and the reason is measurable rather than cautious:
/// arming it makes `System.loadLibrary("net")` throw for any program that has
/// already reached a `java.net` boot class, which is exactly HotSpot's answer
/// and exactly what `RJdkJni.libraryLoading` depends on NOT happening — its
/// `zip` probe falls through to a `net` probe that must succeed
/// (`RJdkJni.java:189-202`), and `run.sh` compares `CK` lines. Whether CratonVM
/// reaches `BootLoader.loadLibrary("net")` before that line cannot be settled
/// from source; it is one A/B on a built binary in both modes with the HotSpot
/// oracle beside it. Nothing here has been built or run.
///
/// Recording is inert under `Compatible` regardless: nothing reads
/// `LOADED_LIBRARIES` unless a `LoaderScoping::On` registration is in force,
/// and only the strict arm installs one.
pub fn record_boot_loader_library(ctx: &mut dyn NativeContext, name: &str) {
    if name.is_empty() {
        return;
    }
    // Loader id 0 is the bootstrap loader — `NativeContext::loader_id_of_class`'s
    // own encoding, and the loader `BootLoader.loadLibrary` loads on behalf of
    // by definition. Any existing owner is left alone: this native returns void
    // and reports nothing, so there is no shape in which to raise the conflict.
    let _existing_owner = claim_library(ctx.vm_identity(), 0, name);
}

pub(crate) fn native_system_getenv_all(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    use cratonvm_types::ClassId;

    // Identity: return the cached singleton so `System.getenv() ==
    // System.getenv()` holds (SC-env-classreading RC-A). The process
    // environment is immutable for a running JVM, so the cached snapshot stays
    // correct. Only the real-layout path below caches; the legacy 3-field
    // fallback still returns the OpenJDK-shaped unmodifiable wrapper but is left
    // uncached so a later call retries once the real `java/util/HashMap` layout
    // is resolvable.
    if let Some(cached) = system_env_singleton(ctx.vm_identity()) {
        return Ok(Some(Value::Object(Some(cached))));
    }

    // Build a HashMap with all environment variables.
    //
    // S111r7: previously this routine allocated `java/util/HashMap` with only
    // 3 fields and stored `(buckets, size, capacity)` at slot indices 0/1/2,
    // matching the synthetic layout used by `native-collections::native_map_*`.
    // That works while every Map operation routes through a registered native,
    // but `SystemEnvironmentPropertySource.containsKey` (Spring core) reaches
    // the bytecode interpreter for `HashMap.containsKey -> getNode`, where
    // `getfield #105 // table:[Ljava/util/HashMap$Node;` resolves to the real
    // HashMap layout's slot for `table` вЂ” slot 2 in JDK 25 (AbstractMap
    // inherits `keySet` (0) and `values` (1); HashMap declares `table` next).
    // Reading slot 2 returned `Int(16)` (our synthetic CAPACITY value), then
    // `arraylength` on `Int(16)` produced
    //   `internal error: expected object reference, got int(16)`.
    //
    // Fix: resolve the real HashMap / HashMap$Node field slot indices via
    // `resolve_field_index`, allocate enough field slots to cover the real
    // layout, and populate the object so the interpreter's bytecode getfield
    // sees correct values. Falls back to the legacy synthetic-3-field layout
    // if the real class wasn't loaded (e.g. running before bootstrap completes
    // or against a synthetic stub).
    let hashmap_class_id = ctx
        .ensure_class_initialized("java/util/HashMap")
        .unwrap_or(ClassId::new(0));
    // Best-effort: also load the Node class so its real field layout is known.
    let _ = ctx.ensure_class_initialized("java/util/HashMap$Node");

    // Real-layout slot resolution. Each `Some(idx)` means we know the
    // bytecode interpreter will read that field at `idx`; if every required
    // field is resolvable AND the resolved indices are mutually consistent
    // (no aliasing), we use the real layout; otherwise fall back.
    let f_table = ctx.resolve_field_index("java/util/HashMap", "table");
    let f_size = ctx.resolve_field_index("java/util/HashMap", "size");
    let f_threshold = ctx.resolve_field_index("java/util/HashMap", "threshold");
    let f_loadfactor = ctx.resolve_field_index("java/util/HashMap", "loadFactor");
    let f_entryset = ctx.resolve_field_index("java/util/HashMap", "entrySet");
    let n_hash = ctx.resolve_field_index("java/util/HashMap$Node", "hash");
    let n_key = ctx.resolve_field_index("java/util/HashMap$Node", "key");
    let n_value = ctx.resolve_field_index("java/util/HashMap$Node", "value");
    let n_next = ctx.resolve_field_index("java/util/HashMap$Node", "next");

    let cap = 16usize;
    // Everything from here to the end of the function allocates repeatedly —
    // two strings and a node per environment variable — while holding the
    // bucket array, the map and each freshly built node. Those are all young
    // objects, so their addresses are only valid until the next allocation:
    // hold them in the scope and re-read at every store.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let buckets_obj = scope.new_ref_array(ClassId::new(0), cap);
    let buckets_h = scope.root(buckets_obj);

    // JDK HashMap.hash: (h = key.hashCode()) ^ (h >>> 16). For Strings,
    // hashCode = sum of 31*h + ch. Then bucket index is (n-1) & hash for
    // power-of-two capacity (16 here).
    fn jdk_string_hash(s: &str) -> i32 {
        let mut h: i32 = 0;
        // String.hashCode is per-char (UTF-16 code unit). For ASCII env vars
        // this is identical to per-byte; for non-ASCII fall back to chars.
        for ch in s.chars() {
            h = h.wrapping_mul(31).wrapping_add(ch as i32);
        }
        h ^ ((h as u32 >> 16) as i32)
    }

    if let (
        Some(f_table),
        Some(f_size),
        Some(f_threshold),
        Some(f_loadfactor),
        Some(f_entryset),
        Some(n_hash),
        Some(n_key),
        Some(n_value),
        Some(n_next),
    ) = (
        f_table,
        f_size,
        f_threshold,
        f_loadfactor,
        f_entryset,
        n_hash,
        n_key,
        n_value,
        n_next,
    ) {
        // Allocate enough slots to cover the real layout.
        let map_n_fields = [f_table, f_size, f_threshold, f_loadfactor, f_entryset]
            .iter()
            .copied()
            .max()
            .unwrap_or(0)
            + 1;
        let node_n_fields = [n_hash, n_key, n_value, n_next]
            .iter()
            .copied()
            .max()
            .unwrap_or(0)
            + 1;
        let map_obj = scope.alloc_object(hashmap_class_id, map_n_fields);
        let map_h = scope.root(map_obj);
        let map = scope.get(&map_h);
        let buckets = scope.get(&buckets_h);
        scope.set_field(map, f_table, Value::Object(Some(buckets)));
        scope.set_field(map, f_size, Value::Int(0));
        // threshold = (int)(capacity * 0.75) for the default load factor.
        scope.set_field(map, f_threshold, Value::Int((cap as i32 * 3) / 4));
        scope.set_field(map, f_loadfactor, Value::Float(0.75));
        scope.set_field(map, f_entryset, Value::Object(None));

        // Fallible since 2026-08-10 (JDK-only wave 2, step 3). This arm is the
        // REAL-layout path — every field index above came from the real
        // `java.util.HashMap`/`HashMap$Node` — so `HashMap$Node` is already
        // loaded here and the ask resolves to the real class rather than
        // fabricating. The refusal only fires on an image where it is not.
        let node_class_id = crate::util_concurrent_ext::refused_class(
            &mut *scope,
            "java/util/HashMap$Node",
            node_n_fields,
        )?;

        for (key, value) in std::env::vars() {
            let key_str = scope.create_string(&key);
            let key_h = scope.root(key_str);
            let val_str = scope.create_string(&value);
            let val_h = scope.root(val_str);
            let hash = jdk_string_hash(&key);
            // (n-1) & hash, since cap=16 is power of two.
            let idx = ((cap as u32 - 1) & hash as u32) as usize;
            let node = scope.alloc_object(node_class_id, node_n_fields);
            // Nothing below allocates, so one read of each address is enough —
            // but it has to come AFTER the last allocation, not before it.
            let key_obj = scope.get(&key_h);
            let val_obj = scope.get(&val_h);
            let buckets = scope.get(&buckets_h);
            let map = scope.get(&map_h);
            scope.set_field(node, n_hash, Value::Int(hash));
            scope.set_field(node, n_key, Value::Object(Some(key_obj)));
            scope.set_field(node, n_value, Value::Object(Some(val_obj)));
            let existing = scope.get_array_element(buckets, idx);
            scope.set_field(node, n_next, existing);
            scope.set_array_element(buckets, idx, Value::Object(Some(node)));

            let old_size = match scope.get_field(map, f_size) {
                Value::Int(s) => s,
                _ => 0,
            };
            scope.set_field(map, f_size, Value::Int(old_size + 1));
        }

        // Cache the OpenJDK-shaped process-wide singleton (double-checked
        // publish). The wrapper's field 0 is the private `m` backing field that
        // libraries such as System Rules reach via reflection.
        //
        // `true`: this is the real-layout arm, so every `java/util/HashMap`
        // field index above came off the real class and running
        // `java.util.Collections` bytecode here is safe. See
        // `wrap_system_env_map`'s boot-ordering note.
        let map = scope.get(&map_h);
        let env = wrap_system_env_map(&mut *scope, map, true);
        let env = set_system_env_singleton(scope.vm_identity(), env);
        return Ok(Some(Value::Object(Some(env))));
    }

    // Legacy fallback: synthetic 3-field layout for environments where
    // the real HashMap class hierarchy isn't fully resolvable.
    // A failed real-class initialization yields ClassId(0), whose Object layout
    // has zero slots. This fallback writes map and node fields, so both need
    // named synthetic layouts even when the real classes cannot initialize.
    //
    // Fallible since 2026-08-10 (JDK-only wave 2, step 3): a 3-field synthetic
    // `java/util/HashMap` IS the compatibility substitution — the comment above
    // says as much — so under `--jdk-only` it is refused and `System.getenv()`
    // raises `NoClassDefFoundError` naming the class instead of handing back a
    // map whose layout `java.base`'s own `HashMap` bytecode cannot read.
    let fallback_map_class_id =
        crate::util_concurrent_ext::refused_class(&mut *scope, "java/util/HashMap", 3)?;
    let fallback_node_class_id =
        crate::util_concurrent_ext::refused_class(&mut *scope, "java/util/HashMap$Node", 4)?;
    let map_obj = scope.alloc_object(fallback_map_class_id, 3); // MAP_NUM_FIELDS = 3
    let map_h = scope.root(map_obj);
    let map = scope.get(&map_h);
    let buckets = scope.get(&buckets_h);
    scope.set_field(map, 0, Value::Object(Some(buckets))); // MAP_FIELD_BUCKETS
    scope.set_field(map, 1, Value::Int(0)); // MAP_FIELD_SIZE
    scope.set_field(map, 2, Value::Int(cap as i32)); // MAP_FIELD_CAPACITY

    for (key, value) in std::env::vars() {
        let key_str = scope.create_string(&key);
        let key_h = scope.root(key_str);
        let val_str = scope.create_string(&value);
        let val_h = scope.root(val_str);
        let hash = jdk_string_hash(&key);
        let idx = ((cap as u32 - 1) & hash as u32) as usize;
        let node = scope.alloc_object(fallback_node_class_id, 4); // hash, key, value, next
        let key_obj = scope.get(&key_h);
        let val_obj = scope.get(&val_h);
        let buckets = scope.get(&buckets_h);
        let map = scope.get(&map_h);
        scope.set_field(node, 0, Value::Int(hash));
        scope.set_field(node, 1, Value::Object(Some(key_obj)));
        scope.set_field(node, 2, Value::Object(Some(val_obj)));

        let existing = scope.get_array_element(buckets, idx);
        scope.set_field(node, 3, existing); // next = existing bucket head
        scope.set_array_element(buckets, idx, Value::Object(Some(node)));

        let old_size = match scope.get_field(map, 1) {
            Value::Int(s) => s,
            _ => 0,
        };
        scope.set_field(map, 1, Value::Int(old_size + 1));
    }

    // `false`: this arm was reached BECAUSE the real `java/util/HashMap` layout
    // was not resolvable, so the class library cannot be asked to run
    // `java.util.Collections.unmodifiableMap` — and a real wrapper delegating to
    // a 3-field synthetic map would be worse than the stand-in whose shims read
    // slot 0. Uncached, as before, so a later call retries the real path.
    let map = scope.get(&map_h);
    let env = wrap_system_env_map(&mut *scope, map, false);
    Ok(Some(Value::Object(Some(env))))
}

pub(crate) fn native_pb_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
    Ok(None)
}

pub(crate) fn native_pb_command(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field(this, 0)))
}

// `native_pb_start` lived here: a `ProcessBuilder.start` that ran
// `check_exec_or_throw` and then handed back a one-slot dummy Process which had
// spawned nothing. Its own comment called it "the simplified
// ProcessBuilder.start stub that never actually spawns", kept as
// defense-in-depth for the SecurityManager gate. That gate now lives in
// `native-io`'s `spawn_and_wrap`, where every spawn route meets it, so the stub
// had nothing left to defend -- and its dummy Process was the same
// allocate-under-a-real-JDK-class-name truncation as the rest of this cluster.

/// The component `ClassId` of a `StackTraceElement[]`. `Object` only when the
/// class is not loaded (or loaded under two loaders), the pre-i6 behaviour.
pub(crate) fn stack_trace_element_component_id(
    ctx: &mut dyn NativeContext,
) -> cratonvm_types::ClassId {
    crate::lang_class::reflection_component_id(ctx, "java/lang/StackTraceElement")
}

/// The `ClassId` of `StackTraceElement[]` itself, the component of
/// `Thread.dumpThreads`' `StackTraceElement[][]` (i7-L2). Loads the array
/// class (and so `StackTraceElement`) on a miss, so it can run Java code and
/// collect: call it only while no raw reference is live. `Object` when the
/// class cannot be had, the pre-i7 behaviour.
pub(crate) fn stack_trace_element_array_class_id(
    ctx: &mut dyn NativeContext,
) -> cratonvm_types::ClassId {
    const NAME: &str = "[Ljava/lang/StackTraceElement;";
    if let Some(id) = ctx.class_id_by_name(NAME) {
        return id;
    }
    if let Ok(Some(Value::Object(Some(mirror)))) = ctx.load_class(NAME) {
        if let Some(id) = ctx.class_id_from_mirror(mirror) {
            return id;
        }
    }
    ctx.class_id_by_name(NAME)
        .unwrap_or_else(|| cratonvm_types::ClassId::new(0))
}

/// Materialize a `java/lang/StackTraceElement[]` from a captured frame trace
/// (innermost frame first, as `getStackTrace()` expects index 0 = current
/// call). Shared by `Thread.getStackTrace0()` and `Thread.dumpThreads()`.
pub(crate) fn build_stack_trace_element_array(
    ctx: &mut dyn NativeContext,
    trace: &[cratonvm_native_api::StackTraceEntry],
) -> Result<cratonvm_types::ObjectRef, MethodCallFailed> {
    // One element, three strings and possibly a mirror are allocated per frame,
    // so neither the array nor the element can be carried as a raw address
    // across the body of the loop.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    // `StackTraceElement[]`, not `Object[]`: `Thread.asyncGetStackTrace` casts
    // `getStackTrace0()`'s result to `[Ljava/lang/StackTraceElement;`.
    let component = stack_trace_element_component_id(&mut *scope);
    let arr_obj = scope.new_ref_array(component, trace.len());
    let arr_h = scope.root(arr_obj);
    // The captured trace is outermost-first (frame[0] = bottom of stack);
    // getStackTrace()/getAllStackTraces() want index 0 = the innermost (current)
    // call, so materialize reversed вЂ” matching HotSpot ordering.
    for (i, e) in trace.iter().rev().enumerate() {
        let ste =
            crate::try_alloc_concurrent_synthetic(&mut *scope, "java/lang/StackTraceElement", 4)?;
        let ste_h = scope.root(ste);
        let cls_dotted = match scope.class_id_by_name(&e.class_name) {
            Some(cid) => {
                crate::lang_class::dotted_class_name(scope.vm_identity(), cid, &e.class_name)
            }
            // A hidden class keeps its `/0x…` suffix (wave 35).
            None => std::sync::Arc::from(crate::lang_class::dotted_binary_name(&e.class_name)),
        };
        crate::lang_misc::fill_stack_trace_element(
            &mut *scope,
            ste,
            &e.class_name,
            &cls_dotted,
            &e.method_name,
            e.source_file.as_deref(),
            e.line_number,
        );
        let arr = scope.get(&arr_h);
        let ste = scope.get(&ste_h);
        scope.set_array_element(arr, i, Value::Object(Some(ste)));
    }
    Ok(scope.get(&arr_h))
}

/// `Thread.getStackTrace0()` вЂ” the live stack of the receiver thread (or, for
/// another thread, its last-published blocking-deposit snapshot). Returns a
/// `StackTraceElement[]`. Previously stubbed to an empty array.
pub(crate) fn native_thread_get_stack_trace(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            let component = stack_trace_element_component_id(ctx);
            let arr = ctx.new_ref_array(component, 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let trace = ctx.thread_stack_trace(this);
    // `Thread.getStackTrace()` leaves hidden frames out, as HotSpot's
    // `asyncGetStackTrace` does (`--jdk-only`, interpreter round i1 wave 35;
    // `NativeContext::hidden_stack_entries`). `dumpThreads`
    // (`getAllStackTraces`) keeps them, as HotSpot's does: measured,
    // `L4W35OtherThreadHidden`.
    let hidden = ctx.hidden_stack_entries(&trace);
    let mut trace: Vec<_> = if hidden.iter().any(|h| *h) {
        trace
            .into_iter()
            .zip(hidden)
            .filter_map(|(entry, hidden)| (!hidden).then_some(entry))
            .collect()
    } else {
        trace
    };
    // Interpreter round i4 wave 92, lane L2: under `-XX:+ShowHiddenFrames`
    // the listing above keeps every hidden frame that exists, and HotSpot
    // lists also the ones this VM never pushed (a lambda proxy's SAM frame,
    // a served `MethodHandle` call's `LambdaForm` frames): the walkers'
    // stand-ins, spliced INNERMOST first as a walk reads them. Before the
    // depth cap, which counts them as HotSpot does.
    if ctx.show_hidden_frames_flag() && trace.len() >= 2 {
        let innermost_first: Vec<_> = trace.into_iter().rev().collect();
        let (mut spliced, _) =
            crate::lang_stackwalker::add_hidden_standin_frames(ctx, innermost_first, |_| true);
        spliced.reverse();
        trace = spliced;
    }
    // HotSpot's `GetStackTraceHandshakeClosure` lists at most
    // `MaxJavaStackTraceDepth` frames (default 1024, `0` = all), the innermost
    // ones, counted after the hidden-frame skip; this listed every frame
    // (interpreter round i4 wave 84, lane L2). `trace` is OUTERMOST-first.
    // `dumpThreads` (`getAllStackTraces`) dumps the whole stack, as HotSpot's
    // `VM_ThreadDump` with `max_depth == -1` does, and is not capped.
    let max_depth = ctx.max_java_stack_trace_depth();
    if max_depth != 0 && trace.len() > max_depth {
        trace.drain(..trace.len() - max_depth);
    }
    let arr = build_stack_trace_element_array(ctx, &trace);
    Ok(Some(Value::Object(Some(arr?))))
}

// ---------------------------------------------------------------------------
// jdk.internal.vm.ThreadSnapshot.create(Thread) -- JDK 25 native (gci i12/td)
// ---------------------------------------------------------------------------

/// `Thread.State.TERMINATED`'s ordinal.
const THREAD_STATE_TERMINATED: i32 = 5;

/// The JVMTI thread-status bits HotSpot stores in `ThreadSnapshot.threadStatus`
/// for a `Thread.State` ordinal: the bits `jdk.internal.misc.VM.toThreadState`
/// decodes back to that same state (`ALIVE` 0x1, `TERMINATED` 0x2, `RUNNABLE`
/// 0x4, `WAITING_INDEFINITELY` 0x10, `WAITING_WITH_TIMEOUT` 0x20,
/// `BLOCKED_ON_MONITOR_ENTER` 0x400). gci i12/td.
pub(crate) fn thread_snapshot_status(state_ordinal: i32) -> i32 {
    const ALIVE: i32 = 0x0001;
    const TERMINATED: i32 = 0x0002;
    const RUNNABLE: i32 = 0x0004;
    const WAITING_INDEFINITELY: i32 = 0x0010;
    const WAITING_WITH_TIMEOUT: i32 = 0x0020;
    const BLOCKED_ON_MONITOR_ENTER: i32 = 0x0400;
    match state_ordinal {
        0 => 0,
        1 => ALIVE | RUNNABLE,
        2 => ALIVE | BLOCKED_ON_MONITOR_ENTER,
        3 => ALIVE | WAITING_INDEFINITELY,
        4 => ALIVE | WAITING_WITH_TIMEOUT,
        _ => TERMINATED,
    }
}

/// `ThreadSnapshot$BlockerLockType` ordinals: `PARK_BLOCKER`,
/// `WAITING_TO_LOCK`, `WAITING_ON`.
const BLOCKER_PARK: i32 = 0;
const BLOCKER_WAITING_TO_LOCK: i32 = 1;
const BLOCKER_WAITING_ON: i32 = 2;

/// Which blocker HotSpot's `GetThreadSnapshotHandshakeClosure` records for a
/// thread in `state_ordinal` (gci i12/td): the monitor it is entering when
/// BLOCKED; its `parkBlocker` when parked (WAITING / TIMED_WAITING with a park
/// blocker set: `LockSupport.park` sets it before parking and clears it
/// after); else the monitor it waits on (`Object.wait`). `None` otherwise
/// (running, sleeping, a park with no blocker).
pub(crate) fn thread_snapshot_blocker(
    state_ordinal: i32,
    park_blocker: bool,
    monitor: bool,
) -> Option<i32> {
    match state_ordinal {
        2 if monitor => Some(BLOCKER_WAITING_TO_LOCK),
        3 | 4 if park_blocker => Some(BLOCKER_PARK),
        3 | 4 if monitor => Some(BLOCKER_WAITING_ON),
        _ => None,
    }
}

/// `static native ThreadSnapshot create(Thread)` (`jdk.internal.vm.ThreadSnapshot`,
/// JDK 25; HotSpot `JVM_CreateThreadSnapshot` / `ThreadSnapshotFactory`). The
/// JDK 25 `ThreadDumper` -- behind `jcmd Thread.dump_to_file` and
/// `HotSpotDiagnosticMXBean.dumpThreads` -- takes one per thread of the
/// thread containers; with no registration both threw
/// `UnsatisfiedLinkError` (gci i12/td, page
/// `gci-i11vt-virtual-thread-continuation-roots-review-findings` item 13).
///
/// Fills the fields "filled by VM": `name`, `threadStatus`
/// ([`thread_snapshot_status`] of `Thread.getState()`, the same answer every
/// other reader gets, virtual threads included), `carrierThread` (a mounted
/// virtual thread's), `stackTrace`, `locks` (`ThreadLock[]`, the monitors a
/// platform thread holds with the depth of the frame that locked each) and
/// the blocker ([`thread_snapshot_blocker`]). A platform thread's stack and
/// monitors come from the VM's `ThreadInfo` snapshot (`thread_jmx_snapshot`,
/// the frames a HotSpot thread dump lists); a virtual thread's stack from
/// its own `getStackTrace()`. `null` for a terminated thread, as HotSpot.
pub(crate) fn native_thread_snapshot_create(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let thread = match args.first() {
        Some(Value::Object(Some(t))) => *t,
        _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
    };
    // Every step below that runs Java or allocates can move the objects held
    // across it; each is rooted here and re-read through its handle.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let thread_h = scope.root(thread);
    let state = match scope.invoke_virtual(thread, "getState", "()Ljava/lang/Thread$State;", &[])? {
        Some(Value::Object(Some(state))) => state,
        _ => return Ok(Some(Value::Object(None))),
    };
    let ordinal = match scope.invoke_virtual(state, "ordinal", "()I", &[])? {
        Some(Value::Int(ordinal)) => ordinal,
        _ => return Ok(Some(Value::Object(None))),
    };
    if ordinal == THREAD_STATE_TERMINATED {
        return Ok(Some(Value::Object(None)));
    }
    let thread = scope.get(&thread_h);
    let is_virtual = matches!(
        scope.invoke_virtual(thread, "isVirtual", "()Z", &[])?,
        Some(Value::Int(v)) if v != 0
    );

    // A platform thread's frames, monitors and contended / waited monitor.
    let thread = scope.get(&thread_h);
    let jmx = if is_virtual { None } else { scope.thread_jmx_snapshot(thread) };
    let mut trace: Option<Vec<cratonvm_native_api::StackTraceEntry>> = None;
    let mut monitor_h: Option<cratonvm_native_api::NativeHandle> = None;
    let mut owned: Vec<(cratonvm_native_api::NativeHandle, i32)> = Vec::new();
    if let Some(snapshot) = jmx {
        monitor_h = snapshot.lock.map(|o| scope.root(o));
        let attributed = snapshot.locked_monitor_depths.len() == snapshot.locked_monitors.len();
        for (i, &object) in snapshot.locked_monitors.iter().enumerate() {
            // A monitor no interpreted frame claims (`-1`, or no attribution)
            // goes to the innermost frame, as the `ThreadInfo` door puts it.
            let depth = if attributed {
                snapshot.locked_monitor_depths[i].max(0)
            } else {
                0
            };
            owned.push((scope.root(object), depth));
        }
        trace = Some(snapshot.stack_trace);
    }

    let thread = scope.get(&thread_h);
    let name_h = match crate::lang_misc::thread_field(&*scope, thread, "name") {
        Value::Object(Some(name)) => Some(scope.root(name)),
        _ => None,
    };
    let thread = scope.get(&thread_h);
    let park_h = match crate::lang_misc::thread_field(&*scope, thread, "parkBlocker") {
        Value::Object(Some(blocker)) => Some(scope.root(blocker)),
        _ => None,
    };
    let carrier_h = if is_virtual {
        let thread = scope.get(&thread_h);
        match scope.get_field_by_name(thread, "carrierThread") {
            Value::Object(Some(carrier)) => Some(scope.root(carrier)),
            _ => None,
        }
    } else {
        None
    };

    let stack = match trace {
        Some(trace) => build_stack_trace_element_array(&mut *scope, &trace)?,
        None => {
            let thread = scope.get(&thread_h);
            match scope.invoke_virtual(thread, "getStackTrace", "()[Ljava/lang/StackTraceElement;", &[])? {
                Some(Value::Object(Some(stack))) => stack,
                _ => {
                    let component = stack_trace_element_component_id(&mut *scope);
                    scope.new_ref_array(component, 0)
                }
            }
        }
    };
    let stack_h = scope.root(stack);

    // `ThreadLock[]`, typeOrdinal 0 (`LOCKED`): this VM eliminates no lock.
    let mut lock_hs: Vec<cratonvm_native_api::NativeHandle> = Vec::with_capacity(owned.len());
    for (object_h, depth) in &owned {
        let lock = match scope.new_object("jdk/internal/vm/ThreadSnapshot$ThreadLock")? {
            Some(Value::Object(Some(lock))) => lock,
            _ => break,
        };
        let object = scope.get(object_h);
        scope.set_field_by_name(lock, "depth", Value::Int(*depth));
        scope.set_field_by_name(lock, "typeOrdinal", Value::Int(0));
        scope.set_field_by_name(lock, "obj", Value::Object(Some(object)));
        lock_hs.push(scope.root(lock));
    }
    let locks_h = match lock_hs.first() {
        Some(first) => {
            let first = scope.get(first);
            let component = scope.class_id_of_object(first);
            let locks = scope.new_ref_array(component, lock_hs.len());
            for (i, h) in lock_hs.iter().enumerate() {
                let lock = scope.get(h);
                scope.set_array_element(locks, i, Value::Object(Some(lock)));
            }
            Some(scope.root(locks))
        }
        None => None,
    };

    let snapshot = match scope.new_object("jdk/internal/vm/ThreadSnapshot")? {
        Some(Value::Object(Some(snapshot))) => snapshot,
        _ => return Ok(Some(Value::Object(None))),
    };
    // No allocation from here on: the handles' addresses stay current.
    let name = name_h.as_ref().map(|h| scope.get(h));
    let carrier = carrier_h.as_ref().map(|h| scope.get(h));
    let stack = scope.get(&stack_h);
    let locks = locks_h.as_ref().map(|h| scope.get(h));
    let park = park_h.as_ref().map(|h| scope.get(h));
    let monitor = monitor_h.as_ref().map(|h| scope.get(h));
    let blocker = thread_snapshot_blocker(ordinal, park.is_some(), monitor.is_some());
    let blocker_object = match blocker {
        Some(BLOCKER_PARK) => park,
        Some(_) => monitor,
        None => None,
    };
    scope.set_field_by_name(snapshot, "name", Value::Object(name));
    scope.set_field_by_name(snapshot, "threadStatus", Value::Int(thread_snapshot_status(ordinal)));
    scope.set_field_by_name(snapshot, "carrierThread", Value::Object(carrier));
    scope.set_field_by_name(snapshot, "stackTrace", Value::Object(Some(stack)));
    scope.set_field_by_name(snapshot, "locks", Value::Object(locks));
    scope.set_field_by_name(snapshot, "blockerTypeOrdinal", Value::Int(blocker.unwrap_or(0)));
    scope.set_field_by_name(snapshot, "blockerObject", Value::Object(blocker_object));
    Ok(Some(Value::Object(Some(snapshot))))
}

// ---------------------------------------------------------------------------
// System.mapLibraryName(String) вЂ” JDK 25 native
// ---------------------------------------------------------------------------

/// Maps a library name to a platform-specific filename.
/// e.g. "foo" в†’ "foo.dll" (Windows), "libfoo.so" (Linux), "libfoo.dylib" (macOS).
pub(crate) fn native_system_map_library_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let name = match args.first() {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        // `System.mapLibraryName(null)`: the JNI body throws a NullPointerException
        // with no message. It answered null, a silent value where the JDK raises
        // (`SbrW6System`'s `mapLibraryName null` row; both modes, toward HotSpot).
        _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
    };
    let mapped = if cfg!(windows) {
        format!("{name}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{name}.dylib")
    } else {
        format!("lib{name}.so")
    };
    let result = ctx.create_string(&mapped);
    Ok(Some(Value::Object(Some(result))))
}

// ---------------------------------------------------------------------------
// Thread.sleepNanos0(long) вЂ” JDK 25 native (replaces sleep(long) internally)
// ---------------------------------------------------------------------------

pub(crate) fn native_thread_sleep_nanos(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Interpreter round i5 wave 104, lane L6 (`L6W104SleepNanosNegative`):
    // the long as `sleep0` / `Unsafe.park` decode it (a category-2 local may
    // arrive tag-lost as a `Double`), and HotSpot 25's `JVM_SleepNanos`
    // refuses a negative time BEFORE the interrupt check:
    // `IllegalArgumentException("nanosecond timeout value out of range")`.
    // A negative time returned at once here (no JDK caller passes one;
    // reflection can).
    let nanos = match args.first() {
        Some(Value::Long(n)) => *n,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Int(i)) => i64::from(*i),
        _ => 0,
    };
    if nanos < 0 {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: "nanosecond timeout value out of range".to_string(),
            }
            .into(),
        );
    }
    // Wave 24 (L7): HotSpot's `JVM_SleepNanos` checks the interrupt first,
    // for every duration: a pre-interrupted `Thread.sleep(0)` (JDK 25:
    // `sleep(0)` -> `sleepNanos(0)` -> here) throws "sleep interrupted" and
    // clears the flag. The check used to sit inside `nanos > 0`.
    if ctx.is_interrupted(true) {
        return Err(sleep_interrupted(ctx, false));
    }
    if nanos > 0 {
        // Keycloak Gap 9 localization (CRATONVM_DBG_SLEEP_TRACE): JDK25
        // Thread.sleep(millis) routes through Thread.sleepNanos -> here, so the
        // worker's poll-loop sleeps land in THIS native (not the millis one).
        if crate::nbflags().dbg_sleep_trace {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static N: AtomicUsize = AtomicUsize::new(0);
            static PRINTED: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            if n % 16 == 0 && PRINTED.fetch_add(1, Ordering::Relaxed) < 80 {
                let st = ctx.capture_stack_trace();
                let frames: Vec<String> = st
                    .iter()
                    .take(12)
                    .map(|e| format!("{}.{}:{}", e.class_name, e.method_name, e.line_number))
                    .collect();
                eprintln!(
                    "[SLEEP-NANOS-TRACE #{n} nanos={nanos}] {}",
                    frames.join(" <- ")
                );
            }
        }
        let requested_millis = ((nanos as u128) + 999_999) / 1_000_000;
        let effective_millis =
            crate::async_handoff_sleep_millis(requested_millis.min(i64::MAX as u128) as i64);
        let effective_nanos = (effective_millis as u128)
            .saturating_mul(1_000_000)
            .max(nanos as u128)
            .min(u64::MAX as u128) as u64;
        let duration = std::time::Duration::from_nanos(effective_nanos);
        // Interpreter round i3 wave 76, lane L6 (page
        // `i75-L6-proposal-one-sleep-body-20261108`): the one sleep body,
        // `sleep(J)`'s too; this one does not pump the scheduled-task registry
        // (as before; whether it should is the page's open HotSpot question).
        return sleep_body(
            ctx,
            duration,
            nanos,
            // Wave 104 (lane L6): as `sleep(J)`, HotSpot's `blockingOperation`.
            "LockSupport.park",
            false,
        );
    }
    Ok(None)
}

/// The one body of a `Thread.sleep` that blocks (interpreter round i3 wave 76,
/// lane L6; page `i75-L6-proposal-one-sleep-body-20261108`): what
/// `native_thread_sleep` (`sleep(J)`, `--compatible`) and
/// `native_thread_sleep_nanos` (`sleepNanos0`, `--jdk-only`, and the target of
/// `sleep0` and `sleep(JI)`) each had a copy of, so the two modes reach
/// HotSpot's one `JVM_SleepNanos` through the same code. A pure refactor: the
/// callers keep their argument checks, their pre-sleep interrupt check, their
/// trace sampler and their duration (`duration`, after
/// `async_handoff_sleep_millis`); `recorded_nanos` is what each recorded as
/// the requested time of `jdk.ThreadSleep` (`sleep(J)`: the millis in nanos;
/// `sleepNanos0`: the nanos asked for); `pinned_reason` is each one's
/// `jdk.VirtualThreadPinned` reason; `pump` is on for `sleep(J)` only.
///
/// NEW-15.4: a non-pinned virtual thread yields its continuation (or, without
/// a timed yield, releases its carrier permit for the sleep); a pinned one
/// keeps the carrier and emits `jdk.VirtualThreadPinned` per JEP 491.
///
/// WP4.5 (`pump`): while this VM has `ScheduledExecutorService` tasks in the
/// scheduled-task registry, the sleep waits in 10 ms slices and runs them
/// between slices, outside the blocked region (a task runs Java, which a
/// GC-blocked thread must not). Otherwise ONE region for the whole sleep
/// (interpreter round i1 wave 49, lane L1: leaving it every slice made a
/// sleeping thread read RUNNABLE between two slices, and JDWP answered
/// RUNNING where HotSpot says SLEEPING: `L1W47JdiBlockedBeforeAttach`), and
/// the region says it is a sleep (interpreter round i2 wave 63, lane L1,
/// `CRATONVM_PUBLISHED_WAIT_KIND`; `TIMED_WAITING` either way, as the sleep is
/// bounded). Wave 24 (L7): counted down from the start instead of a deadline
/// `Instant` (`Thread.sleep(Long.MAX_VALUE)` overflowed it), with the
/// interrupt polled before every wait. Interpreter round i3 wave 75, lane L6
/// (`CRATONVM_SLEEP_NO_POLL`): each wait is one wait of the remainder that an
/// interrupt ends at once (of the pump's slice while pumping); off, a 10 ms
/// `std::thread::sleep` slice inside the blocked-wait trampoline (JIT round 18
/// wave 10, `CRATONVM_XT_HELPER_UNPARKED_READ`).
///
/// After the sleep the flag is read and cleared, and an interrupt seen during
/// or after it throws HotSpot's `InterruptedException("sleep interrupted")`
/// (wave 24: the unit variant threw it without a message).
fn sleep_body(
    ctx: &mut dyn NativeContext,
    duration: std::time::Duration,
    recorded_nanos: i64,
    pinned_reason: &'static str,
    pump: bool,
) -> MethodCallResult {
    let is_virtual = ctx.is_current_virtual();
    let pinned = is_virtual && ctx.vt_pin_count() > 0;
    if pinned {
        ctx.emit_virtual_thread_pinned_jfr(pinned_reason);
    }
    let release = is_virtual && !pinned;
    if release && ctx.vt_sleep_for(duration) {
        return Err(cratonvm_types::error::MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::ContinuationYield {
                wake_after_nanos: duration.as_nanos().min(u64::MAX as u128) as u64,
            },
        ));
    }
    if release {
        ctx.vt_release_carrier();
    }
    let sleep_start = std::time::Instant::now();
    let slice = std::time::Duration::from_millis(10);
    let vm = pump.then(|| ctx.vm_identity());
    let mut interrupted = false;
    let mut in_region = false;
    loop {
        let pumping = vm.is_some_and(|vm| crate::scheduled_pump::registry().has_tasks_for(vm));
        if pumping {
            if in_region {
                ctx.end_blocking_region();
                in_region = false;
            }
            crate::scheduled_pump::registry().pump(ctx);
        }
        let remaining = duration.saturating_sub(sleep_start.elapsed());
        if remaining.is_zero() {
            break;
        }
        if ctx.is_interrupted(false) {
            interrupted = true;
            break;
        }
        if !in_region {
            ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::Sleeping);
            in_region = true;
        }
        let this_slice = remaining.min(slice);
        if !ctx.sleep_wait_unpolled(if pumping { this_slice } else { remaining }) {
            ctx.run_blocked_wait(&mut || std::thread::sleep(this_slice));
        }
    }
    if in_region {
        ctx.end_blocking_region();
    }
    let actual_dur = sleep_start.elapsed();
    if release {
        ctx.vt_acquire_carrier();
    }
    ctx.record_thread_sleep(recorded_nanos, actual_dur.as_nanos() as u64);
    let interrupted_after_sleep = ctx.is_interrupted(true);
    if interrupted || interrupted_after_sleep {
        return Err(sleep_interrupted(ctx, true));
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// T19.N2 вЂ” Thread.sleep0(J)V вЂ” JDK 21+ internal sleep native.
// ---------------------------------------------------------------------------
//
// In JDK 21+ the public `Thread.sleep(long millis)` validates the argument
// and then delegates to this private `sleep0(J)V` for the actual sleep +
// interrupt check. `millis` is guaranteed >= 0 by the public caller, but
// HotSpot's native still performs a defensive negative-millis check and
// throws `IllegalArgumentException`, so we mirror that contract.
//
// Implementation notes:
//   * Bounds-check `millis >= 0` BEFORE casting to `u64` (signedв†’unsigned
//     cast of a negative value is a correctness bug вЂ” -1 would become
//     `u64::MAX`).
//   * When `millis == 0`, HotSpot still checks the interrupt status and
//     throws `InterruptedException` if set; no actual blocking happens.
//   * The sleep is `sleep_body`'s (interpreter round i3 wave 76): one wait of
//     the remainder that an interrupt ends at once (`CRATONVM_SLEEP_NO_POLL`,
//     default on), or 10 ms slices polling the interrupt with it off.
//   * The interrupt flag is CLEARED when we throw `InterruptedException`
//     per JDK spec.
pub(crate) fn native_thread_sleep0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // WP4.5 -- see `native_thread_sleep`: long args from the operand-stack
    // get re-decoded as Doubles by `CompactValue::to_value()`.
    let raw_nanos: i64 = match args.first() {
        Some(Value::Long(ns)) => *ns,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Int(i)) => *i as i64,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::IllegalArgumentException {
                    message: "sleep0: missing long nanos arg".to_string(),
                }
                .into(),
            );
        }
    };
    if raw_nanos < 0 {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: "timeout value is negative".to_string(),
            }
            .into(),
        );
    }
    // JDK 21's `Thread.sleep(long millis)` converts to NANOSECONDS and calls
    // the private `sleep0(long nanos)` (JDK 25 renamed it `sleepNanos0`, which
    // is `native_thread_sleep_nanos`). This native used to read the argument as
    // MILLISECONDS, so under `--jdk-only` on a JDK 21 boot image (where the
    // bytecode `Thread.sleep` wins over the `sleep(J)V` native) `sleep(50)`
    // asked for 50,000,000 ms and never returned
    // (`SBR4-SATB-found-while-probing-HELD-20261005.md` item 1). One sleep
    // body, so the interrupt check, the 10 ms slices, the blocked-state
    // region and the `InterruptedException("sleep interrupted")` message are
    // `sleepNanos0`'s, as HotSpot's `JVM_SleepNanos` is the one body of both.
    native_thread_sleep_nanos(ctx, &[Value::Long(raw_nanos)])
}

// ---------------------------------------------------------------------------
// T14 вЂ” System bootstrap chain: initPhase1/2/3
// ---------------------------------------------------------------------------

/// `System.initPhase1()V` вЂ” JDK bootstrap phase 1.
///
/// In the real JDK, this method:
/// 1. Sets up the system properties map (`System.props`)
/// 2. Initializes stdout, stderr, stdin streams
/// 3. Sets `System.lineSeparator`
///
/// Our implementation delegates to NativeContext methods that are already
/// backed by real VM state (system_properties, system_streams). We set
/// the System class's static fields so that subsequent Java code can read
/// `System.out`, `System.err`, `System.in`, and `System.lineSeparator`
/// directly from the static fields.
pub(crate) fn native_system_init_phase1(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Step 1: Ensure System class is initialized so static fields exist
    ctx.ensure_class_initialized("java/lang/System")?;

    // Step 3, before the streams: set System.lineSeparator from the
    // line.separator property. JIT round 20, lane walkcompat: the JDK's own
    // `initPhase1` captures it right after the properties, before any stream
    // is built, so a stream step that throws (the launcher then falls back to
    // the synthetic streams) no longer leaves the static null -- which the
    // JDK `System.lineSeparator()` body `--compatible` now runs under the
    // `jdk-print-writer` row (`retired_shadow::is_print_writer_line_separator_row`)
    // would hand to every `println`.
    let line_sep = ctx
        .get_system_property("line.separator")
        .unwrap_or_else(|| if cfg!(windows) { "\r\n" } else { "\n" }.to_string());
    let line_sep_obj = ctx.create_string(&line_sep);
    ctx.set_static_field_by_name(
        "java/lang/System",
        "lineSeparator",
        Value::Object(Some(line_sep_obj)),
    );

    // Step 2: Set System.out and System.err from VM-managed streams.
    // The interpreter already intercepts getstatic on System.out/err,
    // but we also write the static fields so direct heap reads see them.
    // `resolve_field_index` is instance-only; `out`/`err`/`in`/`lineSeparator`
    // are all statics, so we use the static-by-name path (the same one
    // `setIn0`/`setOut0`/`setErr0` use elsewhere in this crate).
    // Helper: build a UTF-8 Charset stub matching the layout used by
    // `Charset.forName` / `Charset.defaultCharset` natives (slot 0 = name String).
    // Keycloak's Picocli.getErrWriter goes `new PrintWriter(System.err)` в†’
    // `PrintWriter(OutputStream, boolean)` which reads `((PrintStream)err).charset()`,
    // a plain getfield on the `charset` field. If that field is null, the
    // downstream `new OutputStreamWriter(stream, charset)` throws NPE("charset")
    // and Quarkus silently exits during command-line parsing. We must therefore
    // stamp a non-null Charset on System.out/err at bootstrap time.
    //
    // charset-NPE fix (2026-05-21): `set_field_by_name` resolves the
    // `charset` slot by walking the receiver's class hierarchy. If the
    // System.out/err object was allocated against the 1-field synthetic
    // `java/io/PrintStream` stub (created before the real class was
    // loaded), that walk finds no `charset` field and silently drops
    // the write вЂ” leaving the field null. `ensure_system_streams` now
    // force-loads the real PrintStream class first, but we also make
    // this helper defensive: it ensures the real `java/io/PrintStream`
    // class is loaded so the `charset` field is resolvable, and it
    // verifies the write actually landed.
    /// The charset name a system stream should carry: its `*.encoding`
    /// property when the launcher or the platform set one, UTF-8 otherwise.
    ///
    /// Reading the PROPERTY rather than calling
    /// `cratonvm_native_api::os_encoding` directly is what makes
    /// `-Dstdout.encoding=…` on the command line work, exactly as it does on
    /// HotSpot: the property table has already absorbed the `-D` overrides by
    /// the time `initPhase1` runs.
    fn std_stream_encoding(ctx: &dyn NativeContext, key: &str) -> String {
        ctx.get_system_property(key)
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "UTF-8".to_string())
    }

    fn install_charset(ctx: &mut dyn NativeContext, stream: ObjectRef, encoding: &str) {
        // gc-common w16-e: everything below until the stamp is a GC point
        // (two class initialisations, `Charset.forName` in Java, the stub's
        // allocations), and `stream` used to be stamped through the address
        // it had before them. Rooted here, re-read at the stamp.
        let stream_pin = ctx.pin_native_root(stream);
        install_charset_rooted(ctx, stream_pin, stream, encoding);
        ctx.unpin_native_roots(stream_pin);
    }

    fn install_charset_rooted(
        ctx: &mut dyn NativeContext,
        stream_pin: usize,
        stream: ObjectRef,
        encoding: &str,
    ) {
        // Ensure the real PrintStream class is loaded so `charset` is a
        // resolvable field name. (No-op in pure synthetic-jdk mode.)
        let _ = ctx.ensure_class_initialized("java/io/PrintStream");
        let cs_class = match ctx.ensure_class_initialized("java/nio/charset/Charset") {
            Ok(cid) => cid,
            Err(_) => return,
        };
        // Prefer the REAL charset. `java/nio/charset/Charset` is ABSTRACT, so a
        // hand-allocated instance of it has no `newEncoder()` body and every
        // caller that encodes through this stream dies with
        // `AbstractMethodError` rather than the NPE this helper exists to
        // prevent -- a worse error, one frame further from the cause. See
        // `bug-printstream-charset-answers-the-abstract-base-20260825-FIXED-20260901.md`
        // (retired 2026-09-01 with both of its residuals closed; the stamp
        // below is one of them).
        //
        // This runs during `initPhase1`, which is why the hand-allocated stub
        // was reached for in the first place, so the real call is attempted
        // defensively and the stub stays as the fallback: bootstrap ordering
        // decides which one lands, and neither outcome is worse than today's.
        // `PrintStream.charset()` repairs a stub survivor on first read.
        // `encoding` is the STREAM's encoding, not a constant: HotSpot gives
        // `System.out` the console's charset (`ANSI_X3.4-1968` under `LANG=C`,
        // `Cp1251` on a 1251 Windows host) and only `file.encoding` is pinned
        // to UTF-8. Stamping UTF-8 unconditionally is what made
        // `System.out.print("Ж")` write UTF-8 bytes where HotSpot writes `?`
        // — `printstream_encode` and `install_real_stream_fields` below both
        // read THIS object, so the stamp decides the bytes as well as the
        // answer to `charset()`.
        let resolve = |ctx: &mut dyn NativeContext, name: &str| -> Option<ObjectRef> {
            let want = ctx.create_string(name);
            match ctx.invoke(
                "java/nio/charset/Charset",
                "forName",
                "(Ljava/lang/String;)Ljava/nio/charset/Charset;",
                &[Value::Object(Some(want))],
            ) {
                Ok(Some(Value::Object(Some(real))))
                    if !matches!(
                        ctx.class_name_of_id(ctx.class_id_of_object(real))
                            .as_deref(),
                        Some("java/nio/charset/Charset")
                    ) =>
                {
                    Some(real)
                }
                _ => None,
            }
        };
        let cs_obj = match resolve(ctx, encoding) {
            Some(real) => real,
            // The host named an encoding this image has no charset for (a
            // synthetic-JDK build asked for `cp866`, say). A stand-in that
            // CLAIMS that encoding while everything downstream writes UTF-8
            // would be worse than either honest answer, so degrade the whole
            // stream to UTF-8 rather than the name alone.
            None => match resolve(ctx, "UTF-8") {
                Some(utf8) => utf8,
                None => {
                    // The stub is rooted across its name's allocation.
                    let cs_fields = ctx.class_num_total_fields(cs_class).max(1);
                    let stub = ctx.alloc_object(cs_class, cs_fields);
                    let stub_pin = ctx.pin_native_root(stub);
                    let name = ctx.create_string(encoding);
                    let stub = ctx.read_native_pin(stub_pin, stub);
                    ctx.unpin_native_roots(stub_pin);
                    ctx.set_field(stub, 0, Value::Object(Some(name)));
                    stub
                }
            },
        };
        let stream = ctx.read_native_pin(stream_pin, stream);
        // Use field-by-name so we hit the real-JDK `charset` slot (its
        // declared index differs from any synthetic ordering).
        ctx.set_field_by_name(stream, "charset", Value::Object(Some(cs_obj)));
        // Verify the write landed. If `charset` did not resolve (e.g. the
        // receiver is still a fieldless synthetic stub), the bare
        // `set_field_by_name` above was a no-op and `PrintStream.charset()`
        // would return null в†’ NPE("charset") on the first `new
        // PrintWriter(System.err)`. Fall back to resolving the slot
        // index explicitly against `java/io/PrintStream` and, as a last
        // resort, scan the object's reference slots is unsafe (could
        // clobber `out`), so we only retry the explicit-index path.
        if let Some(idx) = ctx.resolve_field_index("java/io/PrintStream", "charset") {
            if idx < ctx.object_num_fields(stream) {
                ctx.set_field(stream, idx, Value::Object(Some(cs_obj)));
            }
        }
    }

    /// Give `System.out`/`System.err` the three stream fields the REAL
    /// `PrintStream` bytecode needs.
    ///
    /// The synthetic `PrintStream` this VM mints for the system streams has
    /// only `charset` and `autoFlush` populated. MEASURED against HotSpot with
    /// `--add-opens java.base/java.io=ALL-UNNAMED` and reflection on the live
    /// object:
    ///
    /// ```text
    ///               HotSpot                  this VM (before)
    ///   out         BufferedOutputStream     NULL
    ///   charOut     OutputStreamWriter       NULL
    ///   textOut     BufferedWriter           NULL
    ///   charset     sun.nio.cs.UTF_8         sun.nio.cs.UTF_8
    /// ```
    ///
    /// Nothing notices while this VM's own `PrintStream` natives answer: they
    /// write to the host stream directly and never read these fields. The
    /// moment the real bytecode runs, `PrintStream.writeln` calls
    /// `ensureOpen()`, finds `out == null`, throws `IOException`, and catches
    /// it into `trouble = true`:
    ///
    /// ```text
    ///   CRATONVM_ENFORCE_NATIVE_SHADOW=java/io/PrintStream
    ///     System.out.println("A")        prints NOTHING, checkError() == true
    ///     new PrintStream(new FileOutputStream(FileDescriptor.out), true)
    ///                                    prints normally
    /// ```
    ///
    /// **Every write is discarded and the VM exits 0.** That took out twelve of
    /// this lane's thirteen probes at once when the shadow dial was armed over
    /// `java/io/` — each reported `lines=0` and a clean exit, which reads as a
    /// probe that passed. The dial is a supported mode and the retirement
    /// instrument for this whole campaign, so a family that cannot survive
    /// being armed cannot be adjudicated at all.
    ///
    /// Built from the real constructors rather than by hand, so the objects are
    /// whatever the running JDK's own classes are. Every step is defensive: if
    /// anything here is not ready this early in `initPhase1`, the repair is
    /// skipped and the stream is exactly as it was before.
    fn install_real_stream_fields(ctx: &mut dyn NativeContext, stream: ObjectRef, fd_name: &str) {
        // Already wired (a real-JDK-constructed stream, or a second call).
        if matches!(ctx.get_field_by_name(stream, "out"), Value::Object(Some(_))) {
            return;
        }
        // Each `new_object` can move the heap, so everything live across one is
        // pinned and re-read (the native stale-local family). gc-common w16-e:
        // `stream` is pinned BEFORE `FileDescriptor`'s initialisation, whose
        // first run is a `<clinit>` (a GC point); its `charset` used to be read
        // through the pre-initialisation address, and that address pinned.
        let stream_pin = ctx.pin_native_root(stream);
        // `FileDescriptor.out` / `.err` — the same descriptor a hand-written
        // `new FileOutputStream(FileDescriptor.out)` uses, which is the form
        // measured to work under the armed dial.
        let fd = match ctx.ensure_class_initialized("java/io/FileDescriptor") {
            Ok(cid) => match ctx.static_field_index_by_name(cid, fd_name) {
                Some(idx) => match ctx.get_static_field(cid, idx) {
                    Value::Object(Some(fd)) => Some(fd),
                    _ => None,
                },
                None => None,
            },
            Err(_) => None,
        };
        let stream = ctx.read_native_pin(stream_pin, stream);
        let charset = match ctx.get_field_by_name(stream, "charset") {
            Value::Object(Some(cs)) => Some(cs),
            _ => None,
        };
        let (Some(fd), Some(charset)) = (fd, charset) else {
            // zt t7/cl3: the silent skip, said under the print-loss switch.
            if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_PRINT_LOSS") {
                eprintln!(
                    "[cratonvm] print-state: System.{fd_name} left unwired \
                     (FileDescriptor.{fd_name}={} charset={})",
                    fd.is_some(),
                    charset.is_some()
                );
            }
            ctx.unpin_native_roots(stream_pin);
            return;
        };
        let fd_pin = ctx.pin_native_root(fd);
        let cs_pin = ctx.pin_native_root(charset);

        let built = (|| -> Option<()> {
            let fos = match ctx.new_object("java/io/FileOutputStream") {
                Ok(Some(Value::Object(Some(o)))) => o,
                _ => return None,
            };
            let fos_pin = ctx.pin_native_root(fos);
            let fd_now = ctx.read_native_pin(fd_pin, fd);
            let fos_now = ctx.read_native_pin(fos_pin, fos);
            ctx.invoke(
                "java/io/FileOutputStream",
                "<init>",
                "(Ljava/io/FileDescriptor;)V",
                &[Value::Object(Some(fos_now)), Value::Object(Some(fd_now))],
            )
            .ok()?;

            let osw = match ctx.new_object("java/io/OutputStreamWriter") {
                Ok(Some(Value::Object(Some(o)))) => o,
                _ => return None,
            };
            let osw_pin = ctx.pin_native_root(osw);
            let fos_now = ctx.read_native_pin(fos_pin, fos);
            let cs_now = ctx.read_native_pin(cs_pin, charset);
            let osw_now = ctx.read_native_pin(osw_pin, osw);
            ctx.invoke(
                "java/io/OutputStreamWriter",
                "<init>",
                "(Ljava/io/OutputStream;Ljava/nio/charset/Charset;)V",
                &[
                    Value::Object(Some(osw_now)),
                    Value::Object(Some(fos_now)),
                    Value::Object(Some(cs_now)),
                ],
            )
            .ok()?;

            let bw = match ctx.new_object("java/io/BufferedWriter") {
                Ok(Some(Value::Object(Some(o)))) => o,
                _ => return None,
            };
            let bw_pin = ctx.pin_native_root(bw);
            let osw_now = ctx.read_native_pin(osw_pin, osw);
            let bw_now = ctx.read_native_pin(bw_pin, bw);
            ctx.invoke(
                "java/io/BufferedWriter",
                "<init>",
                "(Ljava/io/Writer;)V",
                &[Value::Object(Some(bw_now)), Value::Object(Some(osw_now))],
            )
            .ok()?;

            // `FilterOutputStream.closeLock` is `private final Object closeLock
            // = new Object()` -- an INSTANCE INITIALISER, and this stream was
            // allocated rather than constructed, so no initialiser ever ran.
            // The other three fields above had to be installed by hand for the
            // same reason; this one was missed because it has no wiring to
            // forget, only an initialiser that did not happen. MEASURED
            // 2026-09-12 by reflection against HotSpot 25: `java.lang.Object`
            // there, `null` here, on `System.out` and `System.err`, in both
            // modes. It is the last live item on the blocked list W7-22 §3
            // wrote for retiring `java/io/PrintStream`.
            let lock = match ctx.new_object("java/lang/Object") {
                Ok(Some(Value::Object(Some(o)))) => o,
                _ => return None,
            };
            let lock_pin = ctx.pin_native_root(lock);

            // Publish only once all FOUR exist: a half-wired stream (an `out`
            // with no `textOut`) would turn today's silent discard into an NPE
            // inside `writeln`, which is worse than either endpoint.
            let stream_now = ctx.read_native_pin(stream_pin, stream);
            let fos_now = ctx.read_native_pin(fos_pin, fos);
            let osw_now = ctx.read_native_pin(osw_pin, osw);
            let bw_now = ctx.read_native_pin(bw_pin, bw);
            let lock_now = ctx.read_native_pin(lock_pin, lock);
            ctx.set_field_by_name(stream_now, "out", Value::Object(Some(fos_now)));
            ctx.set_field_by_name(stream_now, "charOut", Value::Object(Some(osw_now)));
            ctx.set_field_by_name(stream_now, "textOut", Value::Object(Some(bw_now)));
            ctx.set_field_by_name(stream_now, "closeLock", Value::Object(Some(lock_now)));
            Some(())
        })();
        // zt t7/cl3: a step that failed leaves the stream unwired, and every
        // later write through the JDK's `PrintStream` bytecode is discarded.
        if built.is_none() && cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_PRINT_LOSS") {
            eprintln!(
                "[cratonvm] print-state: System.{fd_name} left unwired \
                 (a FileOutputStream / OutputStreamWriter / BufferedWriter step failed)"
            );
        }

        ctx.unpin_native_roots(stream_pin);
    }

    /// Stamp one system stream and publish it as `System.<name>`.
    ///
    /// gc-common w16-e: both steps run Java and allocate, and the stream used
    /// to be passed to the second step, and published into the static, through
    /// the address `get_system_stream` returned before either. Rooted across
    /// both and re-read after each.
    fn install_system_stream(
        ctx: &mut dyn NativeContext,
        stream: ObjectRef,
        encoding: &str,
        name: &str,
    ) {
        let stream_pin = ctx.pin_native_root(stream);
        install_charset(ctx, stream, encoding);
        let stream = ctx.read_native_pin(stream_pin, stream);
        install_real_stream_fields(ctx, stream, name);
        let stream = ctx.read_native_pin(stream_pin, stream);
        ctx.unpin_native_roots(stream_pin);
        ctx.set_static_field_by_name("java/lang/System", name, Value::Object(Some(stream)));
    }

    if let Some(out_stream) = ctx.get_system_stream("out") {
        let enc = std_stream_encoding(ctx, "stdout.encoding");
        install_system_stream(ctx, out_stream, &enc, "out");
    }
    if let Some(err_stream) = ctx.get_system_stream("err") {
        let enc = std_stream_encoding(ctx, "stderr.encoding");
        install_system_stream(ctx, err_stream, &enc, "err");
    }
    // S110 вЂ” System.in: wire up to OS stdin (fd id 0 in our FileDescriptorTable,
    // which pre-registers it). The synthetic `Scanner.<init>(InputStream)`
    // native in `native-io/src/lib.rs` reads field 0 of the stream object;
    // when it sees `Value::Int(fd)` it pulls bytes via `fd_table().read_byte`.
    // Allocating a FileInputStream-shaped object with field 0 = Int(0) is
    // therefore enough to make `new Scanner(System.in)` consume real stdin.
    //
    // Without this install, `System.in` was null, so `new Scanner(System.in)`
    // got the empty-string fallback in the native and `nextLine()`/`nextInt()`
    // immediately threw `NoSuchElementException: no more elements`.
    {
        // gc-common w16-e: the stdin object is held in a handle scope. It used
        // to be written through the address it had before the
        // `FileDescriptor` initialisation (a `<clinit>` on first touch) and
        // the descriptor's allocation, and then cached and published through
        // that address too.
        // HotSpot's stdin descriptor reads `handle == -1` on Unix
        // (`FileDescriptor.getHandle(0)`), the stream's handle on Windows
        // (`CRATONVM_FD_FIELDS_PER_PLATFORM`, JIT r23 w1 L6; it was 0, which
        // also left the descriptor indistinguishable from an unpopulated one).
        let stdin_handle = cratonvm_native_io::nio_native::standard_stream_handle(
            0,
            cratonvm_native_io::nio_native::fd_fields_per_platform_enabled(),
        );
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        // Reuse a pinned stdin object if `GETSTATIC System.in` / `ensure_system_stdin_object`
        // materialised it before `initPhase1` (Surefire fork bootstrap).
        let in_h = if let Some(o) = scope.get_system_stream("in") {
            scope.root(o)
        } else {
            let fis_class_id = scope.ensure_class_initialized("java/io/FileInputStream")?;
            let num_fields = scope.class_num_total_fields(fis_class_id).max(2);
            // Both classes are initialised before either object exists.
            let fd_class_id = scope.ensure_class_initialized("java/io/FileDescriptor")?;
            let fd_fields = scope.class_num_total_fields(fd_class_id).max(2);
            let new_in = scope.alloc_object(fis_class_id, num_fields);
            let new_in_h = scope.root(new_in);
            let fd_obj = scope.alloc_object(fd_class_id, fd_fields);
            scope.set_field_by_name(fd_obj, "fd", Value::Int(0));
            scope.set_field_by_name(fd_obj, "handle", Value::Long(stdin_handle));
            let new_in = scope.get(&new_in_h);
            scope.set_field_by_name(new_in, "fd", Value::Object(Some(fd_obj)));
            if !matches!(
                scope.get_field_by_name(new_in, "fd"),
                Value::Object(Some(_))
            ) {
                // Legacy synthetic fallback: encode stdin as `fd + 1` so the
                // reference-slot coercion path cannot collapse fd 0 to null.
                scope.set_field(new_in, 1, Value::Int(1));
            }
            scope.cache_system_stdin(new_in);
            new_in_h
        };
        let in_obj = scope.get(&in_h);
        let fd_obj = match scope.get_field_by_name(in_obj, "fd") {
            Value::Object(Some(fd_obj)) => fd_obj,
            _ => {
                let fd_class_id = scope.ensure_class_initialized("java/io/FileDescriptor")?;
                let fd_fields = scope.class_num_total_fields(fd_class_id).max(2);
                let fd_obj = scope.alloc_object(fd_class_id, fd_fields);
                let in_obj = scope.get(&in_h);
                scope.set_field_by_name(in_obj, "fd", Value::Object(Some(fd_obj)));
                fd_obj
            }
        };
        scope.set_field_by_name(fd_obj, "fd", Value::Int(0));
        scope.set_field_by_name(fd_obj, "handle", Value::Long(stdin_handle));
        let in_obj = scope.get(&in_h);
        scope.cache_system_stdin(in_obj);
        scope.set_static_field_by_name("java/lang/System", "in", Value::Object(Some(in_obj)));
    }

    // Step 3 (`System.lineSeparator`) runs first, right after step 1: see
    // there.

    // Step 4: snapshot the system properties the way the real `initPhase1`'s
    // `VM.saveProperties` does, for native callers that need the boot-time
    // answer rather than the live store -- see `saved_system_property`.
    capture_saved_system_properties(ctx);

    Ok(None)
}

/// `System.initPhase2(ZZ)I` вЂ” JDK bootstrap phase 2 (module system).
///
/// In the real JDK, this initializes the module system graph. Our VM
/// handles modules synthetically (all classes are in the unnamed module),
/// so we return 0 (JNI_OK) to indicate success.
///
/// Parameters: (boolean printToStderr, boolean printStackTrace)
/// Returns: int (0 = success, non-zero = failure)
pub(crate) fn native_system_init_phase2(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Module system is handled synthetically вЂ” report success
    Ok(Some(Value::Int(0)))
}

/// `System.initPhase3()V` вЂ” JDK bootstrap phase 3 (class loader hierarchy).
///
/// In the real JDK, this sets up the platform and application class loaders.
/// Our VM uses a flat class loading model, so this is a no-op.
pub(crate) fn native_system_init_phase3(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

// ---------------------------------------------------------------------------
// T14 вЂ” jdk/internal/misc/VM natives
// ---------------------------------------------------------------------------

/// `VM.getSavedProperty(String)String` вЂ” return a saved VM property.
///
/// The real JDK saves certain system properties during early bootstrap
/// before `System.initPhase1` runs. Our implementation delegates to
/// the same property store used by `System.getProperty`.
pub(crate) fn native_vm_get_saved_property(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let key = match args.first() {
        Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
        _ => return Ok(Some(Value::Object(None))),
    };
    match ctx.get_system_property(&key) {
        Some(value) => {
            let s = ctx.create_string(&value);
            Ok(Some(Value::Object(Some(s))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

/// `VM.saveProperties(Map)V` вЂ” the setter half of `getSavedProperty`'s field,
/// never registered until now.
///
/// Real `initPhase1()V`'s own bytecode is deliberately left unintercepted
/// under a real-JDK-backed image (`register_inner`'s
/// `drop_real_layout_synthetic && class_name == "java/lang/System" &&
/// matches!(method_name, "initPhase1" | "initPhase2" | "initPhase3")` arm)
/// so the real JDK's own bootstrap sequence runs, supported by narrower
/// natives -- this is that support for its very first substantive
/// instruction. `javap -c java.lang.System`: `initPhase1` opens with
/// `setJavaLangAccess()`, `SystemProps.initProperties()`, `VersionProps.init()`,
/// then at bci 12, `invokestatic jdk/internal/misc/VM.saveProperties`.
///
/// Real `saveProperties`'s own body (`javap -c jdk.internal.misc.VM`) is
/// `if (initLevel() != 0) throw new IllegalStateException("Wrong init
/// level"); savedProps = props;` -- a precondition this VM used not to
/// satisfy: `VM.initLevel()`'s native (`native-builtins/src/lib.rs`,
/// `phases_early.rs`) used to floor-clamp to 2, which made
/// `initLevel() != 0` permanently true, so real `saveProperties` threw
/// EVERY time real `initPhase1` reached it, before `VM.savedProps` was ever
/// populated. (j27: over a real JDK `initLevel()` now answers the JDK's
/// static -- the JDK body under the `jdk-vm-init-level` HotSpot-alignment
/// row, lane FBTAIL's static-reading native with the row off -- and only the
/// synthetic JDK keeps the floor. This native still skips the check and the
/// `directMemory` / `pageAlignDirectMemory` writes the real body makes.) `jdk/internal/loader/ClassLoaders`'s own `<clinit>` (via
/// `BootLoader.getServicesCatalog()`) is what actually reads that field back
/// during `ModuleLayer.boot()`, so the failure surfaces there as
/// `IllegalStateException: Not yet initialized` -- a different message for
/// the same root cause, and permanent per JVMS 5.5 once `ClassLoaders`
/// fails its own `<clinit>` once.
///
/// The fix is the same shape as `getSavedProperty`'s own native two
/// registrations up: skip the level check entirely and just do what the
/// real setter's last statement does, `savedProps = props`, taking the
/// `Map` real bytecode just built (`SystemProps.initProperties()`'s result)
/// rather than reconstructing one from this VM's own property table --
/// `saveProperties` is a pure assignment, so there is nothing to get wrong
/// by using the caller's own argument.
pub(crate) fn native_vm_save_properties(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let props = match args.first() {
        Some(v @ Value::Object(Some(_))) => v.clone(),
        _ => return Ok(None),
    };
    // Initialise `VM` BEFORE the store, not after. Native dispatch does not run a
    // static method's class initialiser (JVMS 5.5) the way an interpreted
    // `invokestatic` would, and this native is the first thing to touch `VM`
    // during `initPhase1`: MEASURED on `--jdk-only`, `is_class_initialized`
    // answers false here, the store lands in the pre-`<clinit>` static block, and
    // `VM.<clinit>` then resets it -- so `savedProps` read back `null` on the first
    // `ClassLoaders.<clinit>` (`VM.getSavedProperty` -> "Not yet initialized"),
    // which JVMS 5.5 makes permanent. Every `ServiceLoader`/`java.util.logging`
    // user in the process then died with `NoClassDefFoundError: ClassLoaders`.
    //
    // gc-common w19-e: `VM.<clinit>` is Java, and the map (from `args`) was
    // stored at its address from before it. Rooted across it.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let props_h = crate::lang_misc::w19e_root_value(&mut scope, props);
    let _ = scope.ensure_class_initialized("jdk/internal/misc/VM");
    let props = crate::lang_misc::w19e_rooted_value(&scope, props, props_h.as_ref());
    scope.set_static_field_by_name("jdk/internal/misc/VM", "savedProps", props);
    Ok(None)
}

/// `VM.getRuntimeArguments()[String` вЂ” return the VM runtime arguments.
///
/// Returns an empty String array (we don't expose internal runtime args).
pub(crate) fn native_vm_get_runtime_arguments(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // `String[]`, the declared type (i6-L2).
    let component = crate::lang_class::reflection_component_id(ctx, "java/lang/String");
    let arr = ctx.new_ref_array(component, 0);
    Ok(Some(Value::Object(Some(arr))))
}

// ---------------------------------------------------------------------------
// T15 вЂ” Remaining missing natives
// ---------------------------------------------------------------------------

/// `java/lang/ref/Finalizer.register(Object)V`
///
/// Registers an object for finalization with the VM's own reference processor
/// (this VM finalizes through it; the JDK's `Finalizer` queue stays empty).
///
/// gc-common w5-d (2026-09-24), `handoff-w2d-finalizer-registration-native`
/// item 2: this used to call `discover_reference(3, obj, obj, None)`, and wire
/// `3` is `ReferenceType::Cleaner` (`vm_exec.rs::discover_reference`), not
/// `Finalizer` — a registration that reached it was neither resurrected nor
/// finalized, only screened out later as a malformed cleaner action. It now
/// asks the VM to register a `Finalizer` row, and only for a class that
/// overrides `finalize()`, which is `Finalizer.register`'s own precondition.
/// Nothing in the VM calls `Finalizer.register` for an object it already
/// registered at allocation, so this adds no second row.
pub(crate) fn native_finalizer_register(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = the object to register for finalization
    if let Some(Value::Object(Some(obj))) = args.first() {
        ctx.register_if_finalizable(*obj);
    }
    Ok(None)
}

/// `java/lang/ref/Finalizer.isFinalizationEnabled()Z`
///
/// HotSpot's `JVM_IsFinalizationEnabled`: the launcher's
/// `--finalization=enabled|disabled` (JEP 421). zt t18/hunt2: this VM has the
/// switch now (`VmConfig::finalization_enabled`, per VM); it answered `true`
/// unconditionally and the launcher refused the option. `Finalizer.<clinit>`
/// reads it once into `ENABLED`, so a `false` here is also why no `Finalizer`
/// thread starts, as on HotSpot. The class manager gives no class a finalizer
/// in that mode, so `native_finalizer_register` is never reached for one.
pub(crate) fn native_finalizer_is_finalization_enabled(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(i32::from(ctx.finalization_enabled()))))
}

/// `java/lang/reflect/Array.newArray(Class<?> componentType, int length) в†’ Object`
///
/// Allocates a new array with the given component type and length.
/// This is an alias for `Array.newInstance` but with a different name used
/// internally by the JDK reflection framework.
pub(crate) fn native_array_new_array(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = component type (Class mirror), args[1] = length
    //
    // HotSpot's `Reflection::reflect_new_array` checks, in this order (probe
    // `tools/probes/interp/L4/L4W29ReflectNewArray.java`, rows 1-8): a null
    // component is a `NullPointerException` (before the length), a negative
    // length is `NegativeArraySizeException`, and `void` or an array class
    // that already has 255 dimensions is an `IllegalArgumentException` with
    // no message. Under `--jdk-only` this native is what `Array.newInstance`
    // reaches (the JDK's own `newInstance` body calls it), and it used to
    // allocate an `Object[]` for a null component and a bogus array for
    // `void`; the compatible-mode `newInstance` native made the same checks
    // itself (`reflect_array_component_ok`), so it never gets here with them.
    if !matches!(args.first(), Some(Value::Object(Some(_)))) {
        return Err(RuntimeError::NullPointerException { message: None }.into());
    }
    let length = match args.get(1) {
        Some(Value::Int(n)) => {
            if *n < 0 {
                return Err(RuntimeError::NegativeArraySizeException { size: *n }.into());
            }
            *n as usize
        }
        _ => 0,
    };

    // Determine element type from the Class mirror.
    //
    // `Array.newInstance(Class,int)` / `Arrays.copyOf(.., Class)` delegate here
    // (`reflect/Array.newArray`). The component Class is frequently a
    // *synthesized array-class mirror* (e.g. `[Ljava/lang/String;` from
    // `String[][].class.getComponentType()`), whose name is only recoverable
    // via the VM reverse-map. The previous `read_string`/slot-1 read returned
    // nothing for those mirrors and defaulted to `java/lang/Object`, so
    // `rows.toArray(new Value[0][])` (H2 SortOrder.sort) allocated a bare
    // `Object[]` and CCE'd on the `(Value[][])` checkcast. Resolve via
    // `mirror_class_name` first (handles array + ordinary classes), keeping the
    // old readers as a fallback for legacy/unit-test mirrors.
    let comp_mirror = match args.first() {
        Some(Value::Object(Some(mirror))) => Some(*mirror),
        _ => None,
    };
    // JIT round 23 wave 2 (proxy-define page;
    // `CRATONVM_REFLECT_NEW_ARRAY_ONE_RESOLVE`, default ON): the reverse map
    // answers the mirror's `ClassId` once. `mirror_class_name` asks it first
    // and `mirror_class_id` below asks it again, so when it answers (with a
    // name) the result is the same `(name, id)` pair the two lookups give.
    // Wave 2e: the name is the class record's own `Arc<str>`
    // (`class_name_arc_of_id`, the same text `class_name_of_id` copies), so a
    // reference component costs no `String` at all.
    let one_resolve = crate::nbflags().reflect_new_array_one_resolve;
    let resolved: Option<(std::sync::Arc<str>, cratonvm_types::ClassId)> = match comp_mirror {
        Some(mirror) if one_resolve => ctx.class_id_from_mirror(mirror).and_then(|cid| {
            ctx.class_name_arc_of_id(cid)
                .filter(|s| !s.is_empty())
                .map(|name| {
                    let name = if name.contains('.') {
                        std::sync::Arc::from(name.replace('.', "/"))
                    } else {
                        name
                    };
                    (name, cid)
                })
        }),
        _ => None,
    };
    let (comp_name, resolved_id): (std::sync::Arc<str>, _) = match resolved {
        Some((name, cid)) => (name, Some(cid)),
        None => (std::sync::Arc::from(comp_name_slow(&*ctx, comp_mirror)), None),
    };

    if crate::nbflags().dbg_toarray_ok {
        eprintln!(
            "[DBG_TOARRAY] newArray comp_name={:?} len={}",
            comp_name, length
        );
    }

    // Map primitive type names to ArrayElementType. The human name only:
    // `mirror_class_name` answers `int`, never the descriptor `I`, and a class named
    // `C` or `B` (a Groovy script's, a one-letter test class) is not `char`/`byte`.
    // gc-common w8-b (`common-w3c-native-callback-allocations-cannot-collect`,
    // route 1): the array is the LAST thing this native does, and it holds no
    // heap reference past this point (the component mirror is read above and
    // never again), so the allocation may reclaim and retry before the
    // `OutOfMemoryError`, exactly as `anewarray` would. `live` is empty.
    // This is the genuine JDK native behind `Array.newInstance(Class, int)`,
    // live in every mode (a `Bridge`).
    use cratonvm_types::ArrayElementType as Elem;
    let primitive = match &*comp_name {
        "int" => Some(Elem::Int),
        "long" => Some(Elem::Long),
        "float" => Some(Elem::Float),
        "double" => Some(Elem::Double),
        "boolean" => Some(Elem::Boolean),
        "byte" => Some(Elem::Byte),
        "char" => Some(Elem::Char),
        "short" => Some(Elem::Short),
        _ => None,
    };
    let arr = match primitive {
        Some(elem) => crate::new_array_reclaiming(ctx, elem, length, &mut [], "Array.newInstance")?,
        None => {
            if reflect_new_array_refuses_component(&comp_name, 1) {
                return Err(RuntimeError::IllegalArgumentException {
                    message: String::new(),
                }
                .into());
            }
            // Reference array вЂ” resolve the component class. `ensure_class_initialized`
            // synthesizes array-descriptor components (`[L...;`) on demand, so a
            // multi-dimensional template yields the correct nested array type.
            //
            // Prefer the component mirror's OWN ClassId (`mirror_class_id`,
            // reverse-map lookup) over re-resolving by name:
            // `ensure_class_initialized` has no loader context here, so for
            // a class name registered under more than one ClassId it can
            // silently pick the wrong one. Observed for Jackson's
            // `KeyDeserializers` interface in the AOT `web.service.registry`
            // suite: `old.getClass().getComponentType()` correctly names the
            // interface, but re-resolving that name here returned a SECOND,
            // distinct ClassId for the "same" class, so the freshly
            // allocated array's component type disagreed with the ClassId
            // already tagging `old`'s elements and `System.arraycopy`
            // correctly rejected the mismatch with `ArrayStoreException`.
            // Falling back to the name-based lookup only when the mirror
            // isn't in the reverse map at all (synthesized/legacy mirrors)
            // preserves the multi-dimensional-array-descriptor synthesis
            // this function otherwise relies on `ensure_class_initialized`
            // for.
            let comp_id = resolved_id
                .or_else(|| {
                    comp_mirror.and_then(|m| crate::lang_class::mirror_class_id(&*ctx, m))
                })
                .or_else(|| ctx.ensure_class_initialized(&comp_name).ok())
                .unwrap_or(cratonvm_types::ClassId::new(0));
            crate::new_ref_array_reclaiming(ctx, comp_id, length, &mut [], "Array.newInstance")?
        }
    };
    Ok(Some(Value::Object(Some(arr))))
}

/// `Array.newArray`'s component name read the long way (the mirror's name,
/// then its legacy `read_string` / slot-1 readers), `/`-separated;
/// `java/lang/Object` when nothing answers.
fn comp_name_slow(
    ctx: &dyn NativeContext,
    comp_mirror: Option<cratonvm_types::ObjectRef>,
) -> String {
    match comp_mirror {
        Some(mirror) => crate::lang_class::mirror_class_name(ctx, mirror)
            .filter(|s| !s.is_empty())
            .or_else(|| ctx.read_string(mirror))
            .or_else(|| match ctx.get_field(mirror, 1) {
                Value::Object(Some(name_obj)) => ctx.read_string(name_obj),
                _ => None,
            })
            .map(|s| s.replace('.', "/"))
            .unwrap_or_else(|| "java/lang/Object".to_string()),
        _ => "java/lang/Object".to_string(),
    }
}

/// `java/lang/reflect/Array.multiNewArray(Class componentType, int[] dimensions)`
///
/// Allocates a fully-materialized multi-dimensional array. Distinct from the
/// single-dim `newArray`: the result's runtime class must be the *precise*
/// nested array type вЂ” `Array.newInstance(String.class, {2,2})` в†’
/// `[[Ljava/lang/String;`, not `[Ljava/lang/String;` (SpEL
/// `ArrayConstructorTests.multiDimensionalArrays` asserts this exactly).
///
/// Each non-leaf level is therefore allocated with the resolved nested-array
/// component `ClassId` (via `ensure_class_initialized` on the `[вЂ¦` descriptor)
/// rather than the loose `ClassId(0)` the `multianewarray` *bytecode* path
/// uses вЂ” bytecode-built multiarrays are rarely inspected via `getClass()`,
/// reflective ones are.
///
/// Previously this descriptor (and `Array.newInstance(Class, int[])`) was wired
/// to a single-dim allocator that read only `dims[0]`, collapsing the result to
/// one dimension.
pub(crate) fn native_array_multi_new_array(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    use cratonvm_types::error::MethodCallFailed;

    // HotSpot's `Reflection::reflect_new_multi_array` checks, in this order
    // (probe `tools/probes/interp/L4/L4W29ReflectNewArray.java`, rows 9-29):
    // a null component or a null dimensions array is a
    // `NullPointerException`; zero or more than 255 dimensions is an
    // `IllegalArgumentException`; then each negative dimension, in order, is
    // `NegativeArraySizeException`; then a `void` component, or an array
    // component whose own dimensions plus these exceed 255, is an
    // `IllegalArgumentException`. All the `IllegalArgumentException`s carry
    // no message. Before wave 29 an empty dimensions array or a null one
    // answered `int[0]`, a null component built `Object[]...`, and 256
    // dimensions were built.
    let comp_mirror = match (args.first(), args.get(1)) {
        (Some(Value::Object(Some(m))), Some(Value::Object(Some(_)))) => *m,
        // Defensive: a single Int (1-D shape) вЂ” defer to the single-dim path.
        (_, Some(Value::Int(_))) => return native_array_new_array(ctx, args),
        _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
    };
    // args[1] = int[] of per-dimension lengths.
    let dims_arr = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Err(RuntimeError::NullPointerException { message: None }.into()),
    };
    let ndims = ctx.array_length(dims_arr);
    if ndims == 0 || ndims > REFLECT_ARRAY_MAX_DIMENSIONS {
        return Err(RuntimeError::IllegalArgumentException {
            message: String::new(),
        }
        .into());
    }
    if ndims == 1 {
        // A single dimension behaves exactly like `newArray`; reuse it so the
        // (already well-tested) Class-mirror element-type resolution applies.
        // Its checks are the same ones in the same order for one dimension.
        let one_dim = [
            Value::Object(Some(comp_mirror)),
            ctx.get_array_element(dims_arr, 0),
        ];
        return native_array_new_array(ctx, &one_dim);
    }

    let mut sizes: Vec<usize> = Vec::with_capacity(ndims);
    for i in 0..ndims {
        match ctx.get_array_element(dims_arr, i) {
            Value::Int(n) => {
                if n < 0 {
                    return Err(RuntimeError::NegativeArraySizeException { size: n }.into());
                }
                sizes.push(n as usize);
            }
            _ => sizes.push(0),
        }
    }

    // Resolve the leaf component type from the Class mirror (mirrors the
    // resolution in `native_array_new_array`).
    // A lambda proxy is virtual: its mirror's name is a placeholder, so name it as its arrays
    // are named and let the class manager know it (SBR5 walker).
    let lambda_leaf = crate::lang_class::mirror_class_id(&*ctx, comp_mirror)
        .and_then(|cid| crate::lang_class::register_lambda_array_component(ctx, cid));
    let comp_name = lambda_leaf
        .or_else(|| {
            crate::lang_class::mirror_class_name(&*ctx, comp_mirror).filter(|s| !s.is_empty())
        })
        .or_else(|| ctx.read_string(comp_mirror))
        .map(|s| s.replace('.', "/"))
        .unwrap_or_else(|| "java/lang/Object".to_string());

    // Either a primitive leaf (single-letter descriptor) or a reference leaf
    // (`L...;`, or the array class's own descriptor when the component is
    // itself an array: `int[].class` is `[I`, not `L[I;`). The descriptor is
    // what the nested array-class names are built from.
    let (prim_et, leaf_desc) = match comp_name.as_str() {
        "int" => (Some(cratonvm_types::ArrayElementType::Int), "I".to_string()),
        "long" => (
            Some(cratonvm_types::ArrayElementType::Long),
            "J".to_string(),
        ),
        "float" => (
            Some(cratonvm_types::ArrayElementType::Float),
            "F".to_string(),
        ),
        "double" => (
            Some(cratonvm_types::ArrayElementType::Double),
            "D".to_string(),
        ),
        "boolean" => (
            Some(cratonvm_types::ArrayElementType::Boolean),
            "Z".to_string(),
        ),
        "byte" => (
            Some(cratonvm_types::ArrayElementType::Byte),
            "B".to_string(),
        ),
        "char" => (
            Some(cratonvm_types::ArrayElementType::Char),
            "C".to_string(),
        ),
        "short" => (
            Some(cratonvm_types::ArrayElementType::Short),
            "S".to_string(),
        ),
        other if other.starts_with('[') => (None, other.to_string()),
        other => (None, format!("L{other};")),
    };
    if prim_et.is_none() && reflect_new_array_refuses_component(&comp_name, ndims) {
        return Err(RuntimeError::IllegalArgumentException {
            message: String::new(),
        }
        .into());
    }

    // Reference leaf: the base component class, used for the innermost
    // `String[]`-style array. Primitives ignore it. The mirror's OWN class
    // first, as `native_array_new_array` does: re-resolving the name ran the
    // component's `<clinit>` (HotSpot's `Array.newInstance` initializes
    // nothing), picked the caller's namespace for another loader's class, and
    // found nothing at all for a hidden class. The by-name lookup stays only
    // for mirrors the reverse map does not know (synthesized / legacy ones).
    let base_ref_id = if prim_et.is_none() {
        crate::lang_class::mirror_class_id(&*ctx, comp_mirror)
            .or_else(|| ctx.ensure_class_initialized(&comp_name).ok())
            .unwrap_or(cratonvm_types::ClassId::new(0))
    } else {
        cratonvm_types::ClassId::new(0)
    };

    // The component class of each intermediate level, resolved once per
    // level (not once per sub-array): level `k` holds arrays of
    // `'[' * (ndims - 1 - k) + leaf`. A reference leaf's nested array classes
    // are the ones its DEFINING loader sees (JVMS §5.3.3): for a class of a
    // user-defined loader, or a hidden class, the name alone resolves to
    // nothing or to another class. The by-name path stays as the fallback
    // (primitive leaves, and contexts with no loader-faithful answer).
    let leaf_loader = if prim_et.is_none() && base_ref_id != cratonvm_types::ClassId::new(0) {
        u32::try_from(ctx.loader_id_of_class(base_ref_id)).ok()
    } else {
        None
    };
    let mut level_components: Vec<cratonvm_types::ClassId> = Vec::with_capacity(ndims - 1);
    for level in 0..ndims - 1 {
        let comp_desc: String = "[".repeat(ndims - 1 - level) + &leaf_desc;
        let faithful = match leaf_loader {
            Some(loader) => ctx.array_class_id_for_loader(&comp_desc, loader),
            None => None,
        };
        let comp_id = match faithful {
            Some(id) => id,
            None => ctx
                .ensure_class_initialized(&comp_desc)
                .unwrap_or(cratonvm_types::ClassId::new(0)),
        };
        level_components.push(comp_id);
    }

    fn build(
        ctx: &mut dyn NativeContext,
        sizes: &[usize],
        level: usize,
        prim_et: Option<cratonvm_types::ArrayElementType>,
        base_ref_id: cratonvm_types::ClassId,
        level_components: &[cratonvm_types::ClassId],
    ) -> Result<ObjectRef, MethodCallFailed> {
        let ndims = sizes.len();
        let len = sizes[level];
        if level == ndims - 1 {
            // Innermost specified dimension: leaf array of the base type.
            let arr = match prim_et {
                Some(et) => ctx.new_array(et, len),
                None => ctx.new_ref_array(base_ref_id, len),
            };
            return Ok(arr);
        }
        // Intermediate level: a reference array whose component is the nested
        // array type one level down.
        let mut arr = ctx.new_ref_array(level_components[level], len);
        // Pin the parent across each sub-array allocation: under a moving
        // collector `arr` may relocate while `build` allocates.
        let pin = ctx.pin_native_root(arr);
        for i in 0..len {
            arr = ctx.read_native_pin(pin, arr);
            let sub = build(
                ctx,
                sizes,
                level + 1,
                prim_et,
                base_ref_id,
                level_components,
            )?;
            arr = ctx.read_native_pin(pin, arr);
            ctx.set_array_element(arr, i, Value::Object(Some(sub)));
        }
        ctx.unpin_native_roots(pin);
        Ok(arr)
    }

    let arr = build(ctx, &sizes, 0, prim_et, base_ref_id, &level_components)?;
    Ok(Some(Value::Object(Some(arr))))
}

/// JVMS §4.3.2: an array type has at most 255 dimensions. HotSpot's
/// `Reflection` refuses a reflective allocation that would exceed it with an
/// `IllegalArgumentException` (no message).
const REFLECT_ARRAY_MAX_DIMENSIONS: usize = 255;

/// Whether `Array.newInstance` refuses a reference (non-primitive-array)
/// component named `comp_name` for an allocation of `added_dims` dimensions:
/// `void` (there is no `void[]`), or an array component whose own dimensions
/// plus `added_dims` exceed 255. Only reached off the primitive arms, so the
/// common `Arrays.copyOf` path pays a short string compare.
pub(crate) fn reflect_new_array_refuses_component(comp_name: &str, added_dims: usize) -> bool {
    if comp_name == "void" || comp_name == "V" {
        return true;
    }
    if !comp_name.starts_with('[') {
        return false;
    }
    let own_dims = comp_name.bytes().take_while(|&b| b == b'[').count();
    own_dims + added_dims > REFLECT_ARRAY_MAX_DIMENSIONS
}

const CLASS_FILE_MAGIC: [u8; 4] = [0xCA, 0xFE, 0xBA, 0xBE];

fn define_class_format_error(class_name: &str, method: &str, message: String) -> MethodCallFailed {
    LinkageError::ClassFormatError {
        class_name: class_name.to_string(),
        message: format!("{method}: {message}"),
    }
    .into()
}

// ---------------------------------------------------------------------------
// defineClass0/1/2 — recovering the TYPE of a backend failure (W7-31 §Falsifier 3)
//
// `NativeContext::define_class_full` is typed `Result<ClassId, String>`, and the
// VM's implementation fills that `String` with
// `class_manager::define_class_with_options`'s `VmError` rendered by
// `.map_err(|e| format!("{e:?}"))` — a Rust `Debug` string. Every `defineClassN`
// failure arm then re-wrapped it as `ClassFormatError`, so a class file that
// must raise `UnsupportedClassVersionError` produced instead:
//
//   HotSpot : java.lang.UnsupportedClassVersionError: Preview features are not
//             enabled for <Unknown> (class file version 69.65535). Try running
//             with '--enable-preview'
//   CratonVM: java.lang.ClassFormatError: : defineClass1:
//             Linkage(UnsupportedClassVersionError { class_name: "", message:
//             "Preview features are not enabled for <Unknown> (class file
//             version 69.65535). Try running with '--enable-preview'" })
//
// Two separate breakages in one line. The TYPE is wrong — a container catching
// the JDK's typed exception (application servers probing whether they can load
// a bundle, test frameworks branching on linkage kind) does not catch ours — and
// the MESSAGE is a Rust value dump.
//
// The right repair is to widen `define_class_full` to carry `VmError`; that is a
// trait-signature change across ~25 call sites in six crates and is nominated,
// not done here. What is done here is the repair the record asks for: give the
// `defineClassN` tail a pass-through that recovers the already-typed
// `VmError::Linkage(..)` from the rendering it was flattened into, and rebuild
// the typed variant so `runtime::exceptions::linkage_throwable` — which already
// has a complete, correct arm per variant — produces the JDK exception.
//
// **Never re-emit the Debug text.** Every arm below either carries a field the
// backend wrote (already human-readable — `LinkageError`'s own `#[error]`
// strings and HotSpot's verbatim version wording) or names the variant. The raw
// `msg` is used only when the string is NOT a `Debug` rendering at all, which is
// the case for `define_class_full`'s own plain-string failures ("define_class_full
// failed for X", "initialize after define failed for X: ...").
//
// **`class_name: ""` is not a lost name.** It is the caller's own argument: the
// Java call was `ClassLoader.defineClass(null, bytes, off, len)`, and the class
// manager's version check runs BEFORE `this_class` is read from the constant
// pool, so no name exists to substitute. HotSpot has the identical ordering and
// prints `<Unknown>` mid-message — which the recovered `message` field already
// contains. What was visibly damaged was the outer wrapper's
// `format!("{class_name}: {message}")`, i.e. the stray leading `": "` above;
// that disappears with the flattening, because the `UnsupportedClassVersionError`
// arm of `linkage_throwable` deliberately does not prefix the name.
// ---------------------------------------------------------------------------

/// Split a Rust `Debug` rendering of an enum into `(outer, inner, body)`.
///
/// `Linkage(ClassFormatError { class_name: "A", message: "b" })`
///   -> `("Linkage", "ClassFormatError", "class_name: \"A\", message: \"b\"")`
///
/// A tuple variant with no struct body (`JdkOnly(..)`) yields an empty body.
/// Returns `None` for anything that is not shaped like `Ident(..)` — a plain
/// human-written error string, which the caller must pass through unchanged.
fn split_debug_error(msg: &str) -> Option<(&str, &str, &str)> {
    let open = msg.find('(')?;
    let outer = &msg[..open];
    if outer.is_empty() || !outer.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    let close = msg.rfind(')')?;
    if close <= open {
        return None;
    }
    let inner_all = &msg[open + 1..close];
    // `#[derive(Debug)]` renders a struct variant as `Name { a: 1, b: 2 }`,
    // with exactly one space inside each brace.
    match inner_all.find(" { ") {
        Some(brace) => {
            let end = inner_all.rfind(" }")?;
            if end < brace + 3 {
                return None;
            }
            Some((outer, &inner_all[..brace], &inner_all[brace + 3..end]))
        }
        None => Some((outer, inner_all, "")),
    }
}

/// Undo the escaping `Debug` applies to a `String`, stopping at the closing
/// quote. `\u{..}` forms are not decoded — they are rare (control characters in
/// a class name) and a literal `u{7f}` in a diagnostic is better than a partial
/// parse that drops the rest of the sentence.
fn unescape_debug_string(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('0') => out.push('\0'),
                // Covers `\\`, `\"` and `\'`.
                Some(other) => out.push(other),
                None => break,
            },
            other => out.push(other),
        }
    }
    out
}

/// Read a `name: "value"` `String` field out of a `Debug` struct body.
///
/// The name must sit at a field boundary (start of the body, or just after a
/// `", "` separator) so a `field:` appearing INSIDE another field's text cannot
/// be mistaken for the field itself.
fn debug_string_field(body: &str, field: &str) -> Option<String> {
    let mut from = 0usize;
    while from < body.len() {
        let at = from + body[from..].find(field)?;
        let after = at + field.len();
        let at_boundary = at == 0 || body[..at].ends_with(", ");
        if at_boundary && body[after..].starts_with(": \"") {
            return Some(unescape_debug_string(&body[after + 3..]));
        }
        from = after;
    }
    None
}

/// Recover a typed, catchable `java.lang.ExceptionInInitializerError` for an
/// `initialize=true` define-class failure, instead of the message-only
/// `IllegalStateException` with no `getCause()` this used to build.
///
/// `msg` is the backend's own
/// `"initialize after define failed for {name}: {cause}"` string, where
/// `{cause}` is `describe_throwable`'s `"{internal/class/Name}: {message}"`
/// rendering (bare `{internal/class/Name}` when the Throwable carries no
/// message). See
/// defineclass0-two-registrations-and-the-init-contract-20260912.md
/// §5: HotSpot raises `ExceptionInInitializerError` wrapping the real
/// `<clinit>` exception; this VM raised `IllegalStateException` with `<none>`
/// as the cause, so a caller catching the JDK's type or reading `getCause()`
/// got neither.
///
/// The ORIGINAL exception object is gone by the time the backend has
/// flattened it to this string (`NativeContext::define_class_full` is typed
/// `Result<ClassId, String>` — see the module note above `split_debug_error`).
/// Best-effort recovery: build a NEW instance of the cause's own class from
/// its recovered name and message, which matches HotSpot's `getCause()`
/// class and text for the common case of a plain exception thrown from
/// `<clinit>` (exactly what `apps/probes/L2InitFail.java` exercises), even
/// though it is not the identical object HotSpot would hand back. Falls back
/// to a plain `RuntimeException` carrying the raw cause text when the named
/// class cannot be recovered (not loaded, or no matching constructor) —
/// still typed and catchable, unlike the previous `<none>`.
pub(crate) fn exception_in_initializer_error(
    ctx: &mut dyn NativeContext,
    hidden_name: &str,
    msg: &str,
) -> MethodCallFailed {
    let prefix = format!("initialize after define failed for {hidden_name}: ");
    let cause_desc = msg.strip_prefix(&prefix).unwrap_or(msg);
    let (cause_class, cause_message) = match cause_desc.split_once(": ") {
        Some((class, message)) => (class, Some(message)),
        None => (cause_desc, None),
    };
    let cause_obj = build_throwable(ctx, cause_class, cause_message)
        .or_else(|| build_throwable(ctx, "java/lang/RuntimeException", Some(cause_desc)));
    let fallback = || {
        RuntimeError::IllegalStateException {
            message: format!("ExceptionInInitializerError for {hidden_name}: {msg}"),
        }
        .into()
    };
    match cause_obj {
        Some(cause) => match ctx.new_object_initialized(
            "java/lang/ExceptionInInitializerError",
            "(Ljava/lang/Throwable;)V",
            &[Value::Object(Some(cause))],
        ) {
            Ok(Some(Value::Object(Some(exc)))) => MethodCallFailed::ExceptionThrown(exc),
            _ => fallback(),
        },
        None => fallback(),
    }
}

/// Build a `class_name` instance for [`exception_in_initializer_error`],
/// preferring `(String)` when `message` is present and falling back to the
/// no-arg constructor. Only classes already loaded — never triggers a fresh
/// class load, so a cause type that itself fails to load cannot recurse into
/// the same failure this function exists to report. `None` on any failure
/// (not loaded, no matching constructor, construction threw) — the caller
/// treats that as "try the next fallback", not as a hard error.
fn build_throwable(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    message: Option<&str>,
) -> Option<ObjectRef> {
    ctx.class_id_by_name(class_name)?;
    let result = match message {
        Some(m) => {
            let s = ctx.create_string(m);
            ctx.new_object_initialized(
                class_name,
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(s))],
            )
        }
        None => ctx.new_object_initialized(class_name, "()V", &[]),
    };
    match result {
        Ok(Some(Value::Object(Some(obj)))) => Some(obj),
        _ => None,
    }
}

/// The FORMAT half of [`define_class_linkage_error`], for the callers that must
/// not take the rest.
///
/// `MethodHandles.Lookup.define{,Hidden}Class` is specified with a SPLIT
/// contract, and the split falls exactly on `VmError`'s Linkage/Runtime line:
///
/// * bytes that are not a well-formed ClassFile  -> `ClassFormatError`
/// * bytes that name a class in a different package from the lookup class, or
///   a lookup without the right mode -> `IllegalArgumentException`
///
/// So those doors cannot use `define_class_linkage_error`, which re-types
/// EVERY recovered variant: the backend reports a define into `java.util` as
/// `RuntimeError::SecurityException` (JVMS 5.3.5, and correct for the
/// `ClassLoader` door), and handing that to Java would answer
/// `SecurityException` where the JDK answers `IllegalArgumentException`. This
/// returns `Some` only for the linkage/format family, leaving the caller's own
/// `IllegalArgumentException` wrapper in place for everything else.
///
/// Measured, not assumed: `probes/DynClassGenSweep.java` carries a
/// `define.lookupWrongPackage` row precisely so a change on this surface that
/// over-reaches is visible as a regression rather than as a silent one.
pub(crate) fn lookup_define_format_error(
    class_name: &str,
    method: &str,
    msg: &str,
) -> Option<MethodCallFailed> {
    match typed_define_class_error(class_name, method, msg) {
        Some(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Linkage(e))) => Some(
            MethodCallFailed::InternalError(cratonvm_types::error::VmError::Linkage(e)),
        ),
        // `typed_define_class_error` already re-homes every `ClassFile` variant
        // onto a `Linkage` one (a `ClassFile` variant is UNCATCHABLE), so the
        // arm above is the whole format family.
        _ => None,
    }
}

/// zt t7/cl3: the state of `System.out` and `System.err` as the program's own
/// bytecode sees them. Printed by the launcher after `main` under
/// `CRATONVM_DBG_PRINT_LOSS=1`, so a run whose output went nowhere can be
/// told apart in one pass. The cases it separates:
/// * the `System` static is not the VM's stream (`same_as_vm=false`);
/// * the stream lost its wiring (`out` / `textOut` / `charOut` null), so the
///   JDK's `PrintStream` bytecode throws `IOException` in `ensureOpen()` and
///   absorbs it into `trouble` (`install_real_stream_fields`' doc: every write
///   is discarded and the VM exits 0);
/// * a write failed (`trouble=1`);
/// * the `FileOutputStream` at the bottom has no descriptor the VM can resolve
///   (`fd=-` / `handle=-`). `native-io`'s write natives then return without
///   writing.
///
/// Reads only; allocates nothing.
pub fn system_stream_report(ctx: &dyn NativeContext) -> Vec<String> {
    let class_of = |obj: ObjectRef| {
        ctx.class_name_arc_of_id(ctx.class_id_of_object(obj))
            .map(|n| n.to_string())
            .unwrap_or_else(|| "?".to_string())
    };
    let field_text = |obj: ObjectRef, name: &str| match ctx.get_field_by_name(obj, name) {
        Value::Object(Some(o)) => class_of(o),
        Value::Object(None) => "null".to_string(),
        Value::Int(v) => v.to_string(),
        Value::Long(v) => v.to_string(),
        other => format!("{other:?}"),
    };
    let system = ctx.class_id_by_name("java/lang/System");
    let mut lines = Vec::new();
    for name in ["out", "err"] {
        let java = system
            .and_then(|cid| {
                ctx.static_field_index_by_name(cid, name)
                    .map(|i| ctx.get_static_field(cid, i))
            })
            .and_then(|v| match v {
                Value::Object(o) => o,
                _ => None,
            });
        let vm = ctx.get_system_stream(name);
        let same = match (java, vm) {
            (Some(a), Some(b)) => a.as_ptr() == b.as_ptr(),
            (None, None) => true,
            _ => false,
        };
        let Some(stream) = java else {
            lines.push(format!("System.{name}: null same_as_vm={same}"));
            continue;
        };
        // The descriptor under `out` (a `FileOutputStream`, or one level down
        // through a `FilterOutputStream`'s own `out`).
        let mut sink = match ctx.get_field_by_name(stream, "out") {
            Value::Object(Some(o)) => Some(o),
            _ => None,
        };
        if let Some(s) = sink {
            if !matches!(ctx.get_field_by_name(s, "fd"), Value::Object(Some(_))) {
                if let Value::Object(Some(inner)) = ctx.get_field_by_name(s, "out") {
                    sink = Some(inner);
                }
            }
        }
        let fd = match sink.map(|s| ctx.get_field_by_name(s, "fd")) {
            Some(Value::Object(Some(fd_obj))) => format!(
                "fd={} handle={}",
                field_text(fd_obj, "fd"),
                field_text(fd_obj, "handle")
            ),
            _ => "fd=- handle=-".to_string(),
        };
        lines.push(format!(
            "System.{name}: class={} same_as_vm={same} out={} textOut={} charOut={} \
             charset={} trouble={} closing={} autoFlush={} sink={} {fd}",
            class_of(stream),
            field_text(stream, "out"),
            field_text(stream, "textOut"),
            field_text(stream, "charOut"),
            field_text(stream, "charset"),
            field_text(stream, "trouble"),
            field_text(stream, "closing"),
            field_text(stream, "autoFlush"),
            sink.map(class_of).unwrap_or_else(|| "-".to_string()),
        ));
    }
    lines
}

/// zt t7/cl3: [`system_stream_report`] answers one line per stream, and says
/// "null" for a `System` it cannot read (a mock has no statics) instead of
/// skipping it: a report that went quiet would read as "nothing wrong".
#[cfg(test)]
mod zt7cl3_system_stream_report_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;

    #[test]
    fn zt7cl3_the_report_has_a_line_per_stream_even_when_nothing_resolves() {
        let ctx = MockNativeContext::new();
        let lines = system_stream_report(&ctx);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].starts_with("System.out: "), "{lines:?}");
        assert!(lines[1].starts_with("System.err: "), "{lines:?}");
    }
}

/// zs s2/nd2 (`zgc-zr11nd-native-oom-replay-residuals-20261013.md` §1, class
/// definers): the mirror of a class a definer has just defined, minted with a
/// collection before the `OutOfMemoryError` on ZGC.
///
/// A definer cannot be on the funnel's restartable list (a re-run defines the
/// class again: `LinkageError`), and its mirror is minted AFTER the define, so
/// on a full heap the one-attempt mirror door raised `OutOfMemoryError` without
/// a collection, leaving a defined class whose `Class` object Java never saw.
/// HotSpot allocates the mirror inside the define, after a collection if need
/// be. The callers hold every reference they still use in a pin (loader,
/// protection domain, class data) and nothing raw across this call -- the
/// define before it is already a GC point -- so the reclaim ladder's
/// precondition holds. ZGC only: gci i48/zl2 made
/// `native_doors_collect_on_refusal` true on Generational and G1 too, and the
/// zt round keeps dev's one-attempt mirror door there (zt t1, page
/// `zgc-zt1nd-define-and-door-residuals-after-the-merge-SUPERSEDED-20261019.md` item 2).
/// `CRATONVM_ZGC_DEFINE_MIRROR_COLLECT=0` restores the one-attempt door on ZGC.
/// Spent ladder: the one-attempt door raises the error as before.
pub(crate) fn defined_class_mirror(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
) -> ObjectRef {
    if !ctx.heap_is_zgc()
        || !ctx.native_doors_collect_on_refusal()
        || !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_DEFINE_MIRROR_COLLECT")
    {
        return ctx.get_class_mirror(class_id);
    }
    if let Some(mirror) = ctx.try_get_class_mirror(class_id) {
        return mirror;
    }
    if let Some(mirror) = crate::util_concurrent_ext::retry_after_reclaim(ctx, |ctx| {
        ctx.try_get_class_mirror(class_id)
    }) {
        return mirror;
    }
    ctx.get_class_mirror(class_id)
}

/// zs s7/nd6 (`zgc-zs2nd2-proposal-mirror-inside-the-define-20261014.md`,
/// "Variant: a reserved mirror"): reserve the mirror object of a named class
/// about to be defined, BEFORE `define_class_full`, through the fallible door
/// and the reclaim ladder; a spent ladder is the define's `OutOfMemoryError`
/// with nothing defined, as HotSpot's (the mirror is created inside
/// `SystemDictionary::define_instance_class`), so the program's retry defines
/// the class instead of meeting `LinkageError: duplicate class definition`.
/// `Ok(Some((pin, object)))`: pinned; hand it to
/// [`take_mirror_reservation`] right after the define. `Ok(None)`: no
/// reservation (not ZGC, a switch off, or a context that does not reserve):
/// the define mints its mirror afterwards, as before. The caller must hold
/// no unpinned reference (the ladder collects).
/// `CRATONVM_ZGC_DEFINE_MIRROR_RESERVED=0` (default on) is the kill.
pub(crate) fn reserve_defined_class_mirror(
    ctx: &mut dyn NativeContext,
    internal_name: &str,
) -> Result<Option<(usize, ObjectRef)>, MethodCallFailed> {
    use cratonvm_native_api::MirrorReservation;
    if !ctx.native_doors_collect_on_refusal()
        || !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_DEFINE_MIRROR_COLLECT")
        || !cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_DEFINE_MIRROR_RESERVED")
    {
        return Ok(None);
    }
    let pooled = (!internal_name.is_empty()).then_some(internal_name);
    let reserved = match ctx.try_reserve_class_mirror(pooled) {
        MirrorReservation::Unsupported => return Ok(None),
        MirrorReservation::Reserved(mirror) => mirror,
        MirrorReservation::Refused => {
            match crate::util_concurrent_ext::retry_after_reclaim(ctx, |ctx| {
                match ctx.try_reserve_class_mirror(pooled) {
                    MirrorReservation::Reserved(mirror) => Some(mirror),
                    _ => None,
                }
            }) {
                Some(mirror) => mirror,
                // HotSpot's message for a refused mirror inside a define.
                None => {
                    return Err(RuntimeError::OutOfMemoryError {
                        message: "Java heap space".to_string(),
                    }
                    .into())
                }
            }
        }
    };
    Ok(Some((ctx.pin_native_root(reserved), reserved)))
}

/// zs s7/nd6: the reserved mirror's current address, unpinned (read it right
/// after the define, before anything else allocates).
pub(crate) fn take_mirror_reservation(
    ctx: &mut dyn NativeContext,
    reservation: Option<(usize, ObjectRef)>,
) -> Option<ObjectRef> {
    let (pin, object) = reservation?;
    let object = ctx.read_native_pin(pin, object);
    ctx.unpin_native_roots(pin);
    Some(object)
}

/// [`defined_class_mirror`] that adopts a reserved mirror object first (zs
/// s7/nd6); call it with nothing allocated since [`take_mirror_reservation`].
pub(crate) fn defined_class_mirror_reserved(
    ctx: &mut dyn NativeContext,
    class_id: cratonvm_types::ClassId,
    reserved: Option<ObjectRef>,
) -> ObjectRef {
    if let Some(reserved) = reserved {
        if let Some(mirror) = ctx.adopt_reserved_class_mirror(class_id, reserved) {
            return mirror;
        }
    }
    defined_class_mirror(ctx, class_id)
}

/// zs s8/nd7 (`zgc-zs7nd6-native-door-residuals-FIXED-20261019.md` §1): whether the
/// definers other than `defineClass1/2` reserve their mirror too.
/// `CRATONVM_ZGC_DEFINE_MIRROR_RESERVED_EVERY_DEFINER=0` (default on) is their
/// kill; nd6's `CRATONVM_ZGC_DEFINE_MIRROR_RESERVED=0` turns every door off.
fn every_definer_reserves_mirror() -> bool {
    cratonvm_types::flags::runtime_flag_default_on(
        "CRATONVM_ZGC_DEFINE_MIRROR_RESERVED_EVERY_DEFINER",
    )
}

/// zs s9/nd8: whether a hidden define parks its name in the reserved mirror
/// (`CRATONVM_ZGC_DEFINE_MIRROR_HIDDEN_NAME_PARKED`, default on, `=0` kill;
/// needs `CRATONVM_ZGC_DEFINE_MIRROR_NAME_POOLED_AT_ADOPTION`, whose `=0`
/// would intern the parked name).
fn hidden_name_parked() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_DEFINE_MIRROR_HIDDEN_NAME_PARKED")
        && cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_ZGC_DEFINE_MIRROR_NAME_POOLED_AT_ADOPTION",
        )
}

/// zs s9/nd8 (`zgc-zs8nd7-define-mirror-residuals-FIXED-20261019.md` §1): the name
/// a define's mirror reservation parks. A NAMELESS define
/// (`defineClass(null, bytes, ..)`, `Unsafe.defineClass(null, ..)`) takes
/// the class file's own `this_class`, so the adoption does not allocate the
/// name after the define (a refusal there left the class defined and the
/// retry met `LinkageError`). A wrong guess costs nothing: the adoption
/// reuses a parked name only when it equals the defined class's.
/// `CRATONVM_ZGC_DEFINE_MIRROR_NAME_FROM_BYTES=0` (default on) parks none.
pub(crate) fn define_name_hint<'a>(name: &'a str, bytes: &[u8]) -> std::borrow::Cow<'a, str> {
    if !name.is_empty()
        || !cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_ZGC_DEFINE_MIRROR_NAME_FROM_BYTES",
        )
    {
        return std::borrow::Cow::Borrowed(name);
    }
    match crate::classloader::extract_this_class_name(bytes) {
        Some(this_class) => std::borrow::Cow::Owned(this_class),
        None => std::borrow::Cow::Borrowed(name),
    }
}

/// zs s8/nd7: [`reserve_defined_class_mirror`] for a definer whose
/// `DefineClassFull` is already built. The reservation can collect, so the
/// one reference `opts` carries -- `protection_domain`, which the define
/// reads at its entry (the load-time transformers are handed it) -- is
/// pinned across it and written back current. `name_hint` is the class's
/// internal name when the caller knows it (pooled now, so the adoption's name
/// lookup is a pool hit); a hidden define pools nothing (its name String is
/// not interned). The caller holds no other unpinned reference across this
/// call, and hands the answer to [`take_mirror_reservation`] right after the
/// define, then to [`defined_class_mirror_reserved`] with nothing allocated in
/// between.
pub(crate) fn reserve_mirror_for_define(
    ctx: &mut dyn NativeContext,
    name_hint: &str,
    opts: &mut cratonvm_native_api::DefineClassFull,
) -> Result<Option<(usize, ObjectRef)>, MethodCallFailed> {
    if !every_definer_reserves_mirror() {
        return Ok(None);
    }
    // zs s9/nd8 (`zgc-zs8nd7-define-mirror-residuals-FIXED-20261019.md` §1): a
    // hidden class's (mangled) name is parked too, UNPOOLED -- the VM parks
    // an unpooled String when the name is not in the pool, and only while
    // `CRATONVM_ZGC_DEFINE_MIRROR_NAME_POOLED_AT_ADOPTION` is on (its `=0`
    // pools at the reservation, which a hidden name must never be) -- so the
    // adoption reuses it instead of allocating after the define.
    let pooled = if opts.hidden && !hidden_name_parked() {
        ""
    } else {
        opts.override_name.as_deref().unwrap_or(name_hint)
    };
    let pooled = pooled.to_string();
    let Some(pd) = opts.protection_domain else {
        return reserve_defined_class_mirror(ctx, &pooled);
    };
    let pd_pin = ctx.pin_native_root(pd);
    let reservation = reserve_defined_class_mirror(ctx, &pooled);
    opts.protection_domain = Some(ctx.read_native_pin(pd_pin, pd));
    match reservation {
        Ok(Some((mirror_pin, mirror))) => {
            // Pins are a stack: re-pin the mirror at the domain's slot so the
            // caller's single `take_mirror_reservation` releases both. Nothing
            // allocates between the release and the re-pin.
            let mirror = ctx.read_native_pin(mirror_pin, mirror);
            ctx.unpin_native_roots(pd_pin);
            Ok(Some((ctx.pin_native_root(mirror), mirror)))
        }
        other => {
            ctx.unpin_native_roots(pd_pin);
            other
        }
    }
}

/// Turn a `define_class_full` failure string back into the typed JVM error the
/// backend actually raised.
///
/// `class_name` is the caller's own name argument, used only when the recovered
/// error names nothing (or names nothing useful). See the module note above for
/// why an empty name here is faithful rather than lost.
fn define_class_linkage_error(class_name: &str, method: &str, msg: String) -> MethodCallFailed {
    match typed_define_class_error(class_name, method, &msg) {
        Some(err) => err,
        // Not a `Debug` rendering — `define_class_full`'s own plain-string
        // failures land here, and they are already readable.
        None => define_class_format_error(class_name, method, msg),
    }
}

/// The recovery half of [`define_class_linkage_error`]. Split out so the
/// borrow of `msg` that [`split_debug_error`] produces ends before the caller
/// needs to move the `String` into its fallback.
fn typed_define_class_error(class_name: &str, method: &str, msg: &str) -> Option<MethodCallFailed> {
    let (outer, inner, body) = split_debug_error(msg)?;

    // The backend's own name when it has one (a supertype-resolution failure
    // names the SUPERTYPE, not the class being defined), else the caller's.
    let named = |field: &str| {
        debug_string_field(body, field)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| class_name.to_string())
    };
    // A recovered variant always has a readable message: the field the backend
    // wrote, or — if the field cannot be read — the variant's own name. Never
    // `msg`, which is the `Debug` text this function exists to remove.
    let detail = |field: &str| {
        debug_string_field(body, field).unwrap_or_else(|| format!("{method}: {inner}"))
    };

    Some(match (outer, inner) {
        // -- VmError::Linkage: already the right shape, just re-typed. --------
        ("Linkage", "UnsupportedClassVersionError") => LinkageError::UnsupportedClassVersionError {
            class_name: named("class_name"),
            // HotSpot's wording verbatim; it already names the class
            // mid-sentence, which is why `linkage_throwable` does not prefix.
            message: detail("message"),
        }
        .into(),
        ("Linkage", "ClassFormatError") => LinkageError::ClassFormatError {
            class_name: named("class_name"),
            message: detail("message"),
        }
        .into(),
        ("Linkage", "VerifyError") => LinkageError::VerifyError {
            class_name: named("class_name"),
            method_name: debug_string_field(body, "method_name").unwrap_or_default(),
            message: detail("message"),
        }
        .into(),
        ("Linkage", "NoClassDefFoundError") => LinkageError::NoClassDefFoundError {
            class_name: named("class_name"),
        }
        .into(),
        ("Linkage", "IncompatibleClassChangeError") => LinkageError::IncompatibleClassChangeError {
            message: detail("message"),
        }
        .into(),
        ("Linkage", "ClassCircularityError") => LinkageError::ClassCircularityError {
            class_name: named("class_name"),
        }
        .into(),
        ("Linkage", "DuplicateClassDefinition") => LinkageError::DuplicateClassDefinition {
            class_name: named("class_name"),
            loader: debug_string_field(body, "loader").unwrap_or_else(|| "<unknown>".to_string()),
        }
        .into(),
        ("Linkage", "NoSuchFieldError") => LinkageError::NoSuchFieldError {
            class_name: named("class_name"),
            field_name: debug_string_field(body, "field_name").unwrap_or_default(),
            field_descriptor: debug_string_field(body, "field_descriptor").unwrap_or_default(),
        }
        .into(),
        ("Linkage", "NoSuchMethodError") => LinkageError::NoSuchMethodError {
            class_name: named("class_name"),
            method_name: debug_string_field(body, "method_name").unwrap_or_default(),
            method_descriptor: debug_string_field(body, "method_descriptor").unwrap_or_default(),
        }
        .into(),
        ("Linkage", "IllegalAccessError") => LinkageError::IllegalAccessError {
            message: detail("message"),
        }
        .into(),
        // JVMS §5.3.4: a define a recorded loader constraint refuses
        // (`ClassManager`'s define-time check, `--jdk-only`; interpreter round
        // i1 wave 37, lane L5). `java.lang.LinkageError` itself, with
        // HotSpot's message; without this arm it became a `ClassFormatError`.
        ("Linkage", "LoaderConstraintViolation") => LinkageError::LoaderConstraintViolation {
            message: detail("message"),
        }
        .into(),
        ("Linkage", "AbstractMethodError") => LinkageError::AbstractMethodError {
            class_name: named("class_name"),
            method_name: debug_string_field(body, "method_name").unwrap_or_default(),
        }
        .into(),
        ("Linkage", "UnsupportedClassRedefinitionError") => {
            LinkageError::UnsupportedClassRedefinitionError {
                class_name: named("class_name"),
                message: detail("message"),
            }
            .into()
        }

        // -- VmError::Runtime: only the one JVMS-mandated shape. --------------
        //
        // JVMS §5.3.5 / `ClassLoader.preDefineClass`: a non-bootstrap loader
        // defining into `java.*` is a `SecurityException`, NOT a linkage error,
        // and `class_manager.rs` raises it as `RuntimeError::SecurityException`.
        // `classify_fastpath_invoke_error` routes `VmError::Runtime` to the
        // ordinary runtime-exception path, so this arrives at Java as
        // `java.lang.SecurityException` with the backend's own sentence.
        ("Runtime", "SecurityException") => RuntimeError::SecurityException {
            message: detail("message"),
        }
        .into(),
        // gc-common w6-f: `-XX:MaxMetaspaceSize` refused the define after a
        // collection (`vm_exec.rs::define_class_full`). HotSpot throws
        // `OutOfMemoryError: Metaspace` from `defineClass`, not a linkage error.
        ("Runtime", "OutOfMemoryError") => RuntimeError::OutOfMemoryError {
            message: detail("message"),
        }
        .into(),

        // -- VmError::ClassFile: MUST be re-homed onto a Linkage variant. -----
        //
        // Not cosmetic. `classify_fastpath_invoke_error` (vm/src/runtime/
        // interpreter.rs) converts `VmError::Linkage` and `VmError::Runtime`
        // into Java throwables and sends everything else to
        // `FastPathInvokeError::Fatal` — so returning a `ClassFile` variant from
        // a native is UNCATCHABLE and unwinds past every handler.
        ("ClassFile", "ClassNotFound") => LinkageError::NoClassDefFoundError {
            class_name: named("class_name"),
        }
        .into(),
        ("ClassFile", "UnsupportedVersion") => LinkageError::UnsupportedClassVersionError {
            class_name: named("class_name"),
            // This variant carries `major`/`minor` ints rather than a message,
            // so build HotSpot's shape by hand instead of dumping the fields.
            message: format!(
                "{} has an unsupported class file version",
                named("class_name")
            ),
        }
        .into(),

        // Everything else recognised-but-unmapped keeps `ClassFormatError` —
        // today's answer — but with a readable message rather than the dump.
        // (A circular hierarchy is no longer one of them: the loading guard
        // returns `Linkage(ClassCircularityError)`, re-typed above.)
        _ => LinkageError::ClassFormatError {
            class_name: named("class_name"),
            message: format!(
                "{method}: {}",
                debug_string_field(body, "message").unwrap_or_else(|| inner.to_string())
            ),
        }
        .into(),
    })
}

fn validate_classfile_header(
    class_name: &str,
    method: &str,
    bytes: &[u8],
) -> Result<(), MethodCallFailed> {
    if bytes.len() < 8 || bytes[0..4] != CLASS_FILE_MAGIC {
        return Err(define_class_format_error(
            class_name,
            method,
            "not a valid class file (bad magic)".to_string(),
        ));
    }
    Ok(())
}

/// `--jdk-only`: HotSpot's `ClassFileParser` header errors for the bytes a
/// `defineClass1` / `defineClass2` call hands over, measured on JDK 25
/// (`tools/probes/interp/L5/L5W42DefineClassErrors.java`): fewer than 8 bytes
/// (magic, minor, major) is `ClassFormatError: Truncated class file`; a wrong
/// magic is `ClassFormatError: Incompatible magic value <u32, decimal> in class
/// file <name>`, the name in internal form, `<Unknown>` for a null name. `None`
/// when the header is well-formed (the backend parses the rest). CratonVM said
/// `<name>: defineClass1: not a valid class file (bad magic)` for both before
/// interpreter round i1 wave 42 (lane L5); `--compatible` still does. The
/// VM's other define doors (`classloader::classfile_parser_header_refusal`)
/// use it in both modes since wave 95.
pub(crate) fn jdk_only_classfile_header_error(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    bytes: &[u8],
) -> Option<MethodCallFailed> {
    let message = if bytes.len() < 8 {
        "Truncated class file".to_string()
    } else if bytes[0..4] != CLASS_FILE_MAGIC {
        let magic = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let named = if class_name.is_empty() {
            "<Unknown>"
        } else {
            class_name
        };
        format!("Incompatible magic value {magic} in class file {named}")
    } else {
        return None;
    };
    Some(
        match crate::jboss_module_loader::alloc_single_message_exception(
            ctx,
            "java/lang/ClassFormatError",
            1,
            &message,
        ) {
            Ok(exc) => MethodCallFailed::ExceptionThrown(exc),
            Err(e) => e,
        },
    )
}

/// `--jdk-only`: HotSpot's `ClassFormatError: Truncated class file` for a
/// class file the reader ran off the end of (`ClassReaderError::
/// UnexpectedEndOfData`, which the backend hands back as a `ClassFormatError`
/// whose `Debug` text carries "unexpected end of data"), measured on JDK 25
/// (`L5W42DefineClassErrors`, `magic-version-only`, `cut-in-pool`). CratonVM
/// said `<name>: unexpected end of data at position <n>` before wave 42
/// (lane L5); `--compatible` still does. `None` for any other backend error.
fn jdk_only_truncated_class_file(
    ctx: &mut dyn NativeContext,
    backend_msg: &str,
) -> Option<MethodCallFailed> {
    // Two reader shapes of one fact: a read past the end
    // (`UnexpectedEndOfData`), and `reader::limits::ensure_count_fits`'s
    // pre-size refusal of a declared count the remaining bytes cannot hold
    // ("<table>: declared count N needs at least M bytes but only K remain",
    // `InvalidClassData`; `cut-in-pool` on the host, wave 42 follow-up).
    if !(backend_msg.contains("ClassFormatError")
        && (backend_msg.contains("unexpected end of data")
            || (backend_msg.contains("needs at least") && backend_msg.contains("bytes but only"))))
    {
        return None;
    }
    Some(
        match crate::jboss_module_loader::alloc_single_message_exception(
            ctx,
            "java/lang/ClassFormatError",
            1,
            "Truncated class file",
        ) {
            Ok(exc) => MethodCallFailed::ExceptionThrown(exc),
            Err(e) => e,
        },
    )
}

/// `--jdk-only`: the `ArrayIndexOutOfBoundsException` HotSpot's
/// `ClassLoader.defineClass1` native raises for `(off, len)` over a `byte[]` of
/// `array_len` elements, measured on JDK 25 (`L5W42DefineClassErrors`): a
/// negative `len` first, with a null message (`JNU_ThrowArrayIndexOutOfBounds
/// Exception(env, 0)`), then JNI `GetByteArrayRegion`'s "Array region
/// <off>..<off+len> out of bounds for length <n>". CratonVM said "Array index
/// out of range: <n>" for all of them before wave 42 (lane L5).
fn jdk_only_define_region_error(off: i32, len: i32, array_len: usize) -> Option<MethodCallFailed> {
    if len < 0 {
        return Some(RuntimeError::aioobe_no_message(len).into());
    }
    let array_len = array_len as i64;
    if off < 0 || i64::from(off) > array_len - i64::from(len) {
        return Some(
            RuntimeError::aioobe_with_message(
                off,
                format!(
                    "Array region {off}..{} out of bounds for length {array_len}",
                    i64::from(off) + i64::from(len)
                ),
            )
            .into(),
        );
    }
    None
}

fn read_define_class_nonnegative_int(
    args: &[Value],
    idx: usize,
) -> Result<usize, MethodCallFailed> {
    match args.get(idx) {
        Some(Value::Int(v)) if *v >= 0 => Ok(*v as usize),
        Some(Value::Int(v)) => Err(RuntimeError::aioobe_index_only(*v).into()),
        _ => Ok(0),
    }
}

fn read_byte_array_define_class_slice(
    ctx: &dyn NativeContext,
    array: ObjectRef,
    offset: usize,
    length: usize,
) -> Result<Vec<u8>, MethodCallFailed> {
    let arr_len = ctx.array_length(array);
    let end = offset
        .checked_add(length)
        .ok_or_else(|| RuntimeError::aioobe_index_only(i32::MAX))?;
    if end > arr_len {
        return Err(RuntimeError::aioobe_index_only(end.min(i32::MAX as usize) as i32).into());
    }

    // ONE bulk copy, not a `get_array_element` per byte (SBR5 `clload`: 6.3 million element
    // reads, each with a flag read, for 1 500 classes). The window is bounds-checked above.
    Ok(crate::classloader::read_class_bytes_bulk(
        ctx, array, offset, length,
    ))
}

/// Where a `java.nio.ByteBuffer`'s backing array and cursors actually live.
///
/// The synthetic stub this crate fabricates puts the backing `byte[]` at slot
/// 0 and `position`/`limit`/`capacity` at 1/2/3. A **real** JDK heap buffer
/// does not: `java.nio.HeapByteBuffer` declares no instance fields of its
/// own, so its layout is its ancestors' —
/// `java.nio.Buffer{mark,position,limit,capacity,address,segment}` at 0..5 and
/// `java.nio.ByteBuffer{hb,offset,isReadOnly,bigEndian,nativeByteOrder}` at
/// 6..10 (`javap -p java.nio.Buffer java.nio.ByteBuffer`). Slot 0 on a real
/// buffer is therefore `mark`, an `int` — the array branch below was skipped
/// for every real heap buffer, execution fell into the direct-buffer path,
/// `address` read back 0, and
/// `ClassLoader.defineClass(String, ByteBuffer, ProtectionDomain)` threw
/// "direct ByteBuffer has no native address" for a perfectly valid heap
/// buffer. (`position`/`limit`/`capacity` at 1/2/3 happen to coincide with
/// the real layout; `hb` does not, and neither does `offset`.)
///
/// `hb` is also the discriminator, and it needs no mode flag: a fabricated
/// synthetic stub names its fields `_f0.._fN`, so the by-name resolve MISSES
/// there and the synthetic slots are used unchanged. A real *direct* buffer
/// still resolves `hb` (it is declared on `ByteBuffer`, not `HeapByteBuffer`)
/// and simply reads back null, which correctly routes to the address path.
struct BbDefineLayout {
    /// Backing `byte[]`, or `None` for a direct buffer.
    hb: usize,
    /// `ByteBuffer.offset` — index of the buffer's element 0 inside `hb`.
    /// Non-zero for anything produced by `slice()`. `None` on the synthetic
    /// stub, which has no such field and always starts at 0.
    offset: Option<usize>,
    position: usize,
    limit: usize,
    capacity: usize,
}

fn bb_define_layout(ctx: &dyn NativeContext, bb: ObjectRef) -> BbDefineLayout {
    let cid = ctx.class_id_of_object(bb);
    let named = |n: &str| ctx.resolve_field_index_by_class_id(cid, n);
    match (
        named("hb"),
        named("position"),
        named("limit"),
        named("capacity"),
    ) {
        (Some(hb), Some(position), Some(limit), Some(capacity)) => BbDefineLayout {
            hb,
            offset: named("offset"),
            position,
            limit,
            capacity,
        },
        _ => BbDefineLayout {
            hb: 0,
            offset: None,
            position: 1,
            limit: 2,
            capacity: 3,
        },
    }
}

/// `pub(crate)`: this is the ONLY `defineClass2` ByteBuffer decoder in the
/// crate. `classloader::cl_define_class2` — which SHADOWS this module's
/// `defineClass2` registration in synthetic-JDK mode, because
/// `register_classloader_natives` runs after `register_essential_natives` and
/// `NativeMethodRegistry::register` is last-wins — used to carry a second,
/// unhardened copy that hardcoded slot 0 as the backing array and CLAMPED
/// out-of-range `(off, len)` instead of rejecting them. Both entry points now
/// call this one, so the layout witness above and the `checked_add` bounds
/// below cannot be in effect for one caller and inert for the other.
pub(crate) fn read_byte_buffer_define_class_slice(
    ctx: &dyn NativeContext,
    bb: ObjectRef,
    offset: usize,
    length: usize,
    class_name: &str,
) -> Result<Vec<u8>, MethodCallFailed> {
    let layout = bb_define_layout(ctx, bb);
    let pos_slot = layout.position;
    let limit_slot = layout.limit;
    let capacity_slot = layout.capacity;

    if let Value::Object(Some(array)) = ctx.get_field(bb, layout.hb) {
        // `hb` is shared with every other view of the same array; the buffer's
        // own element 0 sits at `hb_base`. Zero for the synthetic stub and for
        // a whole-array `ByteBuffer.wrap`, non-zero for a `slice()`.
        let hb_base = layout
            .offset
            .map(|s| ctx.get_field(bb, s).as_int().unwrap_or(0).max(0) as usize)
            .unwrap_or(0);
        let arr_len = ctx.array_length(array);
        let cap = arr_len.saturating_sub(hb_base);
        let pos = ctx.get_field(bb, pos_slot).as_int().unwrap_or(0).max(0) as usize;
        let limit = ctx
            .get_field(bb, limit_slot)
            .as_int()
            .unwrap_or(cap as i32)
            .max(0) as usize;
        // Bounds are checked in buffer-relative coordinates, then translated.
        let relative_off = pos
            .checked_add(offset)
            .ok_or_else(|| RuntimeError::aioobe_index_only(i32::MAX))?;
        let upper = limit.min(cap);
        let end = relative_off
            .checked_add(length)
            .ok_or_else(|| RuntimeError::aioobe_index_only(i32::MAX))?;
        if end > upper {
            return Err(RuntimeError::aioobe_index_only(end.min(i32::MAX as usize) as i32).into());
        }
        let absolute_off = hb_base
            .checked_add(relative_off)
            .ok_or_else(|| RuntimeError::aioobe_index_only(i32::MAX))?;
        return read_byte_array_define_class_slice(ctx, array, absolute_off, length);
    }

    let addr = match ctx.get_field_by_name(bb, "address") {
        // zt t20/df8: a real direct buffer whose `address` is 0 (the
        // zero-length `MemorySegment.NULL.asByteBuffer()`): HotSpot's
        // `Java_java_lang_ClassLoader_defineClass2` reads
        // `GetDirectBufferAddress(data)`, gets NULL and throws
        // `JNU_ThrowNullPointerException(env, 0)` (a null message) before it
        // reads a byte. Both modes. Only a buffer without the field (the
        // synthetic stub) keeps the format error below.
        Value::Long(0) => {
            return Err(RuntimeError::NullPointerException { message: None }.into());
        }
        Value::Long(v) => v,
        _ => 0,
    };
    let cap = ctx
        .get_field(bb, capacity_slot)
        .as_int()
        .unwrap_or(0)
        .max(0) as usize;
    let limit = ctx
        .get_field(bb, limit_slot)
        .as_int()
        .unwrap_or(cap as i32)
        .max(0) as usize;
    if addr == 0 {
        return Err(define_class_format_error(
            class_name,
            "defineClass2",
            "direct ByteBuffer has no native address".to_string(),
        ));
    }
    // `offset` IS the buffer's position here: `ClassLoader.defineClass(String,
    // ByteBuffer, ProtectionDomain)` sends a direct buffer as
    // `defineClass2(.., b, b.position(), b.remaining(), ..)`, and HotSpot's
    // native reads `GetDirectBufferAddress(data) + offset`. Adding the
    // position again read `address + 2 * position` (gci i3/jd,
    // `gci-i2jm-defineclass2-direct-buffer-counts-its-position-twice`). The
    // heap arm above keeps its buffer-relative reading: the JDK never sends a
    // heap buffer here (it takes `defineClass1` for those). Dev fixed the
    // same defect independently (interpreter round i5 wave 100, lane L5,
    // probe `L5W100DefineClassBufferPosition`); this arm no longer reads
    // `position` at all.
    let absolute_off = offset;
    let upper = limit.min(cap);
    let end = absolute_off
        .checked_add(length)
        .ok_or_else(|| RuntimeError::aioobe_index_only(i32::MAX))?;
    if end > upper {
        return Err(RuntimeError::aioobe_index_only(end.min(i32::MAX as usize) as i32).into());
    }
    let mut out = vec![0u8; length];
    if length > 0 {
        let src = addr.wrapping_add(absolute_off as i64);
        if !ctx.copy_from_native_memory(src, &mut out) {
            return Err(define_class_format_error(
                class_name,
                "defineClass2",
                format!("direct ByteBuffer copy failed (addr={src:#x}, len={length})"),
            ));
        }
    }
    Ok(out)
}

/// Whether a class the real `ClassLoader.defineClass*` natives define for this
/// loader is filed under the loader's own namespace rather than the global one.
///
/// Strict mode: a plain `URLClassLoader` is a loader like any other. Filing what
/// it defines under the application namespace made a second definition of the
/// same generated name (a CGLIB subclass, first defined in a `URLClassLoader`
/// and then again in the application loader) a duplicate.
/// Pin the `ProtectionDomain` argument of a `defineClass*` native so
/// [`install_protection_domain`] can store it after the define allocates.
/// `None` for a null argument. `--jdk-only` since the fat-jar fix; both
/// modes since interpreter round i1 wave 47, lane L6: `--compatible` handed
/// a user-loader class's load-time transformers a `null` domain and answered
/// a code-source domain of its own from `getProtectionDomain()`
/// (`interpreter-L6-transformers-are-handed-a-null-protection-domain-FIXED-20261011`; probe
/// `tools/probes/interp/L6/L6W47CompatibleAgentModuleOpens.java`).
fn pin_protection_domain_arg(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    index: usize,
) -> Option<(usize, ObjectRef)> {
    match args.get(index) {
        Some(Value::Object(Some(pd))) => Some((ctx.pin_native_root(*pd), *pd)),
        _ => None,
    }
}

/// Store the `ProtectionDomain` a class was defined with in its
/// mirror's `protectionDomain` field, which is what HotSpot does and what JDK
/// 25's `Class.getProtectionDomain()` reads. `ClassLoader.preDefineClass` has
/// already replaced a null domain with the loader's `defaultDomain`, so every
/// loader-defined class gets one. The VM used to derive a code source from
/// the class's file instead, so a class Spring Boot's `LaunchedClassLoader`
/// defined reported `file:/app.jar` where HotSpot reports the
/// `jar:nested:/app.jar/!BOOT-INF/classes/!/` URL the loader passed. See
/// `fatjar-classes-load-from-the-vms-flattened-class-path-not-launchedclassloader-FIXED-20260924.md`.
fn install_protection_domain(
    ctx: &mut dyn NativeContext,
    mirror: ObjectRef,
    pd: Option<(usize, ObjectRef)>,
) {
    if let Some((pin, pd)) = pd {
        let pd = ctx.read_native_pin(pin, pd);
        ctx.set_field_by_name(mirror, "protectionDomain", Value::Object(Some(pd)));
    }
}

/// Does `loader_obj` define into its own namespace: a user-defined loader,
/// or a BARE `URLClassLoader` where the JDK's `URLClassLoader` bodies run
/// (`--jdk-only`, or `--compatible` over a real JDK with the
/// `JdkUrlClassLoaders` row; interpreter round i1 wave 52, lane L5).
fn defines_in_own_namespace(ctx: &mut dyn NativeContext, loader_obj: ObjectRef) -> bool {
    crate::classloader::is_user_defined_loader(ctx, loader_obj)
        || (ctx.jdk_url_class_loaders()
            && crate::classloader::is_bare_url_class_loader(ctx, loader_obj))
}

/// The namespace a define door hands `define_class_full` for `loader` (the
/// door's loader argument) defining `name` (internal form, empty for none),
/// and whether that loader defines in its own namespace
/// ([`defines_in_own_namespace`]), which the door passes on as
/// `DefineClassFull::user_loader_define`. The second answer is the LOADER's,
/// not the namespace's: with `CRATONVM_LOADER_AWARE_RESOLUTION=0` a user
/// loader's define of a fresh name goes to namespace `0`, and the class
/// manager would otherwise take it for an application-loader define
/// (interpreter round i4 wave 96, lane L5;
/// `i95-L5-define-door-loose-ends` items 1 and 3). One copy for
/// `defineClass0/1/2` and both `Unsafe.defineClass` natives, which read a raw
/// field slot as the loader id before wave 96.
pub(crate) fn define_loader_namespace(
    ctx: &mut dyn NativeContext,
    loader: Option<&Value>,
    name: &str,
) -> (u32, bool) {
    match loader {
        Some(Value::Object(Some(loader_obj))) if defines_in_own_namespace(ctx, *loader_obj) => {
            let own = crate::classloader::loader_aware_resolution()
                || name.is_empty()
                || ctx.class_id_by_name(name).is_some();
            let ns = if own {
                crate::classloader::loader_namespace_id(ctx, *loader_obj)
            } else {
                0
            };
            (ns, true)
        }
        // zt t21/df9 (define page §9): the PLATFORM loader object defines in
        // its own built-in namespace (`Extension`), as HotSpot defines in its
        // `ClassLoaderData`. Collapsed to `0` (the application namespace) it
        // shared one name space with the application loader: a define of a
        // name the other one had defined served that class (or failed with
        // `LinkageError`), its class was tagged an application-loader class
        // (`findLoadedClass` / `loadClass` of the platform loader missed it),
        // and the class manager's application-loader package rules refused a
        // `java.*` name the platform loader may define.
        // `CRATONVM_DEFINE_PLATFORM_LOADER_OWN_NAMESPACE=0`: `0` again.
        Some(Value::Object(Some(loader_obj)))
            if crate::classloader::is_platform_class_loader(ctx, *loader_obj)
                && define_platform_loader_own_namespace() =>
        {
            (cratonvm_types::ClassLoaderId::NATIVE_EXTENSION, false)
        }
        _ => (0, false),
    }
}

/// `CRATONVM_DEFINE_PLATFORM_LOADER_OWN_NAMESPACE` (zt t21/df9; default ON,
/// `=0` kill; every collector, both modes): the define doors put a class the
/// platform loader object defines in the `Extension` namespace
/// ([`define_loader_namespace`]), and a built-in loader's `findLoadedClass`
/// answers its own definition of a name before an ancestor's Java-defined one
/// (`classloader::builtin_own_definition_first`).
pub(crate) fn define_platform_loader_own_namespace() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_DEFINE_PLATFORM_LOADER_OWN_NAMESPACE")
}

/// `ClassLoader.defineClass1(ClassLoader, String, byte[], int, int, ProtectionDomain, String)в†’ Class`
///
/// Defines a class from a byte array. WP2.3: routes through
/// `define_class_full` so this entry point shares the same backend
/// (name-mismatch check, dup-define rejection, PD attribution) as
/// the other three (Unsafe.defineClass + Lookup.defineClass).
/// Pre-resolve a class's direct supertypes (superclass + interfaces) through the
/// DEFINING loader before `define_class_full` links them.
///
/// `class_manager::define_class` resolves a class's superclass/interfaces only
/// through CratonVM's global classpath (`load_class`) вЂ” it never calls back into
/// the user `ClassLoader` that is defining the class. That is wrong for a loader
/// whose classes live somewhere the global classpath cannot see: Tomcat's
/// `WebappClassLoader` serves `/WEB-INF/lib` jars from its `WebResourceRoot`, so
/// when it defines `org.apache.taglibs.standard.tlv.JstlCoreTLV` (a JSTL
/// `TagLibraryValidator`) the superclass `JstlBaseTLV` вЂ” in the SAME jar вЂ” is
/// invisible to the global store and the define fails (`ClassNotFound:
/// JstlBaseTLV`), 500-ing every JSP that triggers TLD validation
/// (`TestScopedAttributeELResolver`). JVMS В§5.3.5 makes the defining loader the
/// *initiating* loader for supertype resolution, so load each not-yet-loaded
/// supertype through it first; once present in the store, `define_class_full`
/// links cleanly.
///
/// Only fires for USER-DEFINED loaders and only for supertypes not already
/// loaded, so built-in/app-loader defines (ByteBuddy, cglib, the bootstrap
/// chain) вЂ” whose supertypes resolve from the classpath вЂ” are unaffected.
///
/// `Ok(Some(class file))` -- this pre-pass's parse of `bytes`, for the define
/// to use instead of parsing them again -- only when `keep_parse`
/// (`CRATONVM_DEFINE_ONE_PARSE`; interpreter round i3 wave 72, lane L5).
fn preload_supertypes_via_loader(
    ctx: &mut dyn NativeContext,
    loader_obj: ObjectRef,
    bytes: &[u8],
    keep_parse: bool,
    parsed: Option<PreparsedDefine>,
    answers: &mut SupertypeAnswers,
) -> Result<Option<PreparsedDefine>, cratonvm_types::error::MethodCallFailed> {
    // A bare `java.net.URLClassLoader` is an ordinary isolated loader (Quarkus's
    // `DevModeMediator` builds one over the deployment jars): its own `loadClass`
    // is the only route to a supertype that lives in its URLs, exactly as for a
    // user subclass. Without the pre-pass `define_class_full` looks the supertype
    // up on the global classpath and `NoClassDefFoundError`s for the first class
    // whose super is not yet defined.
    if !crate::classloader::is_user_defined_loader(ctx, loader_obj)
        && !crate::classloader::is_bare_url_class_loader(ctx, loader_obj)
    {
        return Ok(None);
    }
    // `parsed`: the caller's parse of `bytes` (`CRATONVM_DEFINE_PREPASS_OBJECT_ONLY`).
    let parse = match parsed {
        Some(parsed) => Ok(parsed),
        None => parse_for_supertype_link(bytes),
    };
    let (source, cf) = match parse {
        Ok(c) => c,
        Err(_) => return Ok(None), // malformed bytes вЂ” let define_class_full report it
    };
    let mut supertypes: Vec<String> = Vec::new();
    if let Some(s) = &cf.super_class {
        let n: &str = s;
        if !n.is_empty() && n != "java/lang/Object" {
            supertypes.push(n.to_string());
        }
    }
    for iface in &cf.interfaces {
        let n: &str = iface;
        if !n.is_empty() {
            supertypes.push(n.to_string());
        }
    }
    // `CRATONVM_DEFINE_ASKS_FOR_OBJECT`: an `Object` superclass is asked
    // too, after the loop (the superclass is asked last); not in `answers`.
    let ask_object = define_asks_for_object(&*ctx, &cf);
    if supertypes.is_empty() && !ask_object {
        return Ok(keep_parse.then_some((source, cf)));
    }
    answers.begin(&cf, supertypes.len());
    let p_loader = ctx.pin_native_root(loader_obj);
    let mut loader = loader_obj;
    // Loader-faithful gate: under CRATONVM_LOADER_AWARE_RESOLUTION the
    // "already loaded" short-circuit must be keyed on THIS loader's namespace,
    // not the global name index. An isolating/enhancing loader (Hibernate's
    // package-scoped `EnhancingClassLoader`) defines its OWN enhanced copy of an
    // in-package supertype; if a different loader already loaded the un-enhanced
    // copy, the global `class_id_by_name` probe would wrongly skip the drive and
    // the subclass would link the un-enhanced super (NoSuchMethodError on the
    // enhanced `$$_hibernate_*` accessors). Driving `loadClass` here defines the
    // loader's enhanced copy first, which `define_class_full` then prefers via
    // the (loader,name) exact link. Gate-off keeps the global short-circuit в†’
    // byte-identical.
    let loader_faithful = crate::classloader::loader_aware_resolution();
    // `CRATONVM_DEFINE_SUPERTYPE_DICTIONARY` (interpreter round i3 wave 72,
    // lane L5): a name this loader's `loadClass` already answered for a define
    // is not asked again (HotSpot's dictionary hit); the namespace keys it.
    let dictionary = ctx.define_supertype_dictionary();
    let loader_ns = if loader_faithful || dictionary {
        let ns = crate::classloader::loader_namespace_id(ctx, loader);
        loader = ctx.read_native_pin(p_loader, loader);
        ns
    } else {
        0
    };
    // `CRATONVM_DEFINE_RECORDS_SUPERTYPE_INITIATION` (wave 79, L5): the
    // loader is recorded as an initiating loader of each supertype it
    // resolves for this define.
    let record_ns = supertype_initiation_record_ns(ctx, loader);
    loader = ctx.read_native_pin(p_loader, loader);
    // JIT round 23 wave 10 (`CRATONVM_DEFINE_INTERFACES_BEFORE_SUPER`): ask in
    // HotSpot's order (the interfaces, then the superclass); `slot` keeps the
    // pre-pass's own order (superclass first) for `answers`.
    let has_super = cf
        .super_class
        .as_deref()
        .is_some_and(|s| !s.is_empty() && s != "java/lang/Object");
    for slot in supertype_ask_order(supertypes.len(), has_super) {
        let internal = &supertypes[slot];
        loader = ctx.read_native_pin(p_loader, loader);
        let present_id = if loader_faithful {
            ctx.class_id_defined_by_loader_exact(internal, loader_ns)
        } else {
            ctx.class_id_by_name(internal)
        };
        let present = present_id.is_some();
        answers.note_present(slot, present_id);
        let known = !present
            && dictionary
            && crate::classloader::supertype_link_known(ctx, loader_ns, internal);
        if known {
            // The class the loader answered when the row was filed, linked
            // by identity (wave 96, lane L5; `SupertypeLinks`).
            if let Some(id) = crate::classloader::supertype_link_answer(ctx, loader_ns, internal) {
                answers.note_known_answer(&*ctx, slot, id);
            }
        }
        let already = present || known;
        if already {
            // A supertype the flat store already holds is the class
            // `define_class_full` links: HotSpot's `resolve_super_or_fail`
            // would have asked the loader and filed the answer in its
            // dictionary. (Under the loader-faithful gate `present` is the
            // loader's own definition: nothing to record.)
            if let (Some(ns), true, false) = (record_ns, present, loader_faithful) {
                if let Err(e) = note_supertype_initiation(ctx, ns, internal, None) {
                    ctx.unpin_native_roots(p_loader);
                    return Err(e);
                }
            }
            continue; // already present for this loader вЂ” define_class_full links it
        }
        let dotted = internal.replace('/', ".");
        // The G1 near-full proxy-define page: the collecting door. This
        // body calls the loader's Java `loadClass` right below, which may
        // collect, so nothing it holds is unpinned here (the loader is
        // pinned and re-read). The one-attempt door refused at G1's reserve
        // and `defineClass1`, which is not restartable, threw
        // `OutOfMemoryError` (`Zs3Nd3UpcallMints proxy 4096 -Xmx64m`).
        let name_str = ctx.create_string_uninterned_gc_safe(&dotted); // may relocate `loader`
        loader = ctx.read_native_pin(p_loader, loader);
        let p_name = ctx.pin_native_root(name_str);
        loader = ctx.read_native_pin(p_loader, loader);
        let name_str = ctx.read_native_pin(p_name, name_str);
        // Best-effort: a genuine miss/throw is swallowed here and left for
        // `define_class_full` to surface as the spec-mandated linkage error.
        // Under the loader's monitor when it is not parallel-capable
        // (`--jdk-only`), as a VM-initiated load (wave 29, lane L5).
        let result = crate::classloader_real::invoke_load_class_as_the_vm(ctx, loader, name_str);
        // JIT round 20 lane superAcc: the class the loader answered is the
        // supertype the define links (HotSpot's `resolve_super_or_fail`); an
        // answer of another name is HotSpot's `NoClassDefFoundError`.
        if let Ok(Some(Value::Object(Some(mirror)))) = &result {
            if let Err(e) = answers.note_answer(&*ctx, slot, internal, *mirror) {
                ctx.unpin_native_roots(p_name);
                ctx.unpin_native_roots(p_loader);
                return Err(e);
            }
        }
        if let (true, Ok(Some(Value::Object(Some(mirror))))) = (dictionary, &result) {
            let answer = crate::lang_class::mirror_class_id(&*ctx, *mirror);
            crate::classloader::note_supertype_link(ctx, loader_ns, internal, answer);
        }
        if let (Some(ns), Ok(Some(Value::Object(Some(mirror))))) = (record_ns, &result) {
            if let Err(e) = note_supertype_initiation(ctx, ns, internal, Some(*mirror)) {
                ctx.unpin_native_roots(p_name);
                ctx.unpin_native_roots(p_loader);
                return Err(e);
            }
        }
        // Except a `ClassCircularityError`: the class being defined is
        // among its own supertypes, and nothing `define_class_full` can link
        // afterwards names that (it would report the half-defined class as
        // missing). HotSpot propagates it unchanged.
        if let Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(e)) = result {
            if is_class_circularity_error(ctx, e) {
                ctx.unpin_native_roots(p_loader);
                return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(e));
            }
            // JIT round 21 wave 26, lane nesting: a `ClassNotFoundException`
            // becomes the cause of the define's `NoClassDefFoundError`
            // (`supertype_miss_with_cause`). Read before anything allocates.
            answers.note_miss(ctx, internal, e);
        }
        loader = ctx.read_native_pin(p_loader, loader); // invoke may relocate
        ctx.unpin_native_roots(p_name);
    }
    if ask_object {
        // Best-effort, as above: a refusal is left to `define_class_full`.
        if let Err(e) = ask_defining_loader_for_object(ctx, p_loader, loader, loader_ns, record_ns)
        {
            ctx.unpin_native_roots(p_loader);
            return Err(e);
        }
    }
    ctx.unpin_native_roots(p_loader);
    Ok(keep_parse.then_some((source, cf)))
}

/// `CRATONVM_DEFINE_RECORDS_SUPERTYPE_INITIATION` (interpreter round i3 wave
/// 79, lane L5; default OFF): the namespace id of `loader` when the define
/// pre-pass should record it as an initiating loader of the supertypes it
/// resolves -- the switch on, the initiating records on (`--jdk-only`, or
/// `CRATONVM_COMPAT_INITIATING_RECORDS`), and a user-defined namespace (>= 3).
/// HotSpot: `SystemDictionary::resolve_super_or_fail` -> `load_instance_class`
/// files the class the loader answered in that loader's dictionary
/// (`update_dictionary`) when the loader is not its defining loader, so
/// `L.findLoadedClass("java.io.Serializable")` answers the interface once `L`
/// defined a class implementing it
/// (`docs/feature-designs/native-dispatch-architecture.md`).
/// May allocate a namespace id: the caller re-reads its pinned loader.
fn supertype_initiation_record_ns(ctx: &mut dyn NativeContext, loader: ObjectRef) -> Option<u32> {
    if !ctx.define_records_supertype_initiation() || !ctx.initiating_records_on() {
        return None;
    }
    crate::classloader::peek_loader_namespace_id(ctx, loader)
        .or_else(|| Some(crate::classloader::loader_namespace_id(ctx, loader)))
        .filter(|&ns| ns >= 3)
}

/// Record the user loader `loader_ns` as an initiating loader of the
/// supertype `internal` it resolved for a define: the class `mirror` names
/// (its `loadClass` answer), else the flat store's class of that name (the
/// one the define links). `note_initiating_load` checks the loader
/// constraints first (HotSpot's `check_constraints` in `load_instance_class`;
/// a violation is the `LinkageError` it throws) and records nothing for the
/// loader's own class. Allocates only when it refuses.
#[cold]
#[inline(never)]
fn note_supertype_initiation(
    ctx: &mut dyn NativeContext,
    loader_ns: u32,
    internal: &str,
    mirror: Option<ObjectRef>,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    let class_id = match mirror {
        Some(mirror) => ctx.class_id_from_mirror(mirror),
        None => ctx.class_id_by_name(internal),
    };
    match class_id {
        Some(class_id) => ctx.note_initiating_load(loader_ns, internal, class_id),
        None => Ok(()),
    }
}

/// Whether `throwable` is a `java.lang.ClassCircularityError`.
fn is_class_circularity_error(ctx: &dyn NativeContext, throwable: ObjectRef) -> bool {
    crate::classloader::object_extends(ctx, throwable, "java/lang/ClassCircularityError")
}

thread_local! {
    /// The `(loader pin, loader, internal name)` of every class whose direct
    /// supertypes this thread is linking through its defining loader right
    /// now, innermost last.
    ///
    /// JVMS §5.3.5 steps 3-4: a class that is its own supertype is a
    /// `ClassCircularityError`. A user loader meets the cycle as a re-entered
    /// define: defining `p.A` loads `p.B` through the loader, whose define
    /// loads `p.A` again, and `p.A` is not yet defined, so the loader's
    /// `findClass` defines it a second time. HotSpot's placeholder table
    /// (`SystemDictionary::resolve_super_or_fail`) sees `(loader, p.A)` already
    /// resolving its super on this thread and throws; this is that table. The
    /// loader is compared through the pin its `defineClass` native holds for
    /// the whole link, so a collection in between does not hide it.
    ///
    /// Per thread and per call, not per VM: each entry lives exactly as long
    /// as one `link_supertypes_through_defining_loader` call on this thread.
    static SUPERTYPE_LINKS_IN_PROGRESS: std::cell::RefCell<Vec<(usize, ObjectRef, String)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// `CRATONVM_DEFINE_INTERFACES_BEFORE_SUPER` (JIT round 23 wave 10, lane L1;
/// HotSpot alignment, default ON, `=0` asks the superclass first as before):
/// the defining loader's `loadClass` is asked for a defined class's direct
/// supertypes in HotSpot's order. `ClassFileParser::parse_interfaces` resolves
/// each interface (`resolve_super_or_fail`) while it parses, and the
/// superclass is resolved after (`post_process_parsed_stream`). Measured on
/// Temurin 25.0.3 with a logging user loader: `Two extends AbstractList
/// implements Runnable, Closeable` asks `Runnable`, `Closeable`,
/// `AbstractList`; a `Proxy`/`Runnable` proxy asks `Runnable`, `Proxy`. Before,
/// both modes asked the superclass first. Read per define.
pub(crate) fn define_interfaces_before_super_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_DEFINE_INTERFACES_BEFORE_SUPER")
}

/// The order in which the define natives ask the loader for the pre-pass's
/// `count` supertypes, as indices into the pre-pass's own list (the
/// superclass at 0 when `has_super`, then the interfaces in class-file order):
/// the interfaces first, then the superclass, under
/// [`define_interfaces_before_super_enabled`]; the list order otherwise.
fn supertype_ask_order(count: usize, has_super: bool) -> Vec<usize> {
    if has_super && count > 1 && define_interfaces_before_super_enabled() {
        (1..count).chain(std::iter::once(0)).collect()
    } else {
        (0..count).collect()
    }
}

/// `CRATONVM_DEFINE_ASKS_FOR_OBJECT` (verifier-link-loads page, item 3;
/// HotSpot alignment, default ON, `=0` never asks as before): a user loader's
/// define of a class whose superclass is `java/lang/Object` asks that loader's
/// `loadClass("java.lang.Object")`, where HotSpot's `resolve_super_or_fail`
/// does -- last, as the superclass is asked last, and once per loader: the
/// supertype dictionary (`CRATONVM_DEFINE_SUPERTYPE_DICTIONARY`, which this
/// requires) files the answer, as HotSpot's dictionary does, so later defines
/// do not ask again. Measured with `tools/probes/jit/r23/VLoad.java` on
/// HotSpot 25.0.4.1: `DEFINE loadClass java.lang.Object` on the first define
/// of a fresh loader, none for the classes it defines after. Read per define.
pub(crate) fn define_asks_for_object_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_DEFINE_ASKS_FOR_OBJECT")
}

/// Whether a user loader's define of `class_file` asks the loader for
/// `java/lang/Object` ([`define_asks_for_object_enabled`]): its superclass is
/// `Object` and the supertype dictionary is on to file the answer.
fn define_asks_for_object(
    ctx: &dyn NativeContext,
    class_file: &cratonvm_reader::ClassFile,
) -> bool {
    class_file.super_class.as_deref() == Some("java/lang/Object")
        && ctx.define_supertype_dictionary()
        && define_asks_for_object_enabled()
}

/// Ask the user loader `loader_ns` (pinned at `loader_pin`) for
/// `java/lang/Object` unless its dictionary already holds it
/// ([`define_asks_for_object`]). The answer is filed in the dictionary and,
/// with `record_ns`, as an initiating load (HotSpot's `update_dictionary`:
/// `L.findLoadedClass("java.lang.Object")` answers afterwards).
///
/// `Ok(None)`: answered (or nothing to ask). `Ok(Some(cause))`: the loader
/// did not answer `Object` -- `cause` is what it threw, if anything -- which
/// HotSpot reports as `NoClassDefFoundError: java/lang/Object`; the caller
/// decides (the best-effort route ignores it, the isolated route fails).
fn ask_defining_loader_for_object(
    ctx: &mut dyn NativeContext,
    loader_pin: usize,
    loader_obj: ObjectRef,
    loader_ns: u32,
    record_ns: Option<u32>,
) -> Result<Option<Option<ObjectRef>>, cratonvm_types::error::MethodCallFailed> {
    const OBJECT: &str = "java/lang/Object";
    if crate::classloader::supertype_link_known(ctx, loader_ns, OBJECT) {
        return Ok(None);
    }
    let name_str = ctx.create_string("java.lang.Object");
    let p_name = ctx.pin_native_root(name_str);
    let loader = ctx.read_native_pin(loader_pin, loader_obj);
    let name_str = ctx.read_native_pin(p_name, name_str);
    let result = crate::classloader_real::invoke_load_class_as_the_vm(ctx, loader, name_str);
    ctx.unpin_native_roots(p_name);
    match result {
        Ok(Some(Value::Object(Some(mirror)))) => {
            let answer = crate::lang_class::mirror_class_id(&*ctx, mirror);
            if answer.and_then(|id| ctx.class_name_of_id(id)).as_deref() != Some(OBJECT) {
                return Ok(Some(None));
            }
            crate::classloader::note_supertype_link(ctx, loader_ns, OBJECT, answer);
            if let Some(ns) = record_ns {
                note_supertype_initiation(ctx, ns, OBJECT, Some(mirror))?;
            }
            Ok(None)
        }
        Ok(_) => Ok(Some(None)),
        Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(e)) => Ok(Some(Some(e))),
        Err(other) => Err(other),
    }
}

#[cfg(test)]
mod r23w10_supertype_ask_order_tests {
    use super::supertype_ask_order;

    #[test]
    fn interfaces_are_asked_before_the_superclass() {
        let key = concat!("CRATON", "VM_DEFINE_INTERFACES_BEFORE_SUPER");
        cratonvm_types::flags::with_thread_overrides(&[(key, None)], || {
            assert_eq!(supertype_ask_order(3, true), vec![1, 2, 0]);
            assert_eq!(supertype_ask_order(2, true), vec![1, 0]);
            assert_eq!(supertype_ask_order(1, true), vec![0], "only the superclass");
            assert_eq!(supertype_ask_order(2, false), vec![0, 1], "interfaces only");
            assert!(supertype_ask_order(0, false).is_empty());
        });
        cratonvm_types::flags::with_thread_overrides(&[(key, Some("0"))], || {
            assert_eq!(supertype_ask_order(3, true), vec![0, 1, 2], "=0: the old order");
        });
    }
}

/// The classes a defining loader's `loadClass` answered for the direct
/// supertypes of the class it defines (JIT round 20 lane superAcc, page
/// `docs/feature-designs/native-dispatch-architecture.md`).
///
/// HotSpot links exactly those classes (`SystemDictionary::resolve_super_or_fail`
/// -> `L.loadClass(name)`). The define backend links supertypes by NAME: the
/// loader's own definition, then a class one of its recorded user ancestors
/// defined, then a loader-blind lookup -- which, for a loader that delegates
/// outside its parent chain (a sibling, a JBoss module, an OSGi bundle) while
/// another copy of the name exists, links a class the loader never named. The
/// pre-pass records each answer by identity and the define is handed them as
/// `superclass_id_override` / `interface_id_overrides`.
///
/// Handed over only when some answer was defined by a USER loader: a built-in
/// loader's answer is unique by name and is what the by-name link finds, so
/// the common define (JDK supertypes) keeps its path unchanged. Interfaces go
/// all or nothing (the overrides are positional): a supertype the pre-pass
/// skipped without an identity (`supertype_link_known`) leaves them to the
/// by-name link.
#[derive(Default)]
pub(crate) struct SupertypeAnswers {
    /// Per pre-pass supertype, in its order (the superclass first when it is
    /// not `java/lang/Object`, then the interfaces).
    ids: Vec<Option<cratonvm_types::ClassId>>,
    /// Is `ids[0]` the superclass?
    has_super: bool,
    /// The class file's interface count (the overrides' required length).
    interface_count: usize,
    /// Did a user-defined loader define one of the answers?
    user_answer: bool,
    /// The first supertype whose `loadClass` threw a
    /// `ClassNotFoundException`: `(internal name, cause class, cause
    /// message)` (JIT round 21 wave 26, lane nesting;
    /// [`supertype_miss_with_cause`]).
    miss_cause: Option<(String, String, Option<String>)>,
    /// Keep the loader's OWN `ClassNotFoundException` object for the cause
    /// (JIT round 22 wave 1, lane review;
    /// [`supertype_miss_cause_original_enabled`]). Set only by the two
    /// `defineClass` natives, which release [`Self::miss_root`] on every exit
    /// ([`Self::release_miss_root`]); other users never root anything.
    keep_miss_object: bool,
    /// The global root (`NativeContext::add_global_root`) holding that
    /// object, or `0`.
    miss_root: usize,
}

impl SupertypeAnswers {
    fn begin(&mut self, class_file: &cratonvm_reader::ClassFile, supertypes: usize) {
        self.ids = vec![None; supertypes];
        self.has_super = class_file
            .super_class
            .as_deref()
            .is_some_and(|s| !s.is_empty() && s != "java/lang/Object");
        self.interface_count = class_file.interfaces.len();
        self.user_answer = false;
        self.miss_cause = None;
    }

    /// The loader's `loadClass` threw `thrown` for the supertype `internal`.
    /// Only a `ClassNotFoundException` is kept, and only the first
    /// (`supertype_miss_with_cause`).
    fn note_miss(&mut self, ctx: &mut dyn NativeContext, internal: &str, thrown: ObjectRef) {
        if self.miss_cause.is_some() || !supertype_miss_cause_enabled() {
            return;
        }
        if !crate::classloader::object_extends(&*ctx, thrown, "java/lang/ClassNotFoundException") {
            return;
        }
        let Some(class) = ctx.class_name_of_id(ctx.class_id_of_object(thrown)) else {
            return;
        };
        let message = match ctx.get_field_by_name(thrown, "detailMessage") {
            Value::Object(Some(text)) => ctx.read_string(text),
            _ => None,
        };
        self.miss_cause = Some((internal.to_string(), class, message));
        // JIT round 22 wave 1, lane review: keep the thrown object itself
        // (HotSpot's `THROW_MSG_CAUSE` hands the loader's own exception, with
        // its `findClass` stack trace, to the `NoClassDefFoundError`). A global
        // root, because the native pin stack is truncated by the pre-pass's
        // own unpins before the define fails.
        if self.keep_miss_object && supertype_miss_cause_original_enabled() {
            self.release_miss_root(ctx);
            self.miss_root = ctx.add_global_root(thrown);
        }
    }

    /// Release [`Self::miss_root`] (idempotent).
    pub(crate) fn release_miss_root(&mut self, ctx: &mut dyn NativeContext) {
        if self.miss_root != 0 {
            let _ = ctx.remove_global_root(self.miss_root);
            self.miss_root = 0;
        }
    }

    /// The loader's own `ClassNotFoundException`, when kept
    /// ([`Self::keep_miss_object`]) and still rooted.
    fn miss_object(&self, ctx: &dyn NativeContext) -> Option<ObjectRef> {
        if self.miss_root == 0 {
            return None;
        }
        ctx.resolve_global_root(self.miss_root)
    }
}

/// `CRATONVM_DEFINE_SUPERTYPE_MISS_CAUSE_ORIGINAL` -- correctness, both
/// modes, default ON, `=0` rebuilds the cause from its class and message as
/// round 21 wave 26 did (JIT round 22 wave 1, lane review; page
/// `docs/feature-designs/native-dispatch-architecture.md`):
/// the define's `NoClassDefFoundError` carries the defining loader's own
/// `ClassNotFoundException` object (identity and stack trace), as HotSpot's.
/// Read once per missed supertype.
fn supertype_miss_cause_original_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_DEFINE_SUPERTYPE_MISS_CAUSE_ORIGINAL")
}

/// `CRATONVM_DEFINE_SUPERTYPE_MISS_CAUSE` -- correctness, both modes, default
/// ON, `=0` throws the cause-less error as before (JIT round 21 wave 26, lane
/// nesting): [`supertype_miss_with_cause`].
fn supertype_miss_cause_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_DEFINE_SUPERTYPE_MISS_CAUSE")
}

/// The define's `NoClassDefFoundError` for a supertype the defining loader
/// could not load, with that loader's `ClassNotFoundException` as its cause
/// (JIT round 21 wave 26, lane nesting;
/// `CRATONVM_DEFINE_SUPERTYPE_MISS_CAUSE`).
///
/// HotSpot: `resolve_super_or_fail` -> `resolve_or_fail` ->
/// `handle_resolution_exception` turns a pending `ClassNotFoundException`
/// into `NoClassDefFoundError: <internal name>` caused by it
/// (`THROW_MSG_CAUSE`). The pre-pass here swallows the loader's throw and
/// leaves the error to `define_class_full`, which names the supertype but has
/// no cause. Measured on HotSpot 25 (`R21Fz9ReviewEmUser1`'s $B, whose
/// EnclosingMethod class's superclass is missing): `getCause()` is the
/// `ClassNotFoundException` with the loader's message; CratonVM answered no
/// cause. Since JIT round 22 wave 1 (lane review,
/// `CRATONVM_DEFINE_SUPERTYPE_MISS_CAUSE_ORIGINAL`) the cause is the loader's
/// own object, kept under a global root by the define natives; with that
/// switch off, or with no root, it is a new throwable of the recorded class
/// and message (as the resolution-error table rebuilds it). Every other
/// failure is handed back unchanged.
fn supertype_miss_with_cause(
    ctx: &mut dyn NativeContext,
    failed: MethodCallFailed,
    answers: &SupertypeAnswers,
) -> MethodCallFailed {
    let Some((internal, cause_class, cause_message)) = &answers.miss_cause else {
        return failed;
    };
    let class_name = match &failed {
        MethodCallFailed::InternalError(cratonvm_types::error::VmError::Linkage(
            LinkageError::NoClassDefFoundError { class_name },
        )) if class_name.replace('.', "/") == *internal => class_name.clone(),
        _ => return failed,
    };
    // JIT round 22 wave 1, lane review: the loader's own object when it was
    // kept (`CRATONVM_DEFINE_SUPERTYPE_MISS_CAUSE_ORIGINAL`), else rebuilt.
    let cause = match answers.miss_object(&*ctx) {
        Some(original) => original,
        None => match cause_message {
            Some(message) => match crate::phases_early::throw_jca_exc(ctx, cause_class, message) {
                MethodCallFailed::ExceptionThrown(cause) => cause,
                _ => return failed,
            },
            None => match ctx.new_object_initialized(cause_class, "()V", &[]) {
                Ok(Some(Value::Object(Some(cause)))) => cause,
                _ => return failed,
            },
        },
    };
    let cause_pin = ctx.pin_native_root(cause);
    let error =
        crate::phases_early::throw_jca_exc(ctx, "java/lang/NoClassDefFoundError", &class_name);
    let cause = ctx.read_native_pin(cause_pin, cause);
    ctx.unpin_native_roots(cause_pin);
    match error {
        MethodCallFailed::ExceptionThrown(made)
            if ctx.class_name_of_id(ctx.class_id_of_object(made)).as_deref()
                == Some("java/lang/NoClassDefFoundError") =>
        {
            ctx.set_field_by_name(made, "cause", Value::Object(Some(cause)));
            MethodCallFailed::ExceptionThrown(made)
        }
        _ => failed,
    }
}

impl SupertypeAnswers {
    /// A supertype the pre-pass did not ask the loader for: the class it
    /// already holds for it, when known.
    fn note_present(&mut self, slot: usize, id: Option<cratonvm_types::ClassId>) {
        if let Some(cell) = self.ids.get_mut(slot) {
            *cell = id;
        }
    }

    /// A supertype the pre-pass did not ask the loader for because its
    /// dictionary row (`classloader::supertype_link_answer`) holds the class
    /// the loader answered before: linked by identity, as the answer of an
    /// ask is (interpreter round i4 wave 96, lane L5; `L5W86ModuleLayers`).
    fn note_known_answer(
        &mut self,
        ctx: &dyn NativeContext,
        slot: usize,
        id: cratonvm_types::ClassId,
    ) {
        if let Some(cell) = self.ids.get_mut(slot) {
            *cell = Some(id);
        }
        if ctx.loader_id_of_class(id) >= 3 {
            self.user_answer = true;
        }
    }

    /// The loader answered `mirror` for `internal`. HotSpot's
    /// `load_instance_class_impl` takes the answer only when its name is the
    /// requested one, else the supertype resolution fails with
    /// `NoClassDefFoundError: <internal name>`; that error is returned here.
    fn note_answer(
        &mut self,
        ctx: &dyn NativeContext,
        slot: usize,
        internal: &str,
        mirror: ObjectRef,
    ) -> Result<(), cratonvm_types::error::MethodCallFailed> {
        let Some(id) = crate::lang_class::mirror_class_id(ctx, mirror) else {
            return Ok(());
        };
        let Some(answered) = ctx.class_name_arc_of_id(id) else {
            return Ok(());
        };
        let same_name = answered.len() == internal.len()
            && answered
                .bytes()
                .zip(internal.bytes())
                .all(|(a, b)| a == b || (a == b'.' && b == b'/'));
        if !same_name {
            return Err(LinkageError::NoClassDefFoundError {
                class_name: internal.to_string(),
            }
            .into());
        }
        if let Some(cell) = self.ids.get_mut(slot) {
            *cell = Some(id);
        }
        if ctx.loader_id_of_class(id) >= 3 {
            self.user_answer = true;
        }
        Ok(())
    }

    /// The define's `(superclass_id_override, interface_id_overrides)`.
    pub(crate) fn overrides(
        &self,
    ) -> (
        Option<cratonvm_types::ClassId>,
        Option<Vec<cratonvm_types::ClassId>>,
    ) {
        if !self.user_answer {
            return (None, None);
        }
        let (superclass, interfaces) = match (self.has_super, self.ids.split_first()) {
            (true, Some((first, rest))) => (*first, rest),
            _ => (None, &self.ids[..]),
        };
        let interfaces = if !interfaces.is_empty() && interfaces.len() == self.interface_count {
            interfaces
                .iter()
                .copied()
                .collect::<Option<Vec<cratonvm_types::ClassId>>>()
        } else {
            None
        };
        (superclass, interfaces)
    }
}

/// What the define natives' supertype pre-pass hands the define under
/// `CRATONVM_DEFINE_ONE_PARSE` (interpreter round i3 wave 72, lane L5): the
/// shared copy of the class file's bytes it parsed, and that parse. The
/// define caches the same copy as the class's file, so the lazy attributes
/// and the cached bytes keep sharing one allocation as when it parses itself.
/// The vm's `define_class_full` downcasts to exactly this type.
pub(crate) type PreparsedDefine = (std::sync::Arc<[u8]>, cratonvm_reader::ClassFile);

/// The pre-pass's parse of `bytes`: [`cratonvm_reader::read_class`] spelled
/// out (`read_class` is `read_class_arc(Arc::from(bytes))`), keeping the copy
/// for [`PreparsedDefine`]. Same answer and same cost as `read_class`.
fn parse_for_supertype_link(
    bytes: &[u8],
) -> Result<PreparsedDefine, cratonvm_reader::ClassReaderError> {
    // `CRATONVM_DBG_DEFINE_CENSUS`: the `prepass-parse` row (wave 93, L5).
    let parse_clock = cratonvm_classloading::define_census::phase_start();
    let source: std::sync::Arc<[u8]> = std::sync::Arc::from(bytes);
    let class_file = cratonvm_reader::read_class_arc(std::sync::Arc::clone(&source));
    cratonvm_classloading::define_census::phase_end(
        cratonvm_classloading::define_census::DefinePhase::PrepassParse,
        parse_clock,
    );
    Ok((source, class_file?))
}

/// The supertype pre-pass's parse as [`cratonvm_native_api::DefineClassFull::preparsed`]
/// (`CRATONVM_DEFINE_ONE_PARSE`): the define takes it instead of parsing the
/// same bytes again. `None` parses.
fn preparsed_for_define(
    parse: Option<PreparsedDefine>,
) -> Option<std::sync::Arc<dyn std::any::Any + Send + Sync>> {
    parse.map(|parsed| {
        std::sync::Arc::new(parsed) as std::sync::Arc<dyn std::any::Any + Send + Sync>
    })
}

/// [`link_supertypes_through_defining_loader`] for the class `name` (internal
/// form; empty when the caller passed no name), refusing a define of `name`
/// that this thread is already linking the supertypes of for the same loader:
/// a circular hierarchy (see [`SUPERTYPE_LINKS_IN_PROGRESS`]). `Ok(Some(..))`
/// is the pre-pass's parse of `bytes`, kept only when `keep_parse`
/// (`CRATONVM_DEFINE_ONE_PARSE`; interpreter round i3 wave 72, lane L5).
fn link_supertypes_refusing_circularity(
    ctx: &mut dyn NativeContext,
    loader_pin: usize,
    loader_obj: ObjectRef,
    name: &str,
    bytes: &[u8],
    keep_parse: bool,
    answers: &mut SupertypeAnswers,
) -> Result<Option<PreparsedDefine>, cratonvm_types::error::MethodCallFailed> {
    if name.is_empty() {
        return link_supertypes_through_defining_loader(
            ctx, loader_obj, name, bytes, keep_parse, answers,
        );
    }
    let circular = SUPERTYPE_LINKS_IN_PROGRESS.with(|links| {
        links.borrow().iter().any(|(pin, obj, linking)| {
            linking == name && ctx.read_native_pin(*pin, *obj).as_ptr() == loader_obj.as_ptr()
        })
    });
    if circular {
        return Err(LinkageError::ClassCircularityError {
            class_name: name.to_string(),
        }
        .into());
    }
    SUPERTYPE_LINKS_IN_PROGRESS.with(|links| {
        links
            .borrow_mut()
            .push((loader_pin, loader_obj, name.to_string()))
    });
    let result =
        link_supertypes_through_defining_loader(ctx, loader_obj, name, bytes, keep_parse, answers);
    SUPERTYPE_LINKS_IN_PROGRESS.with(|links| {
        links.borrow_mut().pop();
    });
    result
}

/// Resolve direct hierarchy edges through an isolated URL loader before its
/// class is handed to the backend linker. Unlike the generic best-effort
/// helper above, a miss is authoritative: a platform-parented loader cannot
/// fall back to the process application classpath.
pub(crate) fn preload_isolated_loader_supertypes(
    ctx: &mut dyn NativeContext,
    loader_obj: ObjectRef,
    bytes: &[u8],
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    let class_file = match cratonvm_reader::read_class(bytes) {
        Ok(class_file) => class_file,
        Err(_) => return Ok(()),
    };
    let mut names: Vec<String> = Vec::new();
    if let Some(super_name) = &class_file.super_class {
        if !super_name.is_empty() && &**super_name != "java/lang/Object" {
            names.push(super_name.to_string());
        }
    }
    names.extend(
        class_file
            .interfaces
            .iter()
            .filter(|name| !name.is_empty())
            .map(|name| name.to_string()),
    );
    let loader_pin = ctx.pin_native_root(loader_obj);
    let mut loader = loader_obj;
    for internal_name in names {
        loader = ctx.read_native_pin(loader_pin, loader);
        let dotted_name = ctx.create_string(&internal_name.replace('/', "."));
        let name_pin = ctx.pin_native_root(dotted_name);
        loader = ctx.read_native_pin(loader_pin, loader);
        let dotted_name = ctx.read_native_pin(name_pin, dotted_name);
        // Under the loader's monitor when it is not parallel-capable
        // (`--jdk-only`), as a VM-initiated load (wave 29, lane L5).
        let result = crate::classloader_real::invoke_load_class_as_the_vm(ctx, loader, dotted_name);
        ctx.unpin_native_roots(name_pin);
        match result {
            Ok(Some(Value::Object(Some(_)))) => {}
            Err(error) => {
                ctx.unpin_native_roots(loader_pin);
                return Err(error);
            }
            _ => {
                let exception = crate::jboss_module_loader::alloc_single_message_exception(
                    ctx,
                    "java/lang/ClassNotFoundException",
                    1,
                    &internal_name,
                );
                ctx.unpin_native_roots(loader_pin);
                return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                    exception?,
                ));
            }
        }
    }
    ctx.unpin_native_roots(loader_pin);
    Ok(())
}

/// Link a class's direct supertypes through the loader defining it, before the
/// backend links them from the flat class store.
///
/// Under `--jdk-only` an ISOLATED `URLClassLoader` (null or platform parent)
/// has the final word on them, as HotSpot's linker gives it: a supertype the
/// loader refuses fails the definition with `NoClassDefFoundError: <name>`,
/// caused by the loader's own exception. Any other loader, and every loader
/// under `--compatible`, keeps the best-effort preload
/// ([`preload_supertypes_via_loader`]), whose misses fall back to the flat
/// store.
///
/// The best-effort preload was all the `defineClass` natives had. It was
/// harmless while the VM defined isolated loaders' classes itself
/// (`ucl_try_define_local_class`, which calls
/// [`preload_isolated_loader_supertypes`]). Once the `URLClassLoader`
/// retirement handed that job to the loader's real `findClass`, Spring Boot's
/// `ModifiedClassPathClassLoader` with `jackson-core` excluded DEFINED
/// `ObjectMapper` over the application's `ObjectCodec`, where HotSpot throws.
/// See
/// `fatjar-classes-load-from-the-vms-flattened-class-path-not-launchedclassloader-FIXED-20260924.md`.
fn link_supertypes_through_defining_loader(
    ctx: &mut dyn NativeContext,
    loader_obj: ObjectRef,
    name: &str,
    bytes: &[u8],
    keep_parse: bool,
    answers: &mut SupertypeAnswers,
) -> Result<Option<PreparsedDefine>, cratonvm_types::error::MethodCallFailed> {
    // `CRATONVM_DEFINE_PREPASS_OBJECT_ONLY` (interpreter round i4 wave 93,
    // lane L5; default off): for a user-defined loader, parse first. A class
    // that names no supertype but `java/lang/Object` has nothing for either
    // route below to ask the loader, and both answer the same for it -- the
    // parse (kept with `keep_parse`) and no cross-loader answers -- so the
    // isolation probe (`url_classloader_isolated_from_app`: superclass-name
    // walks, the loader's URL stash and `parent` by name) is skipped. Any
    // other class hands this parse to the route it takes, so it is never
    // parsed twice. A loader that is not user-defined keeps the old order
    // (the preload route returns before parsing for it).
    let mut parsed: Option<PreparsedDefine> = None;
    if cratonvm_types::flags::runtime_flag_on("CRATONVM_DEFINE_PREPASS_OBJECT_ONLY")
        && crate::classloader::is_user_defined_loader(ctx, loader_obj)
    {
        let (source, class_file) = match parse_for_supertype_link(bytes) {
            Ok(parsed) => parsed,
            Err(_) => return Ok(None), // malformed bytes: `define_class_full` reports them
        };
        let object_only = class_file
            .super_class
            .as_ref()
            .is_none_or(|s| s.is_empty() || &**s == "java/lang/Object")
            && class_file.interfaces.iter().all(|i| i.is_empty());
        if object_only && !define_asks_for_object(&*ctx, &class_file) {
            // No supertype to ask about: `answers` stays empty, as the
            // preload route leaves it for such a class.
            return Ok(keep_parse.then_some((source, class_file)));
        }
        parsed = Some((source, class_file));
    }
    let isolation_clock = cratonvm_classloading::define_census::phase_start();
    let isolated = ctx.jdk_url_class_loaders()
        && crate::classloader::url_classloader_isolated_from_app(ctx, loader_obj);
    cratonvm_classloading::define_census::phase_end(
        cratonvm_classloading::define_census::DefinePhase::PrepassIsolation,
        isolation_clock,
    );
    if !isolated {
        return preload_supertypes_via_loader(ctx, loader_obj, bytes, keep_parse, parsed, answers);
    }
    let parse = match parsed {
        Some(parsed) => Ok(parsed),
        None => parse_for_supertype_link(bytes),
    };
    let (source, class_file) = match parse {
        Ok(parsed) => parsed,
        Err(_) => return Ok(None), // malformed bytes: `define_class_full` reports them
    };
    // HotSpot refuses these flags (and a wrong name) while it parses, before
    // it resolves a supertype: no `loadClass` for them; `define_class_full`
    // raises the refusal (interpreter round i4 wave 84, lane L5). Judged in
    // the define's own scope, `-Xverify:none` included (t1 wave 13, lane
    // I24W): a class that define accepts gets its supertype `loadClass`es.
    let relaxation = ctx.define_format_relaxation();
    if cratonvm_classloading::define_refused_while_parsing(&class_file, name, relaxation) {
        return Ok(keep_parse.then_some((source, class_file)));
    }
    let mut names: Vec<String> = Vec::new();
    if let Some(super_name) = &class_file.super_class {
        if !super_name.is_empty() && &**super_name != "java/lang/Object" {
            names.push(super_name.to_string());
        }
    }
    names.extend(
        class_file
            .interfaces
            .iter()
            .filter(|name| !name.is_empty())
            .map(|name| name.to_string()),
    );
    let loader_pin = ctx.pin_native_root(loader_obj);
    // `CRATONVM_DEFINE_SUPERTYPE_DICTIONARY` (interpreter round i3 wave 72,
    // lane L5): HotSpot's `resolve_super_or_fail` asks the loader's
    // `loadClass` only for a name its dictionary does not hold yet -- one the
    // loader neither defined nor answered before -- so a supertype shared by
    // many of the loader's classes is asked for once, not once per define.
    let dictionary_ns = if ctx.define_supertype_dictionary() {
        let loader = ctx.read_native_pin(loader_pin, loader_obj);
        Some(crate::classloader::loader_namespace_id(ctx, loader))
    } else {
        None
    };
    // `CRATONVM_DEFINE_RECORDS_SUPERTYPE_INITIATION` (wave 79, L5): see
    // `supertype_initiation_record_ns`.
    let record_ns = {
        let loader = ctx.read_native_pin(loader_pin, loader_obj);
        supertype_initiation_record_ns(ctx, loader)
    };
    answers.begin(&class_file, names.len());
    // JIT round 23 wave 10 (`CRATONVM_DEFINE_INTERFACES_BEFORE_SUPER`): ask in
    // HotSpot's order; `slot` keeps the pre-pass's own order for `answers`.
    let has_super = class_file
        .super_class
        .as_deref()
        .is_some_and(|s| !s.is_empty() && s != "java/lang/Object");
    for slot in supertype_ask_order(names.len(), has_super) {
        let internal_name = names[slot].clone();
        if let Some(ns) = dictionary_ns {
            let own = ctx.class_id_defined_by_loader_exact(&internal_name, ns);
            if own.is_some() {
                answers.note_present(slot, own);
                continue;
            }
            if crate::classloader::supertype_link_known(ctx, ns, &internal_name) {
                // Linked by the identity the row kept (wave 96, lane L5).
                match crate::classloader::supertype_link_answer(ctx, ns, &internal_name) {
                    Some(id) => answers.note_known_answer(&*ctx, slot, id),
                    None => answers.note_present(slot, None),
                }
                continue;
            }
        }
        let dotted_name = ctx.create_string(&internal_name.replace('/', "."));
        let loader = ctx.read_native_pin(loader_pin, loader_obj);
        // Under the loader's monitor when it is not parallel-capable
        // (`--jdk-only`), as a VM-initiated load (wave 29, lane L5).
        let result = crate::classloader_real::invoke_load_class_as_the_vm(ctx, loader, dotted_name);
        let cause = match result {
            // JIT round 20 lane superAcc: the answer is the supertype the
            // define links; an answer of another name is HotSpot's
            // `NoClassDefFoundError` (`load_instance_class_impl` returns null).
            Ok(Some(Value::Object(Some(mirror))))
                if answers
                    .note_answer(&*ctx, slot, &internal_name, mirror)
                    .is_err() =>
            {
                None
            }
            Ok(Some(Value::Object(Some(mirror)))) => {
                if let Some(ns) = dictionary_ns {
                    let answer = crate::lang_class::mirror_class_id(&*ctx, mirror);
                    crate::classloader::note_supertype_link(ctx, ns, &internal_name, answer);
                }
                if let Some(ns) = record_ns {
                    if let Err(e) = note_supertype_initiation(ctx, ns, &internal_name, Some(mirror))
                    {
                        ctx.unpin_native_roots(loader_pin);
                        return Err(e);
                    }
                }
                continue;
            }
            // A circular hierarchy leaves as itself (see
            // `preload_supertypes_via_loader`), not as a missing supertype.
            Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(exception))
                if is_class_circularity_error(ctx, exception) =>
            {
                ctx.unpin_native_roots(loader_pin);
                return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                    exception,
                ));
            }
            Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(exception)) => {
                Some(exception)
            }
            Err(other) => {
                ctx.unpin_native_roots(loader_pin);
                return Err(other);
            }
            Ok(_) => None,
        };
        let cause_pin = cause.map(|c| (ctx.pin_native_root(c), c));
        let error = crate::jboss_module_loader::alloc_single_message_exception(
            ctx,
            "java/lang/NoClassDefFoundError",
            1,
            &internal_name,
        );
        // Typed so the audit starts `error` here: it is used below with no GC
        // point in between (w16-e triage; the flat scanner read the return
        // type's `error::` path as an argument named `error`).
        let error: ObjectRef = match error {
            Ok(error) => error,
            Err(e) => {
                ctx.unpin_native_roots(loader_pin);
                return Err(e);
            }
        };
        if let Some((pin, cause)) = cause_pin {
            let cause = ctx.read_native_pin(pin, cause);
            ctx.set_field_by_name(error, "cause", Value::Object(Some(cause)));
        }
        ctx.unpin_native_roots(loader_pin);
        return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
            error,
        ));
    }
    // `CRATONVM_DEFINE_ASKS_FOR_OBJECT`: an `Object` superclass, last. The
    // loader has the final word here too: a refusal is HotSpot's
    // `NoClassDefFoundError: java/lang/Object`, caused by what it threw.
    if let (Some(ns), true) = (dictionary_ns, define_asks_for_object(&*ctx, &class_file)) {
        let refused =
            match ask_defining_loader_for_object(ctx, loader_pin, loader_obj, ns, record_ns) {
                Ok(refused) => refused,
                Err(e) => {
                    ctx.unpin_native_roots(loader_pin);
                    return Err(e);
                }
            };
        if let Some(cause) = refused {
            let cause_pin = cause.map(|c| (ctx.pin_native_root(c), c));
            let error = crate::jboss_module_loader::alloc_single_message_exception(
                ctx,
                "java/lang/NoClassDefFoundError",
                1,
                "java/lang/Object",
            );
            let error: ObjectRef = match error {
                Ok(error) => error,
                Err(e) => {
                    ctx.unpin_native_roots(loader_pin);
                    return Err(e);
                }
            };
            if let Some((pin, cause)) = cause_pin {
                let cause = ctx.read_native_pin(pin, cause);
                ctx.set_field_by_name(error, "cause", Value::Object(Some(cause)));
            }
            ctx.unpin_native_roots(loader_pin);
            return Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(
                error,
            ));
        }
    }
    ctx.unpin_native_roots(loader_pin);
    Ok(keep_parse.then_some((source, class_file)))
}

/// What a backend "already defined" failure means for the `defineClass` call
/// that provoked it.
///
/// The backend probe is keyed on `(loader_id, name)` where `loader_id` is a
/// synthetic NAMESPACE number
/// (`class_manager.rs::define_class_shared_with_options`), so the error alone
/// cannot tell the two cases below apart. Only the DEFINING LOADER OBJECT can.
enum DuplicateDefine {
    /// Not an "already defined" failure — the caller keeps its own error path.
    NotDuplicate,
    /// **This exact loader object has already defined this name.** JVMS §5.3.5
    /// forbids it and HotSpot raises `java.lang.LinkageError`.
    SameLoaderObject,
    /// A class of this name exists and this loader did NOT define it — a
    /// namespace collision inside CratonVM's flat store, or a delegation gap
    /// that re-reached `findClass`. Serve the existing mirror.
    ServeExisting(ObjectRef),
}

/// Decide which of the two an "already defined" backend error is.
///
/// # Why this exists, and why it is not simply "delete the tolerance"
///
/// Until 2026-08-11 every "already defined" error served the existing mirror,
/// so `defineClass` NEVER raised the `LinkageError` HotSpot raises for a
/// duplicate definition — measured in both modes with a `ClassLoader` subclass
/// calling `defineClass(name, bytes, 0, len)` twice.
///
/// The tolerance is not decoration, though, and the case it covers is real:
/// during a Tomcat webapp stop/start loop the ~14th `WebappClassLoader` in a
/// process began colliding with an earlier, already-finished one over
/// `org/apache/catalina/loader/JdbcLeakPrevention`, and surfacing the backend
/// error there broke the container lifecycle. On HotSpot those are two
/// DIFFERENT loaders, each entitled to its own copy; what collided was
/// CratonVM's namespace numbering, not the loaders.
///
/// So the discriminator is loader-OBJECT identity, and the two shapes separate
/// cleanly:
///
/// | shape | HotSpot | here |
/// |---|---|---|
/// | same loader object defines a name twice | `LinkageError` | `SameLoaderObject` |
/// | two distinct loaders, one namespace | both succeed | `ServeExisting` |
///
/// # The bias, stated
///
/// `SameLoaderObject` is only ever returned on a POSITIVE identification. The
/// defining-loader record is an `ObjectRef` and a moving collection can leave a
/// stale pointer, so "not recorded" and "recorded elsewhere" both fall to
/// `ServeExisting`. A missed `LinkageError` is what this VM did yesterday; a
/// spurious one is a new way to break a workload that was working.
/// Is a backend define error a duplicate definition of `internal_name`?
/// Either spelling counts (see `classify_duplicate_define`).
fn is_duplicate_define_message(internal_name: &str, msg: &str) -> bool {
    !internal_name.is_empty()
        && (msg.contains("already defined") || msg.contains("DuplicateClassDefinition"))
}

/// Is the running `ClassLoader.defineClass1` reached from
/// `BuiltinClassLoader.defineClass`, through nothing but the `ClassLoader` /
/// `SecureClassLoader` define chain?
fn called_from_builtin_loader_define(ctx: &dyn NativeContext) -> bool {
    cratonvm_native_api::find_in_caller_frames(ctx, |id| {
        match ctx.class_name_of_id(id).as_deref() {
            Some("jdk/internal/loader/BuiltinClassLoader") => std::ops::ControlFlow::Break(true),
            Some("java/lang/ClassLoader") | Some("java/security/SecureClassLoader") => {
                std::ops::ControlFlow::Continue(())
            }
            _ => std::ops::ControlFlow::Break(false),
        }
    })
    .unwrap_or(false)
}

fn classify_duplicate_define(
    ctx: &mut dyn NativeContext,
    loader_obj: ObjectRef,
    internal_name: &str,
    loader_id: u32,
    msg: &str,
) -> DuplicateDefine {
    // Both spellings: the backend has raised `DuplicateClassDefinition`
    // (whose `Debug` text `define_class_full` hands back) since 2026-08-28;
    // "already defined" is the older ICCE wording other producers still use.
    // Matching only the old words made every arm below dead code (gc-common
    // w11-e).
    if !is_duplicate_define_message(internal_name, msg) {
        return DuplicateDefine::NotDuplicate;
    }
    // `CRATONVM_DBG_DUPDEF=1` -- name every "already defined" backend error and
    // the verdict it got. This exists because the two verdicts are otherwise
    // indistinguishable from outside: a workload that never reaches
    // `ServeExisting` proves nothing about the tolerance still working, and one
    // that never reaches `SameLoaderObject` proves nothing about the refusal.
    // Both arms print, so a probe can assert it exercised the arm it claims to.
    let dbg = cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DUPDEF").is_some();
    // zt t16/df6 (define page D6): the name is held by THIS loader object's
    // own define whose `addClass` upcall is still in flight (a define that
    // raced past `wait_for_unbound_define`). That class is in no by-name
    // index until its upcall returns, so the questions below cannot see it;
    // its defining-loader row names the loader. HotSpot's second definer
    // would have waited and then met the duplicate definition (or defined,
    // had the first failed); the duplicate definition is the answer the
    // define gave before the late binding existed.
    if let Some(unbound) = ctx.unbound_define_of(internal_name, loader_id) {
        let tables = crate::classloader::loader_tables(ctx);
        if crate::classloader::defining_loader_for(tables, unbound.as_u32())
            .is_some_and(|def| def.as_ptr() == loader_obj.as_ptr())
        {
            if dbg {
                eprintln!(
                    "[DUPDEF] SameLoaderObject(unbound) {internal_name} loader_id={loader_id}"
                );
            }
            return DuplicateDefine::SameLoaderObject;
        }
    }
    // THE one question that may raise: did this exact object define it?
    if crate::classloader::class_defined_by_this_loader_object(ctx, loader_obj, internal_name)
        .is_some()
    {
        if dbg {
            eprintln!("[DUPDEF] SameLoaderObject {internal_name} loader_id={loader_id}");
        }
        return DuplicateDefine::SameLoaderObject;
    }
    // Interpreter round i5 wave 101, lane L5: the BUILT-IN application or
    // platform loader re-defining a name it already defined
    // (`L5W99ModuleVersionClauses` `dup`: `Lookup.defineClass` of `app.Box`'s
    // own bytes, `app.Box` in module `app` of loader 'app'; HotSpot's
    // `LinkageError: loader 'app' attempted duplicate class definition ...`).
    // A built-in loader's classes carry no defining-loader row, so the object
    // question above never sees them, and the tolerance arms below served the
    // existing class. Positive identification only, as above: the existing
    // class must be one the VM KNOWS this built-in loader defined.
    let builtin_ordinal = if crate::classloader::is_application_class_loader(ctx, loader_obj) {
        Some(cratonvm_types::ClassLoaderId::NATIVE_APPLICATION)
    } else if crate::classloader::is_platform_class_loader(ctx, loader_obj) {
        Some(cratonvm_types::ClassLoaderId::NATIVE_EXTENSION)
    } else {
        None
    };
    if let Some(ordinal) = builtin_ordinal {
        let existing = crate::classloader::find_loaded_class_for_loader(ctx, loader_obj, internal_name)
            .and_then(|mirror| ctx.class_id_from_mirror(mirror));
        if let Some(cid) = existing {
            if ctx.loader_id_of_class_definite(cid) == Some(ordinal as i32)
                && crate::classloader::defining_loader_for(
                    crate::classloader::loader_tables(ctx),
                    cid.as_u32(),
                )
                .is_none()
            {
                // `BuiltinClassLoader.loadClassOrNull` defines under the
                // per-name `getClassLoadingLock`, so on HotSpot a define it
                // makes of a name the loader already holds cannot happen. Here
                // the VM resolves a supertype of another class natively, with
                // no Java lock, so a Java-level define can lose that race
                // (Groovy's parallel `loadDgmInfo` vs `groovy.lang.Closure`).
                // Serve the winner; only a define that does NOT come from the
                // built-in loader's own `defineClass` (`Lookup.defineClass`)
                // is a genuine duplicate.
                if called_from_builtin_loader_define(ctx) {
                    if dbg {
                        eprintln!(
                            "[DUPDEF] ServeExisting(built-in loader race) {internal_name} loader_id={loader_id}"
                        );
                    }
                    return DuplicateDefine::ServeExisting(ctx.get_class_mirror(cid));
                }
                if dbg {
                    eprintln!(
                        "[DUPDEF] SameLoaderObject(built-in loader) {internal_name} loader_id={loader_id}"
                    );
                }
                return DuplicateDefine::SameLoaderObject;
            }
        }
    }
    // gc-common w11-e: the name collides with a class in this namespace whose
    // recorded defining loader was collected. That can only be a
    // built-in-namespace class (user namespaces unload with their loader),
    // and its orphan marker hides it from every live loader, so none of the
    // probes below can see it and the define would fail with `LinkageError`.
    // HotSpot would define a fresh copy. CratonVM cannot hold two classes
    // of one name in one namespace, so the new loader adopts the existing
    // class. `register_defining_loader` clears the orphan marker, so the
    // adopter (and its children) can see it again.
    if let Some(class_id) = ctx.class_id_defined_by_loader_exact(internal_name, loader_id) {
        if crate::classloader::is_defining_loader_orphaned(
            crate::classloader::loader_tables(ctx),
            class_id.as_u32(),
        ) {
            if dbg {
                eprintln!(
                    "[DUPDEF] ServeExisting(adopted orphan) {internal_name} loader_id={loader_id}"
                );
            }
            crate::classloader::register_defining_loader(
                crate::classloader::loader_tables(ctx),
                class_id.as_u32(),
                loader_obj,
            );
            return DuplicateDefine::ServeExisting(ctx.get_class_mirror(class_id));
        }
    }
    // A namespace hit whose recorded defining loader IS this object is the same
    // fact reached the other way round — the record can be keyed by namespace
    // before the object association is made.
    if loader_id != 0 {
        if let Some(class_id) = ctx.class_id_defined_by_loader_exact(internal_name, loader_id) {
            let same =
                crate::classloader::defining_loader_for(crate::classloader::loader_tables(ctx), class_id.as_u32())
                    .is_some_and(|def| def.as_ptr() == loader_obj.as_ptr());
            if same {
                if dbg {
                    eprintln!(
                        "[DUPDEF] SameLoaderObject(via namespace) {internal_name} loader_id={loader_id}"
                    );
                }
                return DuplicateDefine::SameLoaderObject;
            }
            // A namespace hit this loader did not define: the collision shape.
            if dbg {
                eprintln!(
                    "[DUPDEF] ServeExisting(namespace collision) {internal_name} loader_id={loader_id}"
                );
            }
            crate::classloader::register_defining_loader(
                crate::classloader::loader_tables(ctx),
                class_id.as_u32(),
                loader_obj,
            );
            return DuplicateDefine::ServeExisting(ctx.get_class_mirror(class_id));
        }
    }
    match crate::classloader::find_loaded_class_for_loader(ctx, loader_obj, internal_name) {
        Some(mirror) => {
            if dbg {
                eprintln!("[DUPDEF] ServeExisting(visible) {internal_name} loader_id={loader_id}");
            }
            DuplicateDefine::ServeExisting(mirror)
        }
        None => {
            if dbg {
                eprintln!("[DUPDEF] NotDuplicate {internal_name} loader_id={loader_id}");
            }
            DuplicateDefine::NotDuplicate
        }
    }
}

/// `CRATONVM_DEFINE_NAMELESS_DUPLICATE_TEXT` (zt t15/x12,
/// `zgc-zt13x10-review-residuals-CLOSED-20261019.md` item 6; default ON, `=0` kill;
/// every collector, both modes): a `defineClass1` / `defineClass2` with a null
/// name whose bytes this very loader object has already defined raises
/// [`duplicate_define_error`] for the class the bytes name (`this_class`), as
/// the named path does: HotSpot 25 names the class from the bytes, so its
/// `LinkageError` reads `loader X attempted duplicate class definition for
/// a.b.C. (a.b.C is in unnamed module of loader X ...)` either way. Before,
/// the nameless arm could not classify (no caller name) and fell to
/// `define_class_linkage_error("")`, without the parenthetical. Only the exact
/// loader-object answer (`class_defined_by_this_loader_object`) decides,
/// which touches no table; every other verdict keeps the old refusal. `None`:
/// not such a duplicate, unreadable bytes, or the switch off.
fn nameless_duplicate_define_error(
    ctx: &mut dyn NativeContext,
    loader_pin: (usize, ObjectRef),
    bytes: &[u8],
    msg: &str,
) -> Option<MethodCallFailed> {
    if !(msg.contains("DuplicateClassDefinition") || msg.contains("already defined"))
        || !cratonvm_types::flags::runtime_flag_default_on(
            "CRATONVM_DEFINE_NAMELESS_DUPLICATE_TEXT",
        )
    {
        return None;
    }
    let internal_name = crate::classloader::extract_this_class_name(bytes)?;
    let loader_obj = ctx.read_native_pin(loader_pin.0, loader_pin.1);
    crate::classloader::class_defined_by_this_loader_object(ctx, loader_obj, &internal_name)?;
    // Re-read, as the named arm does after its classification.
    let loader_obj = ctx.read_native_pin(loader_pin.0, loader_pin.1);
    Some(duplicate_define_error(ctx, loader_obj, &internal_name))
}

#[cfg(test)]
mod zt15x12_nameless_duplicate_text_tests {
    use super::*;
    use crate::test_utils::MockNativeContext;

    /// zt t15/x12 (x10 page item 6): a refusal that is not a duplicate,
    /// bytes without a `this_class` and the `=0` kill all keep the old
    /// refusal (`None`, before the loader is read); the two nameless arms ask
    /// only after their adoption declined.
    #[test]
    fn zt15x12_the_nameless_duplicate_text_is_gated() {
        let mut ctx = MockNativeContext::new();
        // SAFETY: never dereferenced -- every call below returns before it
        // reads the loader.
        let loader = unsafe { ObjectRef::from_raw(0x1000 as *mut u8) };
        let dup = "DuplicateClassDefinition";
        assert!(nameless_duplicate_define_error(&mut ctx, (0, loader), &[], "ClassFormatError")
            .is_none());
        assert!(
            nameless_duplicate_define_error(&mut ctx, (0, loader), &[0xCA, 0xFE], dup).is_none(),
            "no readable this_class"
        );
        {
            let _kill = cratonvm_types::flags::override_thread(
                cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                    "CRATONVM_DEFINE_NAMELESS_DUPLICATE_TEXT",
                    Some("0"),
                )]),
            );
            assert!(!cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_DEFINE_NAMELESS_DUPLICATE_TEXT"
            ));
        }
        let src = include_str!("lang_system.rs").replace("\r\n", "\n");
        // zt t17/x13: the needles are split (`concat!`) so this module, which
        // sits ABOVE the natives, never holds their full headers: unsplit,
        // `find` matched these literals first, here and in every
        // `include_str!("lang_system.rs")` pin of `classloader.rs`.
        for (start, end) in [
            (
                concat!("pub(crate) fn native_classloader_", "define_class1("),
                concat!("pub(crate) fn native_classloader_", "define_class2("),
            ),
            (
                concat!("pub(crate) fn native_classloader_", "define_class2("),
                concat!("pub(crate) fn native_classloader_", "define_class0("),
            ),
        ] {
            let from = src.find(start).expect("start");
            let body = &src[from..from + src[from..].find(end).expect("end")];
            let adopt = body.find("adopt_nameless_add_class_orphan(").expect("adoption");
            let text = body
                .find("nameless_duplicate_define_error(ctx, (p, o), &bytes, &msg)")
                .expect("the text");
            assert!(adopt < text, "{start}: after the adoption");
        }
    }
}

/// zt t25/df11b (define page §9d): a define in the `Extension` namespace (the
/// platform loader object, [`define_loader_namespace`]) of a loaded runtime-image
/// class of a platform module (`classloader::platform_image_class_loaded`):
/// HotSpot's duplicate-definition `LinkageError`, named after the loader, where
/// CratonVM defined a second class of the name (the image class carries the boot
/// tag, so the class manager's `Extension` duplicate probe misses it). `None`
/// for every other define.
fn platform_image_duplicate_refusal(
    ctx: &mut dyn NativeContext,
    loader: Option<&Value>,
    loader_id: u32,
    internal_name: &str,
) -> Option<MethodCallFailed> {
    if loader_id != cratonvm_types::ClassLoaderId::NATIVE_EXTENSION {
        return None;
    }
    let Some(Value::Object(Some(loader_obj))) = loader.copied() else {
        return None;
    };
    // The module-table read can run Java on a package's first ask.
    let pin = ctx.pin_native_root(loader_obj);
    let image = crate::classloader::platform_image_class_loaded(ctx, internal_name);
    let loader_obj = ctx.read_native_pin(pin, loader_obj);
    ctx.unpin_native_roots(pin);
    image?;
    Some(duplicate_define_error(ctx, loader_obj, internal_name))
}

/// The `LinkageError` a duplicate definition raises, named after the loader the
/// way HotSpot names it.
fn duplicate_define_error(
    ctx: &mut dyn NativeContext,
    loader_obj: ObjectRef,
    internal_name: &str,
) -> MethodCallFailed {
    // `--jdk-only`: HotSpot's whole text, from the loader's `nameAndId` and
    // its parent's (interpreter round i1 wave 38, lane L5; probe
    // `L5W38DuplicateDefineMessage`), and `--compatible` with
    // `CRATONVM_COMPAT_MODULE_MESSAGES` (wave 49, lane L6). Built before the
    // allocation, which can move `loader_obj`.
    if ctx.messages_name_user_loaders() {
        if let Some(message) =
            crate::classloader_real::duplicate_definition_message(ctx, loader_obj, internal_name)
        {
            return match crate::jboss_module_loader::alloc_single_message_exception(
                ctx,
                "java/lang/LinkageError",
                1,
                &message,
            ) {
                Ok(exc) => MethodCallFailed::ExceptionThrown(exc),
                Err(e) => e,
            };
        }
    }
    // HotSpot prints `<loader class name> @<identity hash>`. The class name is
    // the part that identifies the loader to a reader; the hash is why no test
    // may assert the whole string.
    let loader = ctx
        .class_name_of_id(ctx.class_id_of_object(loader_obj))
        .map(|n| n.replace('/', "."))
        .unwrap_or_else(|| "<unknown>".to_string());
    LinkageError::DuplicateClassDefinition {
        class_name: internal_name.to_string(),
        loader,
    }
    .into()
}

pub(crate) fn native_classloader_define_class1(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: loader(0), name(1), bytes(2), offset(3), length(4), pd(5), source(6)
    // `CRATONVM_DBG_DEFINE_CENSUS` phase clocks (interpreter round i4 wave 88,
    // lane L5).
    let entry_clock = cratonvm_classloading::define_census::phase_start();
    // `native-total`: this whole body, every exit (wave 95, lane L5).
    let _census_total = cratonvm_classloading::define_census::phase_guard(
        cratonvm_classloading::define_census::DefinePhase::NativeTotal,
        entry_clock,
    );
    let name = match args.get(1) {
        Some(Value::Object(Some(s))) => {
            let n = ctx.read_string(*s).unwrap_or_default();
            // JDK uses dot-separated names; convert to slash-separated
            n.replace('.', "/")
        }
        _ => String::new(),
    };
    if let Ok(filter) = cratonvm_types::flags::runtime_var("CRATONVM_DBG_DEFINE_STACK_FILTER") {
        if !filter.is_empty() && name.contains(filter.as_str()) {
            eprintln!("[DBG_DEFINE_STACK] defineClass1({name})");
            for entry in ctx.capture_stack_trace() {
                eprintln!("    at {}.{}", entry.class_name, entry.method_name);
            }
        }
    }

    // `--jdk-only`: HotSpot's exceptions for the arguments and the header
    // (interpreter round i1 wave 42, lane L5; probe `L5W42DefineClassErrors`),
    // and `--compatible` with `CRATONVM_COMPAT_MODULE_MESSAGES` (wave 49, lane
    // L6). Only message texts and exception types hang off this.
    let jdk_only = ctx.messages_name_user_loaders();
    let byte_array = match args.get(2) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => {
            return Err(RuntimeError::NullPointerException {
                // `JNU_ThrowNullPointerException(env, 0)`: a null message.
                message: (!jdk_only).then(|| "defineClass1: byte[] is null".to_string()),
            }
            .into());
        }
    };

    if jdk_only {
        let raw = |idx: usize| match args.get(idx) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        if let Some(err) =
            jdk_only_define_region_error(raw(3), raw(4), ctx.array_length(byte_array))
        {
            return Err(err);
        }
    }
    let offset = read_define_class_nonnegative_int(args, 3)?;
    let length = read_define_class_nonnegative_int(args, 4)?;
    let bytes = read_byte_array_define_class_slice(ctx, byte_array, offset, length)?;
    // zt t16/pl7: `JVM_DefineClassWithSource` counts the define and its
    // bytes before parsing (`sun.cls.defineAppClasses` / `appClassBytes`).
    let counted = ctx.note_jvm_define_class(bytes.len());
    // zt t16/pl7: and times it (`sun.cls.defineAppClassTime` / `.self`;
    // `CRATONVM_PERF_CLASS_TIMERS`, default ON, inert under `=0`) to this
    // native's end, as `jvm_define_class_common`'s `PerfClassTraceTime`.
    let _define_trace = cratonvm_types::perf_class_trace::ClassTraceTime::begin(
        counted.then(|| ctx.class_trace_counters()).flatten(),
        cratonvm_types::perf_class_trace::ClassTraceEvent::DefineClass,
    );
    if jdk_only {
        if let Some(err) = jdk_only_classfile_header_error(ctx, &name, &bytes) {
            return Err(err);
        }
    }
    validate_classfile_header(&name, "defineClass1", &bytes)?;
    // `CRATONVM_DBG_DEFINE_CENSUS` (interpreter round i4 wave 93, lane L5):
    // `native-entry` split into `entry-args` (to here: the name, the byte
    // copy, the header checks) and `entry-loader` (from here: the boot-append
    // probe, the loader's namespace, the protection domain's URL, the pins).
    cratonvm_classloading::define_census::phase_end(
        cratonvm_classloading::define_census::DefinePhase::EntryArgs,
        entry_clock,
    );
    let entry_loader_clock = cratonvm_classloading::define_census::phase_start();

    // Bootstrap-appended-jar classes (Instrumentation.appendToBootstrapClassLoaderSearch,
    // e.g. Mockito's MockMethodDispatcher) belong to the BOOTSTRAP loader. On
    // HotSpot a user loader's parent delegation reaches the appended boot
    // search before its own findClass runs, so findClass never defines a
    // per-loader copy. CratonVM's real-JDK platform-loader delegation misses
    // the appended jar (its BuiltinClassLoader path never consults the flat
    // store), the JDK loadClass bytecode falls through to findClass, and the
    // duplicate copy's <clinit> then fails Mockito's null-loader assertion.
    // Serve the already-defined bootstrap copy instead — same observable
    // outcome as HotSpot's delegation order.
    if ctx.is_bootstrap_appended_class(&name) {
        if let Some(cid) = ctx.class_id_by_name(&name) {
            return Ok(Some(Value::Object(Some(ctx.get_class_mirror(cid)))));
        }
    }

    // Real JDK ClassLoader instances have no VM-owned integer field. Derive a
    // namespace solely through the stable loader-identity map; reading a raw
    // instance slot here can mistake an unrelated JDK field for a loader id.
    let (loader_id, user_loader_define) = define_loader_namespace(ctx, args.first(), &name);
    // zt t25/df11b: the platform loader's define of a loaded image class of its modules.
    if let Some(err) = platform_image_duplicate_refusal(ctx, args.first(), loader_id, &name) {
        return Err(err);
    }
    // The caller's ProtectionDomain, decoded through the ONE reader that
    // understands both PD shapes. Six copies of an inline decode used to
    // stand here, and all six read `CodeSource.location` with
    // `read_string` -- which fails on a real `java.net.URL`, a different
    // concrete class -- so every real-JDK-constructed CodeSource silently
    // lost its URL and the defined class came back carrying the
    // synthesised `file:/runtime-defined/<name>.class` instead of the
    // caller's. See `extract_pd_code_source_url`.
    let pd_url = match args.get(5) {
        Some(Value::Object(Some(pd))) => {
            crate::classloader::extract_pd_code_source_url_for_define(ctx, *pd)
        }
        _ => None,
    };

    // JVMS В§5.3.5 вЂ” resolve direct supertypes through the DEFINING loader before
    // linking. `define_class_full` resolves the superclass/interfaces only via the
    // global classpath; a user loader whose classes are invisible there (Tomcat's
    // `WebappClassLoader` serving `/WEB-INF/lib` jars) would otherwise fail to
    // define a class whose super lives in the same jar (JSTL `JstlCoreTLV` в†’
    // `JstlBaseTLV`). No-op for built-in/app-loader defines.
    // GC: `preload_supertypes_via_loader` LOADS CLASSES, and `define_class_full`
    // / `get_class_mirror` below allocate, so the loader read out of `args` is a
    // pre-call address at every later use. `args` is the snapshot
    // `safe_native_call_impl` took before this native was entered; a collection
    // that happens INSIDE the callback does not rewrite it. Pin once and re-read
    // at each use. Three sibling entry points carry the identical shape — this
    // is one of them. See `internal/audits/wide-tranche-triage-20260907.md`.
    let loader_pin = match args.first() {
        Some(Value::Object(Some(o))) => Some((ctx.pin_native_root(*o), *o)),
        _ => None,
    };
    // gc-common w16-e: the ProtectionDomain is pinned BEFORE the supertype
    // link, which runs the loader's Java `loadClass`. It used to be read out of
    // `args` after that call, so under `--jdk-only` a collection inside
    // `loadClass` left the mirror's `protectionDomain` a pre-move address.
    let pd_pin = pin_protection_domain_arg(ctx, args, 5);
    // The rest runs as one closure so every arm, the `?`s included, releases
    // the pins above before the native returns (interpreter round i2 wave 62,
    // lane L4: the `[pin-balance] LEAK` census named the `defineClass0`
    // sibling; this one and `defineClass2` had the same shape).
    let pin_floor = loader_pin.map(|(p, _)| p).or(pd_pin.map(|(p, _)| p));
    let mut answers = SupertypeAnswers {
        keep_miss_object: true,
        ..SupertypeAnswers::default()
    };
    let result = (|| -> MethodCallResult {
        // `CRATONVM_DEFINE_ONE_PARSE` (interpreter round i3 wave 72, lane L5):
        // the pre-pass's parse of `bytes`, handed to the define below.
        let keep_parse = ctx.define_one_parse();
        cratonvm_classloading::define_census::phase_end(
            cratonvm_classloading::define_census::DefinePhase::EntryLoader,
            entry_loader_clock,
        );
        cratonvm_classloading::define_census::phase_end(
            cratonvm_classloading::define_census::DefinePhase::NativeEntry,
            entry_clock,
        );
        let prepass_clock = cratonvm_classloading::define_census::phase_start();
        // JIT round 20 lane superAcc: the defining loader's `loadClass`
        // answers, linked by identity (see `SupertypeAnswers`). Round 22
        // wave 1 (lane review): declared before the closure, so its miss root
        // is released on every exit below.
        let preparsed = match loader_pin {
            Some((p, o)) => {
                let loader_obj = ctx.read_native_pin(p, o);
                link_supertypes_refusing_circularity(
                    ctx,
                    p,
                    loader_obj,
                    &name,
                    &bytes,
                    keep_parse,
                    &mut answers,
                )?
            }
            None => None,
        };
        let (superclass_id_override, interface_id_overrides) = answers.overrides();
        cratonvm_classloading::define_census::phase_end(
            cratonvm_classloading::define_census::DefinePhase::SupertypePrepass,
            prepass_clock,
        );

        // `CRATONVM_DBG_DEFINE=1` + `CRATONVM_DBG_DUPCLASS_FILTER=<substring>` --
        // the JAVA callers of this `defineClass`. The class-manager side of the
        // same switch prints the name and the loader; what it cannot print is
        // WHICH Java code decided to define it, and for a duplicate-identity
        // defect that is the whole question -- a loader that defines a class its
        // parent already has is either an isolating loader doing its job or a
        // delegation that failed, and only the caller chain separates the two.
        if cratonvm_types::flags().loader.dbg_define
            && cratonvm_types::flags()
                .loader
                .dbg_dupclass_filter
                .as_deref()
                .is_some_and(|f| name.contains(f))
        {
            let callers: Vec<String> = ctx
                .frame_class_ids()
                .into_iter()
                .rev()
                .take(12)
                .filter_map(|cid| ctx.class_name_of_id(cid))
                .collect();
            eprintln!(
            "[DEFINE-DBG-JAVA] defineClass name={name} loader_id={loader_id} callers={callers:?}"
        );
        }

        // A NULL loader is the BOOT loader: JNI `DefineClass(env, name, NULL, ..)`
        // (`jni.rs::jni_define_class_through_loader` passes it here) and
        // `--jdk-only`'s `JavaLangAccess.defineClass(null, ..)`. HotSpot's
        // `jni_DefineClass` / `JVM_DefineClassWithSource` define in the boot
        // loader, where `java.*` is legal; `loader_id` `0` is the APPLICATION
        // namespace. Java's `ClassLoader.defineClass` always passes its `this`.
        // JIT round 15 wave 6, lane loader6 (exact patch page
        // `r15w6-loader6-define-class1-null-loader-patch-FIXED-20260930.md`).
        // zs s7/nd6: the mirror object first, so a heap that cannot hold it
        // fails this define before anything is published (every reference
        // still used below is pinned).
        // zt t6/nd4: timed as its own census row (`mirror-reserve`).
        // zt t16/df6 (define page D6): a define of this name whose `addClass`
        // upcall is in flight on another thread is waited for, as HotSpot's
        // placeholder makes a second definer wait. Only pinned references
        // are live here.
        let bind_late = loader_pin.is_some()
            && crate::classloader::define_bind_after_add_class_enabled(ctx, loader_id);
        if bind_late {
            // zt t30/dc: the `bind-wait` census row.
            let wait_clock = cratonvm_classloading::define_census::phase_start();
            crate::classloader::wait_for_unbound_define(ctx, &name, &bytes, loader_id)?;
            cratonvm_classloading::define_census::phase_end(
                cratonvm_classloading::define_census::DefinePhase::BindWait,
                wait_clock,
            );
        }
        let reserve_clock = cratonvm_classloading::define_census::phase_start();
        // zt t7/cl: on ZGC, room in the loader's `classes` list for the
        // `addClass` upcall below, before anything is published (after the
        // supertype pre-pass, whose `loadClass` can define into this loader).
        let loader_now = loader_pin.map(|(p, o)| ctx.read_native_pin(p, o));
        crate::classloader::presize_for_add_class(ctx, loader_now, &bytes, &name)?;
        let reservation = reserve_defined_class_mirror(ctx, &define_name_hint(&name, &bytes));
        cratonvm_classloading::define_census::phase_end(
            cratonvm_classloading::define_census::DefinePhase::MirrorReserve,
            reserve_clock,
        );
        // zt t9/df: malformed bytes keep their `ClassFormatError` on a spent heap.
        let reservation =
            crate::classloader::refusal_yields_to_format_error(ctx, reservation, &bytes, &name)?;
        let null_loader_boot = matches!(args.first(), Some(Value::Object(None)))
            && cratonvm_types::flags::runtime_flag_default_on(
                "CRATONVM_DEFINE_CLASS1_NULL_LOADER_BOOTSTRAP",
            );
        let opts = cratonvm_native_api::DefineClassFull {
            code_source_url: pd_url,
            // The load-time transformers' `protectionDomain` (both modes since
            // interpreter round i1 wave 47).
            protection_domain: pd_pin.map(|(p, o)| ctx.read_native_pin(p, o)),
            bootstrap_namespace: null_loader_boot,
            privileged_define: null_loader_boot,
            preparsed: preparsed_for_define(preparsed),
            superclass_id_override,
            interface_id_overrides,
            user_loader_define,
            defining_loader: loader_pin.map(|(p, o)| ctx.read_native_pin(p, o)),
            // zt t16/df6 (define page §1, corner 2): the name is bound after
            // the `addClass` upcall below (`settle_unbound_define`).
            bind_name_after_add_class: bind_late,
            ..Default::default()
        };
        // zt t18/df7 (x13 item 21d): kept for one define after a withdrawn rival,
        // without the pre-pass parse (a second holder of its `Arc` would make
        // the define below parse the bytes again).
        // zt t30/dc: the `bind-retry-clone` census row (bind arm only).
        let clone_clock = cratonvm_classloading::define_census::phase_start().filter(|_| bind_late);
        let retry_opts = bind_late.then(|| cratonvm_native_api::DefineClassFull {
            preparsed: None,
            ..opts.clone()
        });
        cratonvm_classloading::define_census::phase_end(
            cratonvm_classloading::define_census::DefinePhase::BindRetryClone,
            clone_clock,
        );
        let defined = ctx.define_class_full(&name, &bytes, loader_id, opts);
        let reserved = take_mirror_reservation(ctx, reservation);
        // zt t18/df7 (x13 review item 21d): a define that raced past the wait
        // above can meet another define of this name that is still unbound in
        // this loader (the class manager's duplicate check counts it). Wait for
        // it to settle and, when it was withdrawn (its upcall threw), define
        // once more, as HotSpot's second definer waits on the placeholder and
        // then defines its own bytes. Only pinned references are live here.
        let (defined, reserved) = match (defined, retry_opts) {
            (Err(msg), Some(mut again)) => {
                let duplicate =
                    msg.contains("already defined") || msg.contains("DuplicateClassDefinition");
                if crate::classloader::redefine_after_unbound_define_settles(
                    ctx, &name, &bytes, loader_id, duplicate,
                )? {
                    again.protection_domain = pd_pin.map(|(p, o)| ctx.read_native_pin(p, o));
                    let reservation =
                        reserve_defined_class_mirror(ctx, &define_name_hint(&name, &bytes));
                    let reservation = crate::classloader::refusal_yields_to_format_error(
                        ctx,
                        reservation,
                        &bytes,
                        &name,
                    )?;
                    let defined = ctx.define_class_full(&name, &bytes, loader_id, again);
                    (defined, take_mirror_reservation(ctx, reservation))
                } else {
                    (Err(msg), reserved)
                }
            }
            (defined, _) => (defined, reserved),
        };
        match defined {
            // zt t18/df7 (x13 review items 21b, 21c): the steps from the
            // define to the answer run with this thread marked as the unbound
            // define's owner, and a panic among them withdraws the name.
            Ok(class_id) => crate::classloader::hold_unbound_define_across(
                ctx,
                class_id,
                bind_late,
                |ctx: &mut dyn NativeContext| -> MethodCallResult {
                    let mirror_clock = cratonvm_classloading::define_census::phase_start();
                    // MUST run before `get_class_mirror` below: `class_mirror_impl`
                    // consults `defining_loader_for(cid)` at mirror-creation time —
                    // the only time it is ever consulted for this mirror, since the
                    // mirror is cached from then on — to decide whether to record a
                    // `mirror_pin` row. Registering after minting the mirror means
                    // that row is never recorded, and `roots.rs` step 6 falls back to
                    // unconditionally rooting the mirror forever, permanently
                    // defeating unloading for every class this loader defines. See
                    // the identical fix and fuller explanation in
                    // `classloader::define_class_via_full`.
                    // `CRATONVM_DBG_DEFINE_CENSUS` (interpreter round i4 wave 95,
                    // lane L5): `native-mirror` split into `mirror-loader-record`,
                    // `mirror-create` (itself split by `class_mirror_impl` under
                    // the charged scope) and `mirror-native-fields`.
                    let record_clock = cratonvm_classloading::define_census::phase_start();
                    if let Some((p, o)) = loader_pin {
                        let loader_obj = ctx.read_native_pin(p, o);
                        // zt t16/df6: out of the loader's by-name index until the
                        // name is bound (`settle_unbound_define`).
                        if bind_late {
                            crate::classloader::register_defining_loader_unbound(
                                crate::classloader::loader_tables(ctx),
                                class_id.as_u32(),
                                loader_obj,
                            );
                        } else {
                            crate::classloader::register_defining_loader(
                                crate::classloader::loader_tables(ctx),
                                class_id.as_u32(),
                                loader_obj,
                            );
                        }
                    }
                    cratonvm_classloading::define_census::phase_end(
                        cratonvm_classloading::define_census::DefinePhase::MirrorLoaderRecord,
                        record_clock,
                    );
                    let create_clock = cratonvm_classloading::define_census::phase_start();
                    let census_scope = cratonvm_classloading::define_census::enter_charged_scope(
                        create_clock.is_some(),
                    );
                    let mirror = defined_class_mirror_reserved(ctx, class_id, reserved);
                    cratonvm_classloading::define_census::restore_charged_scope(census_scope);
                    cratonvm_classloading::define_census::phase_end(
                        cratonvm_classloading::define_census::DefinePhase::MirrorCreate,
                        create_clock,
                    );
                    let fields_clock = cratonvm_classloading::define_census::phase_start();
                    // The JDK's Class.getClassLoader bytecode reads this instance
                    // field directly. Keep it aligned with the VM's loader registry.
                    if let Some((p, o)) = loader_pin {
                        let loader_obj = ctx.read_native_pin(p, o);
                        ctx.set_field_by_name(
                            mirror,
                            "classLoader",
                            Value::Object(Some(loader_obj)),
                        );
                    }
                    install_protection_domain(ctx, mirror, pd_pin);
                    cratonvm_classloading::define_census::phase_end(
                        cratonvm_classloading::define_census::DefinePhase::MirrorNativeFields,
                        fields_clock,
                    );
                    // zt t7/cl: HotSpot's `define_instance_class` records the class
                    // in its loader's `classes` list (`ClassLoader.addClass`); the
                    // upcall's failure is the define's. zt t9/df: a failed upcall
                    // leaves an orphan a retry of these bytes adopts. zt t16/df6:
                    // with a late binding, the upcall's outcome binds or withdraws
                    // the name first, and a withdrawn class is no orphan.
                    let mirror = match loader_pin {
                        Some((p, o)) => {
                            let loader_obj = ctx.read_native_pin(p, o);
                            // zt t30/dc: `mirror-add-class` / `-bind-settle`
                            // / `-add-tail`, the rest of `native-mirror`.
                            let add_clock = cratonvm_classloading::define_census::phase_start();
                            let added = crate::classloader::add_defined_class_to_loader(
                                ctx, loader_obj, mirror,
                            );
                            cratonvm_classloading::define_census::phase_end(
                                cratonvm_classloading::define_census::DefinePhase::MirrorAddClass,
                                add_clock,
                            );
                            let settle_clock = cratonvm_classloading::define_census::phase_start();
                            let (added, withdrawn) = crate::classloader::settle_unbound_define(
                                ctx, added, class_id, bind_late,
                            );
                            cratonvm_classloading::define_census::phase_end(
                                cratonvm_classloading::define_census::DefinePhase::MirrorBindSettle,
                                settle_clock,
                            );
                            let tail_clock = cratonvm_classloading::define_census::phase_start();
                            if !withdrawn {
                                crate::classloader::note_add_class_orphan_on_failure(
                                    ctx, &added, class_id, &bytes,
                                );
                            }
                            crate::classloader::trace_add_class_door(
                                ctx,
                                "defineClass1",
                                class_id,
                                added.is_ok(),
                            );
                            cratonvm_classloading::define_census::phase_end(
                                cratonvm_classloading::define_census::DefinePhase::MirrorAddTail,
                                tail_clock,
                            );
                            added?
                        }
                        None => mirror,
                    };
                    cratonvm_classloading::define_census::phase_end(
                        cratonvm_classloading::define_census::DefinePhase::NativeMirror,
                        mirror_clock,
                    );
                    Ok(Some(Value::Object(Some(mirror))))
                },
            ),
            Err(msg) => {
                // A refusal naming a user loader by id gets its HotSpot text
                // (i4 wave 84, lane L5; `name_user_loaders_in`).
                let msg = crate::classloader_real::name_user_loaders_in(ctx, &msg);
                if let Some((p, o)) = loader_pin {
                    let loader_obj = ctx.read_native_pin(p, o);
                    match classify_duplicate_define(ctx, loader_obj, &name, loader_id, &msg) {
                        DuplicateDefine::SameLoaderObject => {
                            // zt t9/df: the retry of a define whose `addClass`
                            // upcall failed adopts the class it left behind.
                            let loader_obj = ctx.read_native_pin(p, o);
                            // zt t14/df5: the retry's domain (corner 3).
                            let domain = pd_pin.map(|(dp, d)| ctx.read_native_pin(dp, d));
                            if let Some(adopted) = crate::classloader::adopt_add_class_orphan(
                                ctx,
                                loader_obj,
                                &name,
                                loader_id,
                                &bytes,
                                Some(domain),
                            ) {
                                return adopted.map(|m| Some(Value::Object(Some(m))));
                            }
                            let loader_obj = ctx.read_native_pin(p, o);
                            return Err(duplicate_define_error(ctx, loader_obj, &name));
                        }
                        DuplicateDefine::ServeExisting(mirror) => {
                            return Ok(Some(Value::Object(Some(mirror))));
                        }
                        DuplicateDefine::NotDuplicate => {}
                    }
                    // zt t11/df3: a NAMELESS retry of a define whose `addClass`
                    // upcall failed adopts the class it left behind too (the
                    // classification above needs a caller's name).
                    if name.is_empty() {
                        let loader_obj = ctx.read_native_pin(p, o);
                        let domain = pd_pin.map(|(dp, d)| ctx.read_native_pin(dp, d));
                        if let Some(adopted) = crate::classloader::adopt_nameless_add_class_orphan(
                            ctx,
                            loader_obj,
                            loader_id,
                            &bytes,
                            &msg,
                            Some(domain),
                        ) {
                            return adopted.map(|m| Some(Value::Object(Some(m))));
                        }
                        // zt t15/x12 (x10 page item 6): no orphan to adopt --
                        // HotSpot's duplicate-definition text, parenthetical
                        // included, when this loader object defined the class.
                        if let Some(err) =
                            nameless_duplicate_define_error(ctx, (p, o), &bytes, &msg)
                        {
                            return Err(err);
                        }
                    }
                    // `--jdk-only`, a null name (a named `java.*` define is refused
                    // by `preDefineClass` in Java first): HotSpot's
                    // `ClassFileParser` wording, which names the loader and the
                    // package (wave 42, lane L5; `L5W42DefineClassErrors`).
                    if jdk_only && name.is_empty() && msg.contains("Prohibited package name") {
                        let loader_obj = ctx.read_native_pin(p, o);
                        if let Some(message) = crate::classloader_real::prohibited_package_message(
                            ctx, loader_obj, &bytes,
                        ) {
                            return Err(
                                match crate::jboss_module_loader::alloc_single_message_exception(
                                    ctx,
                                    "java/lang/SecurityException",
                                    1,
                                    &message,
                                ) {
                                    Ok(exc) => MethodCallFailed::ExceptionThrown(exc),
                                    Err(e) => e,
                                },
                            );
                        }
                    }
                }
                if jdk_only {
                    if let Some(err) = jdk_only_truncated_class_file(ctx, &msg) {
                        return Err(err);
                    }
                }
                tracing::warn!("ClassLoader.defineClass1({name}) failed: {msg}");
                // JIT round 21 wave 26, lane nesting: a supertype the loader
                // could not find is caused by its ClassNotFoundException.
                let failed = define_class_linkage_error(&name, "defineClass1", msg);
                Err(supertype_miss_with_cause(ctx, failed, &answers))
            }
        }
    })();
    answers.release_miss_root(ctx);
    if let Some(floor) = pin_floor {
        ctx.unpin_native_roots(floor);
    }
    result
}

/// `ClassLoader.defineClass2(ClassLoader, String, ByteBuffer, int, int, ProtectionDomain, String) -> Class`
pub(crate) fn native_classloader_define_class2(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let entry_clock = cratonvm_classloading::define_census::phase_start();
    let name = match args.get(1) {
        Some(Value::Object(Some(s))) => {
            let n = ctx.read_string(*s).unwrap_or_default();
            n.replace('.', "/")
        }
        _ => String::new(),
    };

    let byte_buffer = match args.get(2) {
        Some(Value::Object(Some(bb))) => *bb,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("defineClass2: ByteBuffer is null".to_string()),
            }
            .into());
        }
    };

    let offset = read_define_class_nonnegative_int(args, 3)?;
    let length = read_define_class_nonnegative_int(args, 4)?;
    let bytes = read_byte_buffer_define_class_slice(ctx, byte_buffer, offset, length, &name)?;
    // zt t16/pl7: `JVM_DefineClassWithSource` counts the define and its
    // bytes before parsing (`sun.cls.defineAppClasses` / `appClassBytes`).
    let counted = ctx.note_jvm_define_class(bytes.len());
    // zt t16/pl7: and times it (`sun.cls.defineAppClassTime` / `.self`;
    // `CRATONVM_PERF_CLASS_TIMERS`, default ON, inert under `=0`) to this
    // native's end, as `jvm_define_class_common`'s `PerfClassTraceTime`.
    let _define_trace = cratonvm_types::perf_class_trace::ClassTraceTime::begin(
        counted.then(|| ctx.class_trace_counters()).flatten(),
        cratonvm_types::perf_class_trace::ClassTraceEvent::DefineClass,
    );
    // `--jdk-only`: HotSpot's header errors (wave 42, lane L5); `--compatible`
    // too under `CRATONVM_COMPAT_MODULE_MESSAGES` (wave 49, lane L6).
    if ctx.messages_name_user_loaders() {
        if let Some(err) = jdk_only_classfile_header_error(ctx, &name, &bytes) {
            return Err(err);
        }
    }
    validate_classfile_header(&name, "defineClass2", &bytes)?;

    let (loader_id, user_loader_define) = define_loader_namespace(ctx, args.first(), &name);
    // zt t25/df11b: the platform loader's define of a loaded image class of its modules.
    if let Some(err) = platform_image_duplicate_refusal(ctx, args.first(), loader_id, &name) {
        return Err(err);
    }

    // The caller's ProtectionDomain, decoded through the ONE reader that
    // understands both PD shapes. Six copies of an inline decode used to
    // stand here, and all six read `CodeSource.location` with
    // `read_string` -- which fails on a real `java.net.URL`, a different
    // concrete class -- so every real-JDK-constructed CodeSource silently
    // lost its URL and the defined class came back carrying the
    // synthesised `file:/runtime-defined/<name>.class` instead of the
    // caller's. See `extract_pd_code_source_url`.
    let pd_url = match args.get(5) {
        Some(Value::Object(Some(pd))) => {
            crate::classloader::extract_pd_code_source_url_for_define(ctx, *pd)
        }
        _ => None,
    };

    // GC: `preload_supertypes_via_loader` LOADS CLASSES, and `define_class_full`
    // / `get_class_mirror` below allocate, so the loader read out of `args` is a
    // pre-call address at every later use. `args` is the snapshot
    // `safe_native_call_impl` took before this native was entered; a collection
    // that happens INSIDE the callback does not rewrite it. Pin once and re-read
    // at each use. Three sibling entry points carry the identical shape — this
    // is one of them. See `internal/audits/wide-tranche-triage-20260907.md`.
    let loader_pin = match args.first() {
        Some(Value::Object(Some(o))) => Some((ctx.pin_native_root(*o), *o)),
        _ => None,
    };
    // gc-common w16-e: the ProtectionDomain is pinned BEFORE the supertype
    // link, which runs the loader's Java `loadClass`. It used to be read out of
    // `args` after that call, so under `--jdk-only` a collection inside
    // `loadClass` left the mirror's `protectionDomain` a pre-move address.
    let pd_pin = pin_protection_domain_arg(ctx, args, 5);
    // One closure, so every arm releases the pins (see `defineClass1`).
    let pin_floor = loader_pin.map(|(p, _)| p).or(pd_pin.map(|(p, _)| p));
    let mut answers = SupertypeAnswers {
        keep_miss_object: true,
        ..SupertypeAnswers::default()
    };
    let result = (|| -> MethodCallResult {
        // `CRATONVM_DEFINE_ONE_PARSE`, as in `defineClass1`.
        let keep_parse = ctx.define_one_parse();
        cratonvm_classloading::define_census::phase_end(
            cratonvm_classloading::define_census::DefinePhase::NativeEntry,
            entry_clock,
        );
        let prepass_clock = cratonvm_classloading::define_census::phase_start();
        // JIT round 20 lane superAcc: the defining loader's `loadClass`
        // answers, linked by identity (see `SupertypeAnswers`). Round 22
        // wave 1 (lane review): declared before the closure, so its miss root
        // is released on every exit below.
        let preparsed = match loader_pin {
            Some((p, o)) => {
                let loader_obj = ctx.read_native_pin(p, o);
                link_supertypes_refusing_circularity(
                    ctx,
                    p,
                    loader_obj,
                    &name,
                    &bytes,
                    keep_parse,
                    &mut answers,
                )?
            }
            None => None,
        };
        let (superclass_id_override, interface_id_overrides) = answers.overrides();
        cratonvm_classloading::define_census::phase_end(
            cratonvm_classloading::define_census::DefinePhase::SupertypePrepass,
            prepass_clock,
        );

        // zt t16/df6: the late name binding and its wait, as in `defineClass1`.
        let bind_late = loader_pin.is_some()
            && crate::classloader::define_bind_after_add_class_enabled(ctx, loader_id);
        if bind_late {
            // zt t30/dc: the `bind-wait` census row.
            let wait_clock = cratonvm_classloading::define_census::phase_start();
            crate::classloader::wait_for_unbound_define(ctx, &name, &bytes, loader_id)?;
            cratonvm_classloading::define_census::phase_end(
                cratonvm_classloading::define_census::DefinePhase::BindWait,
                wait_clock,
            );
        }
        // zt t7/cl: the `classes` presize, as in `defineClass1`.
        let loader_now = loader_pin.map(|(p, o)| ctx.read_native_pin(p, o));
        crate::classloader::presize_for_add_class(ctx, loader_now, &bytes, &name)?;
        // zs s7/nd6: the mirror object first, as in `defineClass1` (zt t9/df:
        // malformed bytes keep their `ClassFormatError`, as there).
        let reservation = reserve_defined_class_mirror(ctx, &define_name_hint(&name, &bytes));
        let reservation =
            crate::classloader::refusal_yields_to_format_error(ctx, reservation, &bytes, &name)?;
        let opts = cratonvm_native_api::DefineClassFull {
            code_source_url: pd_url,
            // The load-time transformers' `protectionDomain` (both modes since
            // interpreter round i1 wave 47).
            protection_domain: pd_pin.map(|(p, o)| ctx.read_native_pin(p, o)),
            preparsed: preparsed_for_define(preparsed),
            superclass_id_override,
            interface_id_overrides,
            user_loader_define,
            defining_loader: loader_pin.map(|(p, o)| ctx.read_native_pin(p, o)),
            bind_name_after_add_class: bind_late,
            ..Default::default()
        };
        // zt t18/df7 (x13 item 21d): kept for one define after a withdrawn rival,
        // without the pre-pass parse (a second holder of its `Arc` would make
        // the define below parse the bytes again).
        // zt t30/dc: the `bind-retry-clone` census row (bind arm only).
        let clone_clock = cratonvm_classloading::define_census::phase_start().filter(|_| bind_late);
        let retry_opts = bind_late.then(|| cratonvm_native_api::DefineClassFull {
            preparsed: None,
            ..opts.clone()
        });
        cratonvm_classloading::define_census::phase_end(
            cratonvm_classloading::define_census::DefinePhase::BindRetryClone,
            clone_clock,
        );
        let defined = ctx.define_class_full(&name, &bytes, loader_id, opts);
        let reserved = take_mirror_reservation(ctx, reservation);
        // zt t18/df7 (x13 review item 21d): a define that raced past the wait
        // above can meet another define of this name that is still unbound in
        // this loader (the class manager's duplicate check counts it). Wait for
        // it to settle and, when it was withdrawn (its upcall threw), define
        // once more, as HotSpot's second definer waits on the placeholder and
        // then defines its own bytes. Only pinned references are live here.
        let (defined, reserved) = match (defined, retry_opts) {
            (Err(msg), Some(mut again)) => {
                let duplicate =
                    msg.contains("already defined") || msg.contains("DuplicateClassDefinition");
                if crate::classloader::redefine_after_unbound_define_settles(
                    ctx, &name, &bytes, loader_id, duplicate,
                )? {
                    again.protection_domain = pd_pin.map(|(p, o)| ctx.read_native_pin(p, o));
                    let reservation =
                        reserve_defined_class_mirror(ctx, &define_name_hint(&name, &bytes));
                    let reservation = crate::classloader::refusal_yields_to_format_error(
                        ctx,
                        reservation,
                        &bytes,
                        &name,
                    )?;
                    let defined = ctx.define_class_full(&name, &bytes, loader_id, again);
                    (defined, take_mirror_reservation(ctx, reservation))
                } else {
                    (Err(msg), reserved)
                }
            }
            (defined, _) => (defined, reserved),
        };
        match defined {
            // zt t18/df7 (x13 review items 21b, 21c): the steps from the
            // define to the answer run with this thread marked as the unbound
            // define's owner, and a panic among them withdraws the name.
            Ok(class_id) => crate::classloader::hold_unbound_define_across(
                ctx,
                class_id,
                bind_late,
                |ctx: &mut dyn NativeContext| -> MethodCallResult {
                    let mirror_clock = cratonvm_classloading::define_census::phase_start();
                    // MUST run before `get_class_mirror` below — see the identical
                    // fix and fuller explanation in `classloader::define_class_via_full`
                    // and the `defineClass1` sibling above.
                    if let Some((p, o)) = loader_pin {
                        let loader_obj = ctx.read_native_pin(p, o);
                        if bind_late {
                            crate::classloader::register_defining_loader_unbound(
                                crate::classloader::loader_tables(ctx),
                                class_id.as_u32(),
                                loader_obj,
                            );
                        } else {
                            crate::classloader::register_defining_loader(
                                crate::classloader::loader_tables(ctx),
                                class_id.as_u32(),
                                loader_obj,
                            );
                        }
                    }
                    let create_clock = cratonvm_classloading::define_census::phase_start();
                    // zt t1/nd: the charged scope `defineClass1` opens, so
                    // `class_mirror_impl` splits this define's `mirror-create`
                    // too (it was attributed whole under `defineClass2`).
                    let census_scope = cratonvm_classloading::define_census::enter_charged_scope(
                        create_clock.is_some(),
                    );
                    let mirror = defined_class_mirror_reserved(ctx, class_id, reserved);
                    cratonvm_classloading::define_census::restore_charged_scope(census_scope);
                    cratonvm_classloading::define_census::phase_end(
                        cratonvm_classloading::define_census::DefinePhase::MirrorCreate,
                        create_clock,
                    );
                    if let Some((p, o)) = loader_pin {
                        let loader_obj = ctx.read_native_pin(p, o);
                        ctx.set_field_by_name(
                            mirror,
                            "classLoader",
                            Value::Object(Some(loader_obj)),
                        );
                    }
                    install_protection_domain(ctx, mirror, pd_pin);
                    // zt t7/cl: `ClassLoader.addClass`, as in `defineClass1` (zt
                    // t9/df: a failed upcall leaves an orphan, as there).
                    let mirror = match loader_pin {
                        Some((p, o)) => {
                            let loader_obj = ctx.read_native_pin(p, o);
                            // zt t30/dc: `mirror-add-class` / `-bind-settle`
                            // / `-add-tail`, the rest of `native-mirror`.
                            let add_clock = cratonvm_classloading::define_census::phase_start();
                            let added = crate::classloader::add_defined_class_to_loader(
                                ctx, loader_obj, mirror,
                            );
                            cratonvm_classloading::define_census::phase_end(
                                cratonvm_classloading::define_census::DefinePhase::MirrorAddClass,
                                add_clock,
                            );
                            let settle_clock = cratonvm_classloading::define_census::phase_start();
                            let (added, withdrawn) = crate::classloader::settle_unbound_define(
                                ctx, added, class_id, bind_late,
                            );
                            cratonvm_classloading::define_census::phase_end(
                                cratonvm_classloading::define_census::DefinePhase::MirrorBindSettle,
                                settle_clock,
                            );
                            let tail_clock = cratonvm_classloading::define_census::phase_start();
                            if !withdrawn {
                                crate::classloader::note_add_class_orphan_on_failure(
                                    ctx, &added, class_id, &bytes,
                                );
                            }
                            crate::classloader::trace_add_class_door(
                                ctx,
                                "defineClass2",
                                class_id,
                                added.is_ok(),
                            );
                            cratonvm_classloading::define_census::phase_end(
                                cratonvm_classloading::define_census::DefinePhase::MirrorAddTail,
                                tail_clock,
                            );
                            added?
                        }
                        None => mirror,
                    };
                    cratonvm_classloading::define_census::phase_end(
                        cratonvm_classloading::define_census::DefinePhase::NativeMirror,
                        mirror_clock,
                    );
                    Ok(Some(Value::Object(Some(mirror))))
                },
            ),
            Err(msg) => {
                // As in `defineClass1` (i4 wave 84, lane L5).
                let msg = crate::classloader_real::name_user_loaders_in(ctx, &msg);
                if let Some((p, o)) = loader_pin {
                    let loader_obj = ctx.read_native_pin(p, o);
                    match classify_duplicate_define(ctx, loader_obj, &name, loader_id, &msg) {
                        DuplicateDefine::SameLoaderObject => {
                            // zt t9/df: as in `defineClass1`.
                            let loader_obj = ctx.read_native_pin(p, o);
                            // zt t14/df5: the retry's domain (corner 3).
                            let domain = pd_pin.map(|(dp, d)| ctx.read_native_pin(dp, d));
                            if let Some(adopted) = crate::classloader::adopt_add_class_orphan(
                                ctx,
                                loader_obj,
                                &name,
                                loader_id,
                                &bytes,
                                Some(domain),
                            ) {
                                return adopted.map(|m| Some(Value::Object(Some(m))));
                            }
                            let loader_obj = ctx.read_native_pin(p, o);
                            return Err(duplicate_define_error(ctx, loader_obj, &name));
                        }
                        DuplicateDefine::ServeExisting(mirror) => {
                            return Ok(Some(Value::Object(Some(mirror))));
                        }
                        DuplicateDefine::NotDuplicate => {}
                    }
                    // zt t11/df3: as in `defineClass1` (a nameless retry adopts).
                    if name.is_empty() {
                        let loader_obj = ctx.read_native_pin(p, o);
                        let domain = pd_pin.map(|(dp, d)| ctx.read_native_pin(dp, d));
                        if let Some(adopted) = crate::classloader::adopt_nameless_add_class_orphan(
                            ctx,
                            loader_obj,
                            loader_id,
                            &bytes,
                            &msg,
                            Some(domain),
                        ) {
                            return adopted.map(|m| Some(Value::Object(Some(m))));
                        }
                        // zt t15/x12 (x10 page item 6): no orphan to adopt --
                        // HotSpot's duplicate-definition text, parenthetical
                        // included, when this loader object defined the class.
                        if let Some(err) =
                            nameless_duplicate_define_error(ctx, (p, o), &bytes, &msg)
                        {
                            return Err(err);
                        }
                    }
                    // A DIRECT buffer with a null name reaches this native
                    // (`ClassLoader.defineClass(String, ByteBuffer, PD)` sends
                    // a heap buffer to `defineClass1`): the same HotSpot
                    // `ClassFileParser` wording as `defineClass1`'s arm, which
                    // this native lacked (interpreter round i5 wave 106, lane
                    // L5; probe `L5W106DefineDirectBufferProhibited`).
                    if ctx.messages_name_user_loaders()
                        && name.is_empty()
                        && msg.contains("Prohibited package name")
                    {
                        let loader_obj = ctx.read_native_pin(p, o);
                        if let Some(message) = crate::classloader_real::prohibited_package_message(
                            ctx, loader_obj, &bytes,
                        ) {
                            return Err(
                                match crate::jboss_module_loader::alloc_single_message_exception(
                                    ctx,
                                    "java/lang/SecurityException",
                                    1,
                                    &message,
                                ) {
                                    Ok(exc) => MethodCallFailed::ExceptionThrown(exc),
                                    Err(e) => e,
                                },
                            );
                        }
                    }
                }
                if ctx.messages_name_user_loaders() {
                    if let Some(err) = jdk_only_truncated_class_file(ctx, &msg) {
                        return Err(err);
                    }
                }
                tracing::warn!("ClassLoader.defineClass2({name}) failed: {msg}");
                // JIT round 21 wave 26, lane nesting: a supertype the loader
                // could not find is caused by its ClassNotFoundException.
                let failed = define_class_linkage_error(&name, "defineClass2", msg);
                Err(supertype_miss_with_cause(ctx, failed, &answers))
            }
        }
    })();
    answers.release_miss_root(ctx);
    if let Some(floor) = pin_floor {
        ctx.unpin_native_roots(floor);
    }
    result
}

/// `ClassLoader.defineClass0(ClassLoader, Class, String, byte[], int, int, ProtectionDomain, boolean, int, Object) в†’ Class`
///
/// JDK 21+ variant of defineClass with additional flags. WP2.3:
/// shares the same backend via `define_class_full`. The `flags`
/// argument is decoded with the JDK's `MethodHandleNatives.Constants`
/// values (0x1 = NESTMATE, 0x2 = HIDDEN, 0x4 = STRONG_LOADER_LINK: a hidden
/// class without it unloads with its mirror, gc-common w18-d — see
/// `docs/feature-designs/class-resolution-and-caching.md`)
/// and translated into options.
pub(crate) fn native_classloader_define_class0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: loader(0), lookup(1), name(2), bytes(3), offset(4),
    //       length(5), pd(6), init(7), flags(8), classData(9)
    let entry_clock = cratonvm_classloading::define_census::phase_start();
    let name = match args.get(2) {
        Some(Value::Object(Some(s))) => {
            let n = ctx.read_string(*s).unwrap_or_default();
            n.replace('.', "/")
        }
        _ => String::new(),
    };

    let byte_array = match args.get(3) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("defineClass0: byte[] is null".to_string()),
            }
            .into());
        }
    };

    let offset = read_define_class_nonnegative_int(args, 4)?;
    let length = read_define_class_nonnegative_int(args, 5)?;
    let bytes = read_byte_array_define_class_slice(ctx, byte_array, offset, length)?;
    // HotSpot's `JVM_LookupDefineClass` parses in `ClassFileParser`: its
    // header text, with `defineClass1`'s gate (interpreter round i4 wave 95,
    // lane L5; `i42-L5-proposal-one-classfileparser-shaped-error-for-every-define-door`).
    if ctx.messages_name_user_loaders() {
        if let Some(err) = jdk_only_classfile_header_error(ctx, &name, &bytes) {
            return Err(err);
        }
    }
    validate_classfile_header(&name, "defineClass0", &bytes)?;

    let (loader_id, user_loader_define) = define_loader_namespace(ctx, args.first(), &name);
    // zt t25/df11b: the platform loader's define of a loaded image class of its modules.
    if let Some(err) = platform_image_duplicate_refusal(ctx, args.first(), loader_id, &name) {
        return Err(err);
    }

    // The caller's ProtectionDomain, decoded through the ONE reader that
    // understands both PD shapes. Six copies of an inline decode used to
    // stand here, and all six read `CodeSource.location` with
    // `read_string` -- which fails on a real `java.net.URL`, a different
    // concrete class -- so every real-JDK-constructed CodeSource silently
    // lost its URL and the defined class came back carrying the
    // synthesised `file:/runtime-defined/<name>.class` instead of the
    // caller's. See `extract_pd_code_source_url`.
    let pd_url = match args.get(6) {
        Some(Value::Object(Some(pd))) => {
            crate::classloader::extract_pd_code_source_url_for_define(ctx, *pd)
        }
        _ => None,
    };

    // `init` (boolean) at arg 7: run <clinit> after define.
    let initialize = matches!(args.get(7), Some(Value::Int(v)) if *v != 0);
    let init_after_class_data = initialize && matches!(args.get(9), Some(Value::Object(Some(_))));
    // `flags` (int) at arg 8. JDK 25 `MethodHandleNatives.Constants`:
    //   NESTMATE_CLASS = 0x01, HIDDEN_CLASS = 0x02, STRONG_LOADER_LINK = 0x04,
    //   ACCESS_VM_ANNOTATIONS = 0x08.
    // This block previously read bit 0 as HIDDEN (that is NESTMATE) and bit 2
    // as NESTMATE (that is STRONG), so a non-nestmate `defineHiddenClass`
    // (flags 0x02) decoded as `hidden = false` and collided on a duplicate
    // define. `classloader.rs:4048-4057` has carried the correct constants all
    // along — see DEFINE_CLASS0_FLAG_* there.
    let flags = match args.get(8) {
        Some(Value::Int(f)) => *f,
        _ => 0,
    };
    let nestmate = (flags & 0x1) != 0;
    let hidden = (flags & 0x2) != 0;
    // gc-common w18-d: STRONG_LOADER_LINK clear on a hidden class = it unloads
    // with its mirror (registered below, once the native is done with it).
    let strong_loader_link = (flags & 0x4) != 0;
    // zt t21/df9: a HIDDEN class of the platform loader (the lambdas and
    // species of the platform modules' classes) keeps the application
    // namespace it had before `define_loader_namespace` gave the platform
    // loader its own: its name is unique, so no define can collide on it.
    let loader_id = if hidden
        && !user_loader_define
        && loader_id == cratonvm_types::ClassLoaderId::NATIVE_EXTENSION
    {
        0
    } else {
        loader_id
    };
    // zt t9/df (`zgc-zt7cl-define-path-and-class-name-residuals-SUPERSEDED-20261019.md`
    // item 4): a non-hidden `initialize` define into a loader object runs
    // `<clinit>` after the `addClass` upcall below, as HotSpot's
    // `JVM_LookupDefineClass` initializes only after `define_instance_class`
    // returned. It ran inside the define, before the upcall, unless class
    // data was attached. `CRATONVM_ZGC_DEFINE_INIT_AFTER_ADD_CLASS=0` restores
    // that order.
    let init_after_define = init_after_class_data
        || (initialize
            && !hidden
            && matches!(args.first(), Some(Value::Object(Some(_))))
            && crate::classloader::define_init_after_add_class_enabled(ctx));

    // If hidden, mangle the name uniquely.
    let (effective_name, override_name) = if hidden {
        let id = crate::classloader::HIDDEN_CLASS_COUNTER
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mangled = format!("{name}/0x{id:016x}");
        (mangled.clone(), Some(mangled))
    } else {
        (name.clone(), None)
    };

    // Resolve nest-host name when NESTMATE is set: the lookup class
    // at arg 1 supplies the nest — its OWN nest host when it is itself a
    // nested class (JEP 371; HotSpot's `JVM_LookupDefineClass` takes
    // `lookup_class->nest_host()`), as `classloader.rs::cl_define_class0`
    // already did. It used the lookup class's name, so a NESTMATE hidden class
    // defined through a Lookup on `Outer$Inner` reported `Outer$Inner` from
    // `getNestHost()` (interpreter round i1 wave 27, lane L5; probe
    // `tools/probes/interp/L5/L5W27NestmateEdges.java`).
    let nest_host_class_name = if nestmate {
        match args.get(1) {
            Some(Value::Object(Some(lookup_mirror))) => {
                crate::lang_class::mirror_class_id(ctx, *lookup_mirror).and_then(|cid| {
                    ctx.nest_host_name(cid)
                        .or_else(|| ctx.class_name_of_id(cid))
                })
            }
            _ => None,
        }
    } else {
        None
    };

    // `--jdk-only`: `Lookup.defineClass` on a lookup class of the bootstrap or
    // platform loader reaches here with that loader (`null`, or the platform
    // loader object). HotSpot's `JVM_LookupDefineClass` defines the class in
    // the lookup class's loader, where a `java.*` name is legal, and the JDK has
    // already confined the name to the lookup class's package
    // (`Lookup.makeClassDefiner`) and required PACKAGE access to it. CratonVM
    // maps both loaders to `Application` here (the `0` sentinel), whose H5
    // guard refused every such define as a "prohibited package": JaCoCo's
    // agent defines `java.lang.$JaCoCo` through a private lookup on `Object`
    // once `Instrumentation.redefineModule` has opened `java.lang` to it
    // (interpreter round i1 wave 29, lane L5; probe
    // `L5/L5W29ChildFirstAgentLoader`, `inject` row). The privileged define is the
    // `Unsafe.defineClass` exemption, and keeps its guard against forging or
    // shadowing a class the bootstrap loader already has. Hidden classes were
    // exempt already. Both modes since wave 47 (lane L6): `--compatible`'s own
    // `Lookup.defineClass` door (`lookup_define.rs` `lk_define_class_b`) makes
    // the same bootstrap define, and this door agrees with it.
    let bootstrap_lookup_define = !hidden
        && match args.first() {
            Some(Value::Object(None)) => true,
            Some(Value::Object(Some(loader_obj))) => {
                crate::classloader::is_platform_loader_object(ctx, *loader_obj)
            }
            _ => false,
        };
    if bootstrap_lookup_define && cratonvm_types::flags().loader.dbg_define {
        eprintln!("[DEFINE-DBG] defineClass0 privileged {name} (bootstrap/platform lookup)");
    }
    // gci i11/cl (`gch-h4fl-xlog-class-load-residuals` item 7): a NESTMATE
    // define's lookup class itself, the `-Xlog:class+load` source HotSpot
    // prints (`nest_host_class_name` is its nest host).
    let lookup_class_name = if nest_host_class_name.is_some() {
        match args.get(1) {
            Some(Value::Object(Some(lookup_mirror))) => {
                crate::lang_class::mirror_class_id(ctx, *lookup_mirror)
                    .and_then(|cid| ctx.class_name_of_id(cid))
            }
            _ => None,
        }
    } else {
        None
    };
    let mut opts = cratonvm_native_api::DefineClassFull {
        override_name,
        hidden,
        code_source_url: pd_url,
        lookup_class_name,
        // With class data, `<clinit>` runs AFTER it is attached (below): a
        // spun `LambdaForm$MH` / lambda class's `<clinit>` loads its constants
        // with `MethodHandles.classData`, which read null when the define ran
        // the initializer first (HotSpot stores the data before it initialises).
        initialize: initialize && !init_after_define,
        nest_host_class_name,
        non_strong_hidden: !strong_loader_link,
        privileged_define: bootstrap_lookup_define,
        // The bootstrap loader's lookup (a null loader) defines in the
        // bootstrap namespace, as `JVM_LookupDefineClass` does: before wave 37
        // the `0` sentinel put it in the application loader's, where
        // `Class.forName(name, false, null)` could not find it (interpreter
        // round i1 wave 37, lane L5; probe `L5/L5W37BootstrapLookupDefine`).
        // The platform loader's lookup keeps the application namespace (its
        // mirror names the platform loader already).
        bootstrap_namespace: bootstrap_lookup_define
            && matches!(args.first(), Some(Value::Object(None))),
        user_loader_define,
        ..Default::default()
    };
    // `CRATONVM_LOOKUP_DEFINE_JDK_BODY` (default off; interpreter round i6
    // wave 120, lane L5, `i44-L5-proposal-lookup-define-runs-the-jdks-own-checks`
    // step 2): with the `Lookup` define natives unregistered, every
    // `Lookup.defineClass` / `defineHiddenClass` reaches this native through
    // the JDK's `ClassDefiner`, so it carries what `lk_define_class_b` did.
    // The supertypes are already resolved through the defining loader below
    // (the JDK passes `lookupClass.getClassLoader()`; JIT round 20 lane
    // superAcc, by identity); what was missing is loader-faithful linking, so
    // a generated proxy links against the exact interface its generator named.
    if cratonvm_types::flags().loader.lookup_define_jdk_body
        && matches!(args.get(1), Some(Value::Object(Some(_))))
    {
        opts.force_loader_faithful_linking = true;
    }
    // A HIDDEN class of a bootstrap-tagged lookup class goes to the bootstrap
    // namespace too, in both modes: its lookup class's loader and runtime
    // package, as HotSpot defines it (interpreter round i1 wave 44, lane L5;
    // `classloader::hidden_define_in_bootstrap_namespace`).
    if hidden {
        let lookup_mirror = match args.get(1) {
            Some(Value::Object(Some(o))) => Some(*o),
            _ => None,
        };
        let loader = args.first().copied().unwrap_or(Value::Object(None));
        if crate::classloader::hidden_define_in_bootstrap_namespace(ctx, lookup_mirror, loader) {
            opts.bootstrap_namespace = true;
            opts.privileged_define = true;
        }
    }
    let non_strong_hidden = opts.unloads_with_its_mirror();
    // GC: `preload_supertypes_via_loader` LOADS CLASSES, and `define_class_full`
    // / `get_class_mirror` below allocate, so the loader read out of `args` is a
    // pre-call address at every later use. `args` is the snapshot
    // `safe_native_call_impl` took before this native was entered; a collection
    // that happens INSIDE the callback does not rewrite it. Pin once and re-read
    // at each use. Three sibling entry points carry the identical shape — this
    // is one of them. See `internal/audits/wide-tranche-triage-20260907.md`.
    let loader_pin = match args.first() {
        Some(Value::Object(Some(o))) => Some((ctx.pin_native_root(*o), *o)),
        _ => None,
    };
    let pd_pin = pin_protection_domain_arg(ctx, args, 6);
    // `classData` (arg 9), pinned for the same reason. gc-common w10-f: this
    // backend used to drop it outright, so `Class.getClassData()` (the field
    // `MethodHandles.classData` reads) was null for every hidden class
    // defined through here.
    let class_data_pin = match args.get(9) {
        Some(Value::Object(Some(o))) => Some((ctx.pin_native_root(*o), *o)),
        _ => None,
    };
    // The domain `Lookup.ClassDefiner` passed (the lookup class's,
    // `null` for a bootstrap lookup class) is handed to the load-time
    // transformers, as `defineClass1` hands its own; it was pinned and
    // installed in the mirror but not passed (interpreter round i1 wave 29, lane
    // L5; `interpreter-L6-transformers-are-handed-a-null-protection-domain-FIXED-20261011` item 2).
    // A hidden class is never offered to them. zs s8/nd7: read inside the
    // closure below, after the supertype pre-pass (it loads classes: a GC
    // point) and the mirror reservation, not here.
    // One closure, so every arm releases the three pins above before the
    // native returns (interpreter round i2 wave 62, lane L4; the
    // `[pin-balance] LEAK` census named this native under `Lookup.bind`).
    let pin_floor = [loader_pin, pd_pin, class_data_pin]
        .into_iter()
        .flatten()
        .map(|(p, _)| p)
        .min();
    cratonvm_classloading::define_census::phase_end(
        cratonvm_classloading::define_census::DefinePhase::NativeEntry,
        entry_clock,
    );
    let result = (|| -> MethodCallResult {
        // JIT round 20 lane superAcc (correctness, both modes): a user loader's
        // `Lookup.defineClass` / `defineHiddenClass` resolves the class's direct
        // supertypes through that loader, as `defineClass1` does and HotSpot's
        // `resolve_super_or_fail` does for every define: without the pre-pass a
        // supertype the loader had not loaded yet (a package-private base its
        // `findClass` serves) was looked up by name in the flat store and the
        // define failed with `NoClassDefFoundError` (probe `R20SuperAccModule`,
        // `hidden-pkgpriv`). The answers are linked by identity
        // (`SupertypeAnswers`); a built-in loader returns at once.
        let mut opts = opts;
        if let Some((p, o)) = loader_pin {
            let loader_obj = ctx.read_native_pin(p, o);
            let mut answers = SupertypeAnswers::default();
            link_supertypes_refusing_circularity(
                ctx,
                p,
                loader_obj,
                &name,
                &bytes,
                false,
                &mut answers,
            )?;
            let (superclass_id_override, interface_id_overrides) = answers.overrides();
            if superclass_id_override.is_some() {
                opts.superclass_id_override = superclass_id_override;
            }
            if interface_id_overrides.is_some() {
                opts.interface_id_overrides = interface_id_overrides;
            }
        }
        // zt t16/df6: the late name binding and its wait, as in
        // `defineClass1`, for a non-hidden class (the class manager also
        // binds at once when this define initializes).
        let bind_late = !hidden
            && loader_pin.is_some()
            && crate::classloader::define_bind_after_add_class_enabled(ctx, loader_id);
        if bind_late {
            // zt t30/dc: the `bind-wait` census row.
            let wait_clock = cratonvm_classloading::define_census::phase_start();
            crate::classloader::wait_for_unbound_define(ctx, &effective_name, &bytes, loader_id)?;
            cratonvm_classloading::define_census::phase_end(
                cratonvm_classloading::define_census::DefinePhase::BindWait,
                wait_clock,
            );
            opts.bind_name_after_add_class = true;
        }
        // zt t7/cl: a non-hidden class's `classes` presize, as in
        // `defineClass1` (a hidden class is never added to the list). It runs
        // Java (a GC point), so it comes before the domain is read below.
        if !hidden {
            let loader_now = loader_pin.map(|(p, o)| ctx.read_native_pin(p, o));
            crate::classloader::presize_for_add_class(ctx, loader_now, &bytes, "")?;
        }
        // The domain through its pin, current after the pre-pass above.
        if !hidden {
            opts.protection_domain = pd_pin.map(|(p, o)| ctx.read_native_pin(p, o));
        }
        // zs s8/nd7: the mirror object first, as `defineClass1` does, so a heap
        // that cannot hold it fails this define before anything is published
        // (every reference used below is pinned; the domain in `opts` is
        // re-read across the reservation). A hidden class pools no name.
        // zt t9/df: malformed bytes keep their `ClassFormatError`.
        let reservation = reserve_mirror_for_define(ctx, &effective_name, &mut opts);
        let reservation =
            crate::classloader::refusal_yields_to_format_error(ctx, reservation, &bytes, "")?;
        // zt t18/df7 (x13 item 21d): kept for one define after a withdrawn rival,
        // without the pre-pass parse (a second holder of its `Arc` would make
        // the define below parse the bytes again).
        // zt t30/dc: the `bind-retry-clone` census row (bind arm only).
        let clone_clock = cratonvm_classloading::define_census::phase_start().filter(|_| bind_late);
        let retry_opts = bind_late.then(|| cratonvm_native_api::DefineClassFull {
            preparsed: None,
            ..opts.clone()
        });
        cratonvm_classloading::define_census::phase_end(
            cratonvm_classloading::define_census::DefinePhase::BindRetryClone,
            clone_clock,
        );
        let defined = ctx.define_class_full(&effective_name, &bytes, loader_id, opts);
        let reserved = take_mirror_reservation(ctx, reservation);
        // zt t18/df7 (x13 review item 21d): a define that raced past the wait
        // above can meet another define of this name that is still unbound in
        // this loader (the class manager's duplicate check counts it). Wait for
        // it to settle and, when it was withdrawn (its upcall threw), define
        // once more, as HotSpot's second definer waits on the placeholder and
        // then defines its own bytes. Only pinned references are live here.
        let (defined, reserved) = match (defined, retry_opts) {
            (Err(msg), Some(mut again)) => {
                let duplicate =
                    msg.contains("already defined") || msg.contains("DuplicateClassDefinition");
                if crate::classloader::redefine_after_unbound_define_settles(
                    ctx,
                    &effective_name,
                    &bytes,
                    loader_id,
                    duplicate,
                )? {
                    again.protection_domain = pd_pin.map(|(p, o)| ctx.read_native_pin(p, o));
                    let reservation = reserve_mirror_for_define(ctx, &effective_name, &mut again);
                    let reservation = crate::classloader::refusal_yields_to_format_error(
                        ctx,
                        reservation,
                        &bytes,
                        "",
                    )?;
                    let defined = ctx.define_class_full(&effective_name, &bytes, loader_id, again);
                    (defined, take_mirror_reservation(ctx, reservation))
                } else {
                    (Err(msg), reserved)
                }
            }
            (defined, _) => (defined, reserved),
        };
        match defined {
            // zt t18/df7 (x13 review items 21b, 21c): the steps from the
            // define to the answer run with this thread marked as the unbound
            // define's owner, and a panic among them withdraws the name.
            Ok(class_id) => crate::classloader::hold_unbound_define_across(
                ctx,
                class_id,
                bind_late,
                |ctx: &mut dyn NativeContext| -> MethodCallResult {
                    let mirror_clock = cratonvm_classloading::define_census::phase_start();
                    // MUST run before `get_class_mirror` below — see the identical
                    // fix and fuller explanation in `classloader::define_class_via_full`
                    // and the `defineClass1`/`defineClass2` siblings above.
                    if let Some((p, o)) = loader_pin {
                        let loader_obj = ctx.read_native_pin(p, o);
                        if bind_late {
                            crate::classloader::register_defining_loader_unbound(
                                crate::classloader::loader_tables(ctx),
                                class_id.as_u32(),
                                loader_obj,
                            );
                        } else {
                            crate::classloader::register_defining_loader(
                                crate::classloader::loader_tables(ctx),
                                class_id.as_u32(),
                                loader_obj,
                            );
                        }
                    }
                    let create_clock = cratonvm_classloading::define_census::phase_start();
                    // zt t1/nd: `defineClass1`'s charged scope (see `defineClass2`).
                    let census_scope = cratonvm_classloading::define_census::enter_charged_scope(
                        create_clock.is_some(),
                    );
                    let mirror = defined_class_mirror_reserved(ctx, class_id, reserved);
                    cratonvm_classloading::define_census::restore_charged_scope(census_scope);
                    cratonvm_classloading::define_census::phase_end(
                        cratonvm_classloading::define_census::DefinePhase::MirrorCreate,
                        create_clock,
                    );
                    if let Some((p, o)) = loader_pin {
                        let loader_obj = ctx.read_native_pin(p, o);
                        ctx.set_field_by_name(
                            mirror,
                            "classLoader",
                            Value::Object(Some(loader_obj)),
                        );
                    }
                    install_protection_domain(ctx, mirror, pd_pin);
                    if let Some((p, o)) = class_data_pin {
                        let data = ctx.read_native_pin(p, o);
                        crate::classloader::attach_class_data(ctx, mirror, data);
                    }
                    // zt t7/cl: a non-hidden class goes into its loader's `classes`
                    // list before it is initialized (`Lookup.defineClass`:
                    // HotSpot's `JVM_LookupDefineClass` -> `define_instance_class`).
                    // zt t9/df: a failed upcall leaves an orphan, as in
                    // `defineClass1`.
                    let mirror = match loader_pin {
                        Some((p, o)) if !hidden => {
                            let loader_obj = ctx.read_native_pin(p, o);
                            // zt t30/dc: `mirror-add-class` / `-bind-settle`
                            // / `-add-tail`, the rest of `native-mirror`.
                            let add_clock = cratonvm_classloading::define_census::phase_start();
                            let added = crate::classloader::add_defined_class_to_loader(
                                ctx, loader_obj, mirror,
                            );
                            cratonvm_classloading::define_census::phase_end(
                                cratonvm_classloading::define_census::DefinePhase::MirrorAddClass,
                                add_clock,
                            );
                            let settle_clock = cratonvm_classloading::define_census::phase_start();
                            let (added, withdrawn) = crate::classloader::settle_unbound_define(
                                ctx, added, class_id, bind_late,
                            );
                            cratonvm_classloading::define_census::phase_end(
                                cratonvm_classloading::define_census::DefinePhase::MirrorBindSettle,
                                settle_clock,
                            );
                            let tail_clock = cratonvm_classloading::define_census::phase_start();
                            if !withdrawn {
                                crate::classloader::note_add_class_orphan_on_failure(
                                    ctx, &added, class_id, &bytes,
                                );
                            }
                            crate::classloader::trace_add_class_door(
                                ctx,
                                "defineClass0",
                                class_id,
                                added.is_ok(),
                            );
                            cratonvm_classloading::define_census::phase_end(
                                cratonvm_classloading::define_census::DefinePhase::MirrorAddTail,
                                tail_clock,
                            );
                            added?
                        }
                        _ => mirror,
                    };
                    // The deferred `<clinit>` (see `init_after_class_data` and, zt
                    // t9/df, `init_after_define`). A failure is the JVMS 5.5
                    // `ExceptionInInitializerError` the VM already built. The mirror
                    // is re-read: the initializer can move it.
                    let mirror = if init_after_define {
                        ctx.initialize_class(class_id)?;
                        ctx.get_class_mirror(class_id)
                    } else {
                        mirror
                    };
                    // gc-common w18-d: last, with nothing left to allocate; the mirror
                    // is this native's return value. The defining loader was recorded
                    // and written into `classLoader` above, so the mirror carries the
                    // edge to it.
                    if non_strong_hidden {
                        crate::classloader::register_non_strong_hidden_class(ctx, class_id);
                    }
                    cratonvm_classloading::define_census::phase_end(
                        cratonvm_classloading::define_census::DefinePhase::NativeMirror,
                        mirror_clock,
                    );
                    Ok(Some(Value::Object(Some(mirror))))
                },
            ),
            Err(msg) => {
                // `hidden` is excluded on purpose and always was: a hidden class
                // is never registered under its name, so "already defined" cannot
                // be about it, and JVMS §5.3.5's rule does not apply to a class
                // that has no binary name in any loader's namespace.
                //
                // gc-common w9-a: read the loader through the pin, as the
                // `defineClass1` / `defineClass2` siblings do. This arm used the
                // `args` snapshot, which is a pre-call address once
                // `define_class_full` (GC-capable) has run: after a moving
                // collection the duplicate check compared the defining-loader
                // record against a stale address (so the loader's own re-define
                // could pass as `NotDuplicate`), and `duplicate_define_error`
                // read the loader's name off a vacated slot.
                if !hidden {
                    if let Some((p, o)) = loader_pin {
                        let loader_obj = ctx.read_native_pin(p, o);
                        match classify_duplicate_define(
                            ctx,
                            loader_obj,
                            &effective_name,
                            loader_id,
                            &msg,
                        ) {
                            DuplicateDefine::SameLoaderObject => {
                                // zt t9/df: as in `defineClass1`.
                                let loader_obj = ctx.read_native_pin(p, o);
                                // zt t14/df5: the retry's domain (corner 3).
                                let domain = pd_pin.map(|(dp, d)| ctx.read_native_pin(dp, d));
                                if let Some(adopted) = crate::classloader::adopt_add_class_orphan(
                                    ctx,
                                    loader_obj,
                                    &effective_name,
                                    loader_id,
                                    &bytes,
                                    Some(domain),
                                ) {
                                    // zt t10/x7: the adopted class gets the
                                    // deferred `<clinit>` the success arm runs
                                    // after its upcall (`init_after_define`):
                                    // the orphan's own define failed at the
                                    // upcall, before that initialization, and
                                    // HotSpot's retry (`JVM_LookupDefineClass`)
                                    // initializes the class it defines.
                                    let mirror = adopted?;
                                    if !init_after_define {
                                        return Ok(Some(Value::Object(Some(mirror))));
                                    }
                                    let Some(adopted_id) = ctx.class_id_from_mirror(mirror) else {
                                        return Ok(Some(Value::Object(Some(mirror))));
                                    };
                                    ctx.initialize_class(adopted_id)?;
                                    let mirror = ctx.get_class_mirror(adopted_id);
                                    return Ok(Some(Value::Object(Some(mirror))));
                                }
                                let loader_obj = ctx.read_native_pin(p, o);
                                return Err(duplicate_define_error(
                                    ctx,
                                    loader_obj,
                                    &effective_name,
                                ));
                            }
                            DuplicateDefine::ServeExisting(mirror) => {
                                return Ok(Some(Value::Object(Some(mirror))));
                            }
                            DuplicateDefine::NotDuplicate => {}
                        }
                    }
                }
                tracing::warn!("ClassLoader.defineClass0({effective_name}) failed: {msg}");
                // initialize=true failures: see `exception_in_initializer_error`'s
                // doc comment (JVMS 5.5). Checked before `define_class_linkage_error`,
                // which does not recognise this shape and would otherwise wrap it
                // as a `ClassFormatError` — the sibling of the same defect fixed at
                // the two `lookup_define.rs` call sites and the `classloader.rs`
                // `defineHiddenClass` one.
                if msg.contains("initialize after define failed") {
                    return Err(exception_in_initializer_error(ctx, &effective_name, &msg));
                }
                Err(define_class_linkage_error(
                    &effective_name,
                    "defineClass0",
                    msg,
                ))
            }
        }
    })();
    if let Some(floor) = pin_floor {
        ctx.unpin_native_roots(floor);
    }
    result
}

// ---------------------------------------------------------------------------
// jdk.internal.perf.Perf natives (C27)
//
// The JDK's performance-counter infrastructure uses `Perf.getPerf().createLong(...)`
// to register internal counters. We don't track perf counters, so we return
// benign defaults вЂ” empty/zero-filled direct ByteBuffers for createLong /
// createByteArray (so callers can still write into them), zeros / no-ops for
// the rest. Perf counters in the real JDK only drive diagnostic output; no
// program correctness depends on their values. Since zt t9/pl `attach0(0)`
// hands out the VM's own perf-data image (the jvmstat file's view), so the
// internal `sun.management` counters read what `jstat` reads.
// ---------------------------------------------------------------------------

pub(crate) fn native_perf_attach(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // attach(String, int) -> ByteBuffer  вЂ”  return an empty direct buffer.
    ctx.invoke(
        "java/nio/ByteBuffer",
        "allocateDirect",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(0)],
    )
}

pub(crate) fn native_perf_attach0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // private attach0(int) -> ByteBuffer
    //
    // zt t9/pl (`docs/feature-designs/zgc-architecture.md`):
    // HotSpot's `Perf_Attach` answers lvmid 0 (or this process's id) with a
    // direct buffer over the VM's own perf-data memory (`PerfMemory::attach`),
    // which `VMManagementImpl.getInternalCounters` walks for
    // `sun.management.HotspotRuntime` / `HotspotThread` / `HotspotMemory` /
    // `HotspotClassLoading`'s internal counters. A zero-capacity buffer made
    // it set `noPerfData` and answer an empty list for every pattern. The
    // lvmid is the last `int` argument (the receiver, if any, comes first).
    // Without perf data (`-XX:-UsePerfData`, `CRATONVM_PERF_DATA_FILE=0`) or
    // under `CRATONVM_PERF_ATTACH_SELF=0`, the zero-capacity buffer below
    // stays.
    //
    // zt t10/pl2 (`zgc-zt9pl-perf-counter-residuals-SUPERSEDED-20261019.md` item 5):
    // another VM's id maps that VM's published perf data read-only, as
    // HotSpot's `PerfMemory::attach` does for a foreign id (jvmstat's local
    // monitoring: `MonitoredHost.getMonitoredVm`, `VirtualMachine.list()`'s
    // display names). `Perf.attach` wraps it in a cleaner that calls
    // `detach` with this buffer, which unmaps it. No such VM (or
    // `CRATONVM_PERF_ATTACH_FOREIGN=0`): the zero-capacity buffer, as before.
    //
    // zt t14/pl5 (`zgc-zt12pl4-*` item 5, rows `pl4_fa_missing` / `_missingC`):
    // an id with no perf data at all throws HotSpot's
    // `IllegalArgumentException` (`PerfMemory::attach`: `Could not map vmid
    // to user name`) instead (`CRATONVM_PERF_ATTACH_MISSING_THROWS`, default
    // ON, `=0` kill); data that exists but cannot be mapped keeps the
    // zero-capacity buffer.
    //
    // zt pl4 item 5: on POSIX, HotSpot's own lookup and exceptions
    // (`NativeContext::attach_foreign_perf_data_hotspot`; another user's
    // directory, a container's `/proc/<pid>/root/tmp`, an unmappable file).
    let own = perf_attach_targets_this_vm(args);
    let hotspot = if own {
        None
    } else {
        perf_attach_lvmid(args).and_then(|lvmid| ctx.attach_foreign_perf_data_hotspot(lvmid))
    };
    let used_hotspot = hotspot.is_some();
    let view = match hotspot {
        Some(Ok(view)) => view,
        Some(Err(failure)) => {
            use cratonvm_native_api::ForeignPerfAttachFailure as F;
            return Err(match failure {
                F::IllegalArgument(message) => RuntimeError::IllegalArgumentException { message },
                F::Io(message) => RuntimeError::IOException { message },
                F::OutOfMemory(message) => RuntimeError::OutOfMemoryError { message },
            }
            .into());
        }
        None if own => ctx.attach_own_perf_data(),
        None => perf_attach_lvmid(args).and_then(|lvmid| ctx.attach_foreign_perf_data(lvmid)),
    };
    if view.is_none() && !own && !used_hotspot {
        let missing = perf_attach_lvmid(args).and_then(|id| ctx.foreign_perf_data_missing(id));
        if let Some(message) = missing {
            return Err(RuntimeError::IllegalArgumentException {
                message: message.to_string(),
            }
            .into());
        }
    }
    if let Some((address, length)) = view {
        match i32::try_from(length) {
            Ok(capacity) => {
                let made = ctx.new_object_initialized(
                    "java/nio/DirectByteBuffer",
                    "(JILjava/lang/Object;Ljava/lang/foreign/MemorySegment;)V",
                    &[
                        Value::Long(address as i64), // Cast: the view's address
                        Value::Int(capacity),
                        Value::Object(None),
                        Value::Object(None),
                    ],
                );
                // A foreign view no buffer holds is released at once.
                if made.is_err() && !own {
                    ctx.detach_perf_data(address);
                }
                return made;
            }
            Err(_) if !own => ctx.detach_perf_data(address),
            Err(_) => {}
        }
    }
    ctx.invoke(
        "java/nio/ByteBuffer",
        "allocateDirect",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(0)],
    )
}

/// zt t9/pl: whether `Perf.attach0`'s lvmid (the last `int` argument) names
/// this VM: 0, or this process's id, as HotSpot's `PerfMemory::attach`
/// decides.
fn perf_attach_targets_this_vm(args: &[Value]) -> bool {
    let lvmid = perf_attach_lvmid(args);
    lvmid == Some(0) || (lvmid.is_some() && lvmid == i32::try_from(std::process::id()).ok())
}

/// zt t10/pl2: `Perf.attach0`'s lvmid, the last `int` argument.
fn perf_attach_lvmid(args: &[Value]) -> Option<i32> {
    args.iter().rev().find_map(|v| match v {
        Value::Int(i) => Some(*i),
        _ => None,
    })
}

/// zt t10/pl2: `Perf.detach(ByteBuffer)`, the cleaner `Perf.attach` registers
/// for an id other than 0: unmaps a foreign view `attach0` mapped
/// (`NativeSystemAccess::detach_perf_data`, which leaves this VM's own perf
/// data and any other buffer alone, as HotSpot's `PerfMemory::detach` leaves
/// its own memory).
pub(crate) fn native_perf_detach(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let buffer = args.iter().rev().find_map(|v| match v {
        Value::Object(Some(obj)) => Some(*obj),
        _ => None,
    });
    if let Some(buffer) = buffer {
        if let Value::Long(address) = ctx.get_field_by_name(buffer, "address") {
            if address != 0 {
                ctx.detach_perf_data(address as usize); // Cast: the buffer's address
            }
        }
    }
    Ok(None)
}

pub(crate) fn native_perf_create_long(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // createLong(String name, int variability, int units, long value) -> ByteBuffer
    // Return an 8-byte writable direct buffer so the counter slot is usable.
    ctx.invoke(
        "java/nio/ByteBuffer",
        "allocateDirect",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(8)],
    )
}

pub(crate) fn native_perf_create_byte_array(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // W7-86. `createByteArray` is `public native ByteBuffer createByteArray(
    // String, int, int, byte[], int)` — an INSTANCE method (`javap -p --module
    // java.base jdk.internal.perf.Perf`, Adoptium 25.0.3.9) — so `args[0]` is
    // the `Perf` receiver and `maxLength`, the fifth PARAMETER, sits at
    // `args[5]`. The comment this replaces listed the parameters in their
    // STATIC positions and then indexed by them: `args[4]` is the `byte[]`
    // value, a `Value::Object`, which missed the `Value::Int` arm and left
    // `max_length` at its 0 default — so every perf byte-array counter got a
    // ZERO-length direct buffer instead of the size the caller asked for.
    //
    // Not observable from ordinary Java: `jdk.internal.perf.Perf` is exported
    // to nobody and `Perf.getPerf()` is itself gated, so there is no probe for
    // this row and none is pretended. What is measured is the registration:
    // `--dump-native-registry` on a default run shows it owning its slot, and
    // the real method is `acc_native` with no code, so nothing else answers it.
    // Compatible mode (`--real-jdk`, default).
    let max_length = match args.get(5) {
        Some(Value::Int(v)) if *v >= 0 => *v,
        _ => 0,
    };
    ctx.invoke(
        "java/nio/ByteBuffer",
        "allocateDirect",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(max_length)],
    )
}

pub(crate) fn native_perf_high_res_counter(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // zt t21/pl10 (`CRATONVM_PERF_HRT_VM_ORIGIN`, default ON, `=0` kill):
    // HotSpot's `Perf.highResCounter()` is `os::elapsed_counter()`, ticks since
    // the VM began (`os::init`), the same origin as the perf counters'
    // `sun.gc.collector.<n>.lastEntryTime` / `lastExitTime`, so
    // `highResCounter() - lastExitTime` is the time since that pause. Here:
    // ticks since this VM's start (`vm_start_instant`, the origin
    // `perf_data_file` stamps the pause times from), at least 1 (the boot runs
    // for milliseconds before any Java code can ask, so the sentinel idiom
    // below is safe). A context with no start (mocks): the lazy origin below.
    if cratonvm_types::flags::runtime_flag_default_on("CRATONVM_PERF_HRT_VM_ORIGIN") {
        if let Some(origin) = ctx.vm_start_instant() {
            let elapsed = u64::try_from(origin.elapsed().as_nanos()).unwrap_or(u64::MAX);
            let ticks = cratonvm_types::perf_ticks::ticks_from_nanos(elapsed).max(1);
            return Ok(Some(Value::Long(ticks)));
        }
    }
    // highResCounter() -> long nanos-since-start, biased by a fixed non-zero
    // origin. This had exactly the lazy-`OnceLock` shape `System.nanoTime`
    // had, and therefore exactly the same first-reading-is-0 defect. It feeds
    // the same "is my timer armed yet?" comparisons, and `highResFrequency`
    // below reports nanosecond ticks, so callers scale this value as a
    // duration. See `MONOTONIC_ORIGIN_NANOS`.
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    // zt t21/pl10 (`CRATONVM_PERF_HRT_QPC_FREQUENCY`): the elapsed time in
    // ticks of `highResFrequency` (HotSpot's `os::elapsed_counter`: the
    // performance counter on Windows, nanoseconds elsewhere), so a caller
    // that divides by the frequency gets seconds. The origin stays the fixed
    // non-zero offset (in ticks: only its non-zero-ness matters).
    let elapsed = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
    let ticks = MONOTONIC_ORIGIN_NANOS
        .saturating_add(cratonvm_types::perf_ticks::ticks_from_nanos(elapsed));
    Ok(Some(Value::Long(ticks)))
}

pub(crate) fn native_perf_high_res_frequency(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // highResFrequency() -> ticks per second: HotSpot's
    // `os::elapsed_frequency()`, the performance counter's frequency on
    // Windows (zt t21/pl10, `CRATONVM_PERF_HRT_QPC_FREQUENCY`; 1e9 under its
    // `=0` kill), 1_000_000_000 elsewhere. The same figure as the perf image's
    // `sun.os.hrt.frequency`.
    Ok(Some(Value::Long(
        cratonvm_types::perf_ticks::hrt_frequency(),
    )))
}

#[cfg(test)]
mod zt10x7_define0_adoption_tests {
    /// zt t10/x7: `defineClass0`'s duplicate arm runs the deferred `<clinit>`
    /// (`init_after_define`) on a class it adopts from a failed `addClass`
    /// upcall, after the adoption, as its success arm runs it after the
    /// upcall. Before, a retry of a non-hidden `defineClass0` with `initialize`
    /// (`Lookup.defineClass` itself passes `false`) answered the orphan
    /// uninitialized: its own define stopped at the upcall, before the
    /// initialization. A text pin: the unit-test context
    /// cannot run a failing `ArrayList.add` inside a define.
    #[test]
    fn zt10x7_an_adopted_define0_class_is_initialized_after_the_adoption() {
        let src = include_str!("lang_system.rs");
        let start = src
            .find("pub(crate) fn native_classloader_define_class0(")
            .expect("defineClass0");
        let end = start
            + src[start..]
                .find("pub(crate) fn native_perf_attach(")
                .expect("the next native");
        let define0 = &src[start..end];
        let arm = define0
            .find("DuplicateDefine::SameLoaderObject =>")
            .expect("the duplicate arm");
        let adopt = define0[arm..]
            .find("adopt_add_class_orphan(")
            .expect("the adoption")
            + arm;
        let gate = define0[adopt..]
            .find("if !init_after_define")
            .expect("the deferred-init gate")
            + adopt;
        let init = define0[adopt..]
            .find("ctx.initialize_class(adopted_id)?")
            .expect("the deferred init")
            + adopt;
        let refused = define0[adopt..]
            .find("duplicate_define_error(")
            .expect("the LinkageError")
            + adopt;
        assert!(adopt < gate && gate < init, "adopt, then the gate, then <clinit>");
        assert!(init < refused, "inside the adoption, before the LinkageError arm");
    }
}

#[cfg(test)]
mod zt9pl_perf_attach_tests {
    use super::*;

    /// zt t9/pl: lvmid 0 and this process's id are this VM, with or without
    /// a receiver before them; another id, or no `int` at all, is not.
    #[test]
    fn zt9pl_attach0_targets_this_vm_only_for_zero_or_own_pid() {
        let receiver = Value::Object(None);
        assert!(perf_attach_targets_this_vm(&[Value::Int(0)]));
        assert!(perf_attach_targets_this_vm(&[receiver.clone(), Value::Int(0)]));
        let own = i32::try_from(std::process::id()).unwrap();
        assert!(perf_attach_targets_this_vm(&[receiver.clone(), Value::Int(own)]));
        let other = if own == i32::MAX { 1 } else { own + 1 };
        assert!(!perf_attach_targets_this_vm(&[receiver.clone(), Value::Int(other)]));
        assert!(!perf_attach_targets_this_vm(&[receiver]));
        assert!(!perf_attach_targets_this_vm(&[]));
    }

    /// zt t10/pl2: the lvmid `attach0` hands to the foreign door is the
    /// last `int` argument, with or without the receiver before it.
    #[test]
    fn zt10pl2_attach0_reads_the_lvmid_as_the_last_int() {
        let receiver = Value::Object(None);
        assert_eq!(perf_attach_lvmid(&[receiver.clone(), Value::Int(4242)]), Some(4242));
        assert_eq!(perf_attach_lvmid(&[Value::Int(1), Value::Int(2)]), Some(2));
        assert_eq!(perf_attach_lvmid(&[receiver]), None);
        assert_eq!(perf_attach_lvmid(&[]), None);
    }

    /// zt t10/pl2: `Perf.detach` is the native that unmaps a foreign view
    /// (it was a no-op), and `attach0` sends ids other than this VM's to the
    /// foreign door. Text pins: the unit-test context maps no perf data.
    #[test]
    fn zt10pl2_detach_unmaps_and_attach0_has_a_foreign_door() {
        let lib = include_str!("lib.rs");
        let at = lib.find("\"detach\",").expect("Perf.detach");
        let before = &lib[at.saturating_sub(80)..at];
        assert!(before.contains("\"jdk/internal/perf/Perf\","), "{before}");
        let end = at + lib[at..].find(");").expect("the registration's end");
        assert!(lib[at..end].contains("lang_system::native_perf_detach"));
        let src = include_str!("lang_system.rs");
        let start = src.find("pub(crate) fn native_perf_attach0(").expect("attach0");
        let body = &src[start..start + src[start..].find("\n}\n").expect("its end")];
        assert!(body.contains("ctx.attach_foreign_perf_data(lvmid)"));
        assert!(body.contains("ctx.detach_perf_data(address)"));
    }
}

// ---------------------------------------------------------------------------
// Tests for T2.2.21 Thread.sleep(long, int) argument validation
// ---------------------------------------------------------------------------
#[cfg(test)]
mod t2_tests {
    use super::*;
    use crate::test_utils::mock_ctx;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    #[test]
    fn t2_thread_sleep_millis_nanos_zero_is_noop() {
        let mut ctx = mock_ctx();
        let r = native_thread_sleep_millis_nanos(&mut ctx, &[Value::Long(0), Value::Int(0)]);
        assert!(r.is_ok());
    }

    #[test]
    fn t2_thread_sleep_rejects_negative_millis() {
        let mut ctx = mock_ctx();
        let r = native_thread_sleep_millis_nanos(&mut ctx, &[Value::Long(-1), Value::Int(0)]);
        assert!(r.is_err(), "negative millis must throw");
    }

    #[test]
    fn t2_thread_sleep_rejects_negative_nanos() {
        let mut ctx = mock_ctx();
        let r = native_thread_sleep_millis_nanos(&mut ctx, &[Value::Long(0), Value::Int(-1)]);
        assert!(r.is_err(), "negative nanos must throw");
    }

    #[test]
    fn t2_thread_sleep_rejects_oversized_nanos() {
        let mut ctx = mock_ctx();
        let r =
            native_thread_sleep_millis_nanos(&mut ctx, &[Value::Long(0), Value::Int(1_000_000)]);
        assert!(r.is_err(), "nanos >= 1_000_000 must throw");
    }

    #[test]
    fn t2_thread_sleep_accepts_max_nanos() {
        let mut ctx = mock_ctx();
        let r = native_thread_sleep_millis_nanos(&mut ctx, &[Value::Long(0), Value::Int(999_999)]);
        assert!(r.is_ok(), "nanos = 999_999 must be accepted");
    }

    /// i6-L2: `Thread.getStackTrace0` / `dumpThreads` arrays are allocated
    /// with the `StackTraceElement` component once that class is loaded, so
    /// `Thread.asyncGetStackTrace`'s `(StackTraceElement[])` cast holds under
    /// the strict array-cast rule; `Object` only before it is loaded.
    #[test]
    fn i6_l2_stack_trace_arrays_take_the_stack_trace_element_component() {
        let mut ctx = mock_ctx();
        assert_eq!(stack_trace_element_component_id(&mut ctx).as_u32(), 0);
        let ste = ctx
            .ensure_class_initialized("java/lang/StackTraceElement")
            .expect("the mock loads any name");
        assert_eq!(stack_trace_element_component_id(&mut ctx), ste);
    }

    /// i7-L2: `dumpThreads`' outer array takes `StackTraceElement[]` as its
    /// component once the array class exists; `Object` when the context can
    /// neither find nor load it (the mock's `load_class` answers nothing).
    #[test]
    fn i7_l2_dump_threads_outer_array_takes_the_array_class_component() {
        let mut ctx = mock_ctx();
        assert_eq!(stack_trace_element_array_class_id(&mut ctx).as_u32(), 0);
        let ste_array = ctx
            .ensure_class_initialized("[Ljava/lang/StackTraceElement;")
            .expect("the mock loads any name");
        assert_eq!(stack_trace_element_array_class_id(&mut ctx), ste_array);
    }
}

// ---------------------------------------------------------------------------
// Tests for T19.N2 вЂ” Thread.sleep0(J)V
// ---------------------------------------------------------------------------
#[cfg(test)]
mod t19_n2_thread_sleep0_tests {
    use super::*;
    use crate::test_utils::mock_ctx;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    /// 0ms must return essentially immediately (no actual sleep).
    #[test]
    fn t19_n2_thread_sleep0_zero_millis_returns_immediately() {
        let mut ctx = mock_ctx();
        let start = std::time::Instant::now();
        let r = native_thread_sleep0(&mut ctx, &[Value::Long(0)]);
        let elapsed = start.elapsed();
        assert!(r.is_ok(), "sleep0(0) should succeed, got {:?}", r);
        assert!(
            elapsed < std::time::Duration::from_millis(5),
            "sleep0(0) should return in < 5ms, took {:?}",
            elapsed
        );
    }

    /// 10ms must actually block for at least ~10ms (chunk size is 100ms,
    /// so a 10ms request sleeps for the full 10ms in one partial chunk).
    #[test]
    fn t19_n2_thread_sleep0_10ms_actually_sleeps() {
        let mut ctx = mock_ctx();
        let start = std::time::Instant::now();
        let r = native_thread_sleep0(&mut ctx, &[Value::Long(10_000_000)]);
        let elapsed = start.elapsed();
        assert!(r.is_ok(), "sleep0(10) should succeed, got {:?}", r);
        // Must have slept at least the requested amount.
        assert!(
            elapsed >= std::time::Duration::from_millis(10),
            "sleep0(10) should sleep в‰Ґ 10ms, took {:?}",
            elapsed
        );
        // Must not have wildly overslept (generous upper bound for CI).
        assert!(
            elapsed <= std::time::Duration::from_millis(200),
            "sleep0(10) should return within ~200ms upper bound, took {:?}",
            elapsed
        );
    }

    /// Negative millis в†’ IllegalArgumentException (defensive native check).
    #[test]
    fn t19_n2_thread_sleep0_negative_throws_illegal_argument() {
        let mut ctx = mock_ctx();
        let r = native_thread_sleep0(&mut ctx, &[Value::Long(-1)]);
        assert!(r.is_err(), "negative millis must throw");
        match r {
            Err(cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    cratonvm_types::error::RuntimeError::IllegalArgumentException { message },
                ),
            )) => {
                assert!(
                    message.contains("negative"),
                    "expected 'negative' in message, got: {}",
                    message
                );
            }
            other => panic!(
                "expected IllegalArgumentException for negative millis, got {:?}",
                other
            ),
        }
    }

    /// When the interrupt flag is pre-set, sleep0(0) throws immediately.
    #[test]
    fn t19_n2_thread_sleep0_zero_millis_with_interrupt_throws() {
        let ctx = mock_ctx();
        ctx.set_interrupted(true);
        let mut ctx = ctx;
        let r = native_thread_sleep0(&mut ctx, &[Value::Long(0)]);
        assert!(r.is_err(), "sleep0(0) with interrupt flag must throw");
        // And the flag must be CLEARED per JDK spec.
        assert!(
            !ctx.is_interrupted(false),
            "interrupt flag must be cleared after InterruptedException",
        );
    }

    /// Interpreter round i1 wave 24 (L7): HotSpot's `JVM_SleepNanos` checks
    /// the interrupt before the duration, so a zero sleep on an interrupted
    /// thread throws and clears the flag -- through every sleep native
    /// (`Thread.sleep(J)V`, `sleepNanos0`, `sleep(JI)V`). Each used to return
    /// normally with the flag still set (`L7W23Interrupts` row `pre sleep0`).
    #[test]
    fn a_zero_sleep_on_an_interrupted_thread_throws_through_every_sleep_native() {
        type SleepNative = fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult;
        let natives: [(&str, SleepNative, Vec<Value>); 3] = [
            ("sleep(J)V", native_thread_sleep, vec![Value::Long(0)]),
            (
                "sleepNanos0",
                native_thread_sleep_nanos,
                vec![Value::Long(0)],
            ),
            (
                "sleep(JI)V",
                native_thread_sleep_millis_nanos,
                vec![Value::Long(0), Value::Int(0)],
            ),
        ];
        for (name, native, args) in natives {
            let mut ctx = mock_ctx();
            ctx.set_interrupted(true);
            let r = native(&mut ctx, &args);
            assert!(
                r.is_err(),
                "{name}: a zero sleep on an interrupted thread must throw"
            );
            assert!(
                !ctx.is_interrupted(false),
                "{name}: the throw must clear the interrupt flag"
            );
            // And an uninterrupted zero sleep still returns at once.
            let mut ctx = mock_ctx();
            assert!(native(&mut ctx, &args).is_ok(), "{name}: sleep(0) returns");
        }
    }

    /// Wave 24 (L7): `sleepNanos0` sleeps in slices and polls the interrupt,
    /// so an interrupted long sleep ends promptly (it used to be one
    /// uninterruptible `std::thread::sleep` of the whole duration). The mock
    /// context is `!Sync`, so no other thread can raise its flag mid-sleep;
    /// this test pins the pre-check path and its bounded duration, and the
    /// mid-sleep path is covered by the `L7W23Interrupts` / `L7W24SleepInterrupts`
    /// probes.
    /// Interpreter round i5 wave 104, lane L6 (`L6W104SleepNanosNegative`):
    /// HotSpot 25's `JVM_SleepNanos` throws `IllegalArgumentException
    /// ("nanosecond timeout value out of range")` for a negative time, before
    /// it looks at the interrupt (the flag stays set).
    #[test]
    fn sleep_nanos0_of_a_negative_time_throws_before_the_interrupt_check() {
        let mut ctx = mock_ctx();
        ctx.set_interrupted(true);
        let r = native_thread_sleep_nanos(&mut ctx, &[Value::Long(-1)]);
        match r {
            Err(cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    cratonvm_types::error::RuntimeError::IllegalArgumentException { message },
                ),
            )) => assert_eq!(message, "nanosecond timeout value out of range"),
            other => panic!("expected IllegalArgumentException, got {other:?}"),
        }
        assert!(ctx.is_interrupted(false), "the interrupt flag must stay set");
    }

    #[test]
    fn sleep_nanos0_of_an_interrupted_thread_does_not_sleep() {
        let mut ctx = mock_ctx();
        ctx.set_interrupted(true);
        let start = std::time::Instant::now();
        let r = native_thread_sleep_nanos(&mut ctx, &[Value::Long(5_000_000_000)]);
        assert!(r.is_err(), "an interrupted sleepNanos0 must throw");
        assert!(
            start.elapsed() < std::time::Duration::from_millis(500),
            "an interrupted 5 s sleepNanos0 must not sleep, took {:?}",
            start.elapsed()
        );
    }

    /// Interrupt delivered mid-sleep: set the flag on the context, then
    /// run sleep0 for a longer-than-chunk duration and verify that it
    /// returns within one chunk (в‰¤ 110ms) with InterruptedException, and
    /// that the interrupt flag is cleared per JDK spec.
    ///
    /// We pre-set the flag because `MockNativeContext` is `!Sync`; the
    /// chunked poll at loop-top runs BEFORE the first `std::thread::sleep`,
    /// so a pre-set flag exercises the same code path as a flag that
    /// arrives during an earlier chunk.
    #[test]
    fn t19_n2_thread_sleep0_interrupt_during_sleep() {
        let ctx = mock_ctx();
        ctx.set_interrupted(true);
        let mut ctx = ctx;
        let start = std::time::Instant::now();
        // Request a 2-second sleep вЂ” if interrupt polling is broken, the
        // test will hang for ~2s (still fail, but visibly).
        let r = native_thread_sleep0(&mut ctx, &[Value::Long(2_000_000_000)]);
        let elapsed = start.elapsed();
        assert!(r.is_err(), "sleep0 with interrupt must throw");
        match r {
            Err(cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    cratonvm_types::error::RuntimeError::InterruptedException,
                ),
            )) => { /* the bare fallback */ }
            // `sleepNanos0`'s own body builds the real exception
            // (`InterruptedException: sleep interrupted`, as HotSpot's
            // `JVM_SleepNanos` throws).
            Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(_)) => {}
            other => panic!("expected InterruptedException, got {:?}", other),
        }
        // Interrupt flag must be cleared per JDK spec.
        assert!(
            !ctx.is_interrupted(false),
            "interrupt flag must be cleared after InterruptedException",
        );
        // Poll happens at top of loop (before first chunk sleep), so the
        // flag is detected within well under one chunk (100ms). Allow
        // 110ms for CI jitter.
        assert!(
            elapsed <= std::time::Duration::from_millis(110),
            "interrupt should be detected within в‰¤110ms, took {:?}",
            elapsed
        );
    }
}

#[cfg(test)]
mod t14_tests {
    use super::*;
    use crate::test_utils::mock_ctx;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    // -----------------------------------------------------------------------
    // T14.1 вЂ” initPhase1
    // -----------------------------------------------------------------------

    #[test]
    fn init_phase1_succeeds() {
        let mut ctx = mock_ctx();
        // Ensure System class exists so ensure_class_initialized works
        let _ = ctx.ensure_class_initialized("java/lang/System").unwrap();
        let r = native_system_init_phase1(&mut ctx, &[]);
        assert!(r.is_ok());
        assert_eq!(r.unwrap(), None); // void return
    }

    // -----------------------------------------------------------------------
    // T14.2 вЂ” initPhase2
    // -----------------------------------------------------------------------

    #[test]
    fn init_phase2_returns_zero() {
        let mut ctx = mock_ctx();
        let r = native_system_init_phase2(&mut ctx, &[Value::Int(0), Value::Int(0)]);
        assert_eq!(r.unwrap(), Some(Value::Int(0)));
    }

    // -----------------------------------------------------------------------
    // T14.3 вЂ” initPhase3
    // -----------------------------------------------------------------------

    #[test]
    fn init_phase3_succeeds() {
        let mut ctx = mock_ctx();
        let r = native_system_init_phase3(&mut ctx, &[]);
        assert!(r.is_ok());
        assert_eq!(r.unwrap(), None); // void return
    }

    // -----------------------------------------------------------------------
    // T14.4 вЂ” VM.getSavedProperty
    // -----------------------------------------------------------------------

    #[test]
    fn vm_get_saved_property_returns_null_for_missing() {
        let mut ctx = mock_ctx();
        let key = ctx.create_string("nonexistent.property");
        let r = native_vm_get_saved_property(&mut ctx, &[Value::Object(Some(key))]);
        assert_eq!(r.unwrap(), Some(Value::Object(None)));
    }

    #[test]
    fn vm_get_saved_property_returns_value() {
        let mut ctx = mock_ctx();
        ctx.set_system_property("test.key", "test.value");
        let key = ctx.create_string("test.key");
        let r = native_vm_get_saved_property(&mut ctx, &[Value::Object(Some(key))]);
        let obj = match r.unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected string Object, got {other:?}"),
        };
        assert_eq!(ctx.read_string(obj).unwrap(), "test.value");
    }

    #[test]
    fn vm_get_saved_property_null_key() {
        let mut ctx = mock_ctx();
        let r = native_vm_get_saved_property(&mut ctx, &[Value::Object(None)]);
        assert_eq!(r.unwrap(), Some(Value::Object(None)));
    }

    // `saveProperties` stores into `VM`'s static block, and a native does not get
    // the JVMS 5.5 class-initialisation an interpreted `invokestatic` does: on
    // `--jdk-only` the store landed in the pre-`<clinit>` block and `VM.<clinit>`
    // then reset it, so `ClassLoaders.<clinit>` read `savedProps == null` and
    // every `ServiceLoader`/`java.util.logging` user died with
    // `NoClassDefFoundError: ClassLoaders`. The native must initialise `VM` first.
    #[test]
    fn vm_save_properties_initialises_vm_before_storing() {
        let mut ctx = mock_ctx();
        assert!(
            ctx.class_id_by_name("jdk/internal/misc/VM").is_none(),
            "fixture: VM must start uninitialised"
        );
        let props = ctx.create_string("stand-in for the Map");
        let r = native_vm_save_properties(&mut ctx, &[Value::Object(Some(props))]);
        assert_eq!(r.unwrap(), None);
        assert!(
            ctx.class_id_by_name("jdk/internal/misc/VM").is_some(),
            "saveProperties stored into an uninitialised VM"
        );
    }

    #[test]
    fn vm_save_properties_null_argument_is_a_noop() {
        let mut ctx = mock_ctx();
        let r = native_vm_save_properties(&mut ctx, &[Value::Object(None)]);
        assert_eq!(r.unwrap(), None);
    }

    // -----------------------------------------------------------------------
    // T14.5 вЂ” VM.getRuntimeArguments
    // -----------------------------------------------------------------------

    #[test]
    fn vm_get_runtime_arguments_returns_empty_array() {
        let mut ctx = mock_ctx();
        let r = native_vm_get_runtime_arguments(&mut ctx, &[]);
        let arr = match r.unwrap() {
            Some(Value::Object(Some(a))) => a,
            other => panic!("expected array, got {other:?}"),
        };
        assert_eq!(ctx.array_length(arr), 0);
    }
}

#[cfg(test)]
mod t15_tests {
    use super::*;
    use crate::test_utils::mock_ctx;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    // -----------------------------------------------------------------------
    // T15.1.5 вЂ” Finalizer.register
    // -----------------------------------------------------------------------

    #[test]
    fn finalizer_register_with_object() {
        let mut ctx = mock_ctx();
        let obj = ctx.alloc_object(cratonvm_types::ClassId::new(0), 2);
        let r = native_finalizer_register(&mut ctx, &[Value::Object(Some(obj))]);
        assert!(r.is_ok());
        assert_eq!(r.unwrap(), None); // void
                                      // gc-common w5-d: the VM's finalizable-registration hook, not the
                                      // wire-3 (`Cleaner`) `discover_reference` it used to call.
        assert_eq!(*ctx.register_if_finalizable_calls.borrow(), vec![obj]);
    }

    #[test]
    fn finalizer_register_with_null() {
        let mut ctx = mock_ctx();
        let r = native_finalizer_register(&mut ctx, &[Value::Object(None)]);
        assert!(r.is_ok()); // null is silently ignored
        assert!(ctx.register_if_finalizable_calls.borrow().is_empty());
    }

    // -----------------------------------------------------------------------
    // T15.1.6 вЂ” Array.newArray
    // -----------------------------------------------------------------------

    #[test]
    fn array_new_array_int() {
        let mut ctx = mock_ctx();
        let mirror = ctx.create_string("int");
        let r = native_array_new_array(&mut ctx, &[Value::Object(Some(mirror)), Value::Int(5)]);
        let arr = match r.unwrap() {
            Some(Value::Object(Some(a))) => a,
            other => panic!("expected array, got {other:?}"),
        };
        assert_eq!(ctx.array_length(arr), 5);
    }

    #[test]
    fn array_new_array_negative_size() {
        let mut ctx = mock_ctx();
        let mirror = ctx.create_string("int");
        let r = native_array_new_array(&mut ctx, &[Value::Object(Some(mirror)), Value::Int(-1)]);
        assert!(
            r.is_err(),
            "negative size should throw NegativeArraySizeException"
        );
    }

    // -----------------------------------------------------------------------
    // T15.1.3 вЂ” ClassLoader.defineClass0/1/2
    // -----------------------------------------------------------------------

    #[test]
    fn define_class1_empty_bytes_throws() {
        let mut ctx = mock_ctx();
        // Create a byte array with non-CAFEBABE bytes
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 4);
        ctx.set_array_element(arr, 0, Value::Int(0));
        ctx.set_array_element(arr, 1, Value::Int(0));
        ctx.set_array_element(arr, 2, Value::Int(0));
        ctx.set_array_element(arr, 3, Value::Int(0));
        let name = ctx.create_string("com/example/Foo");
        let r = native_classloader_define_class1(
            &mut ctx,
            &[
                Value::Object(None), // loader
                Value::Object(Some(name)),
                Value::Object(Some(arr)),
                Value::Int(0),       // offset
                Value::Int(4),       // length
                Value::Object(None), // pd
                Value::Object(None), // source
            ],
        );
        assert!(
            r.is_err(),
            "invalid class bytes must throw, not return null"
        );
    }

    #[test]
    fn define_class2_reads_bytebuffer_backing_array() {
        let mut ctx = mock_ctx();
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 4);
        for i in 0..4 {
            ctx.set_array_element(arr, i, Value::Int(0));
        }
        let bb = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
        ctx.set_field(bb, 0, Value::Object(Some(arr)));
        ctx.set_field(bb, 1, Value::Int(0));
        ctx.set_field(bb, 2, Value::Int(4));
        ctx.set_field(bb, 3, Value::Int(4));
        let name = ctx.create_string("com/example/Foo");
        let r = native_classloader_define_class2(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Object(Some(name)),
                Value::Object(Some(bb)),
                Value::Int(0),
                Value::Int(4),
                Value::Object(None),
                Value::Object(None),
            ],
        );
        match r {
            Err(cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Linkage(
                    cratonvm_types::error::LinkageError::ClassFormatError { message, .. },
                ),
            )) => assert!(
                message.contains("defineClass2"),
                "expected defineClass2 class-format failure, got {message}"
            ),
            other => panic!("expected ClassFormatError from ByteBuffer handler, got {other:?}"),
        }
    }

    #[test]
    fn define_class0_empty_bytes_throws() {
        let mut ctx = mock_ctx();
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 4);
        let name = ctx.create_string("com/example/Foo");
        let r = native_classloader_define_class0(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Object(None),
                Value::Object(Some(name)),
                Value::Object(Some(arr)),
                Value::Int(0),
                Value::Int(4),
                Value::Object(None),
                Value::Int(0),
                Value::Int(0),
                Value::Object(None),
            ],
        );
        assert!(
            r.is_err(),
            "invalid class bytes must throw, not return null"
        );
    }

    #[test]
    fn define_class1_out_of_bounds() {
        let mut ctx = mock_ctx();
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 2);
        let name = ctx.create_string("com/example/Foo");
        let r = native_classloader_define_class1(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Object(Some(name)),
                Value::Object(Some(arr)),
                Value::Int(0),
                Value::Int(10), // length > array size
                Value::Object(None),
                Value::Object(None),
            ],
        );
        assert!(r.is_err(), "should throw ArrayIndexOutOfBoundsException");
    }
}

// ---------------------------------------------------------------------------
// SecurityManager.checkExec gating for process spawn (HIGH security)
// ---------------------------------------------------------------------------
//
// These tests verify the contract documented on `check_exec_or_throw`:
//
//   1. With no SecurityManager installed, the gate is a no-op and the
//      spawn proceeds (existing behaviour, preserves backwards compat).
//   2. With a SecurityManager that denies any exec, the gate propagates
//      `SecurityException` and `std::process::Command` is NEVER touched.
//   3. With a SecurityManager that allows only specific paths, only the
//      allowed paths reach the spawn syscall.
//
// We exercise the integration through `native_runtime_exec_string` and
// `native-io`'s `native_process_builder_start` -- the two Java-visible spawn
// entry points -- so any future refactor that bypasses `check_exec_or_throw`
// regresses these tests. `ProcessBuilder.start` reaches the gate only through
// the hook `install_spawn_policy_hook` installs, which is why that test calls
// it explicitly.
//
// MockNativeContext.invoke_virtual returns whatever's pre-armed in
// `invoke_virtual_result` (taken once), defaulting to `Ok(None)` вЂ”
// matching JDK's "no exception thrown == allowed" semantics. This lets us
// simulate both deny (pre-arm an Err) and allow (default).
/// `Runtime.exec` cmdarray validation, measured against HotSpot 25 in
/// probes/ProcSurfaceProbe.java (T10/T11).
#[cfg(test)]
mod exec_cmdarray_tests {
    use super::*;
    use crate::test_utils::mock_ctx;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use cratonvm_types::error::VmError;
    use cratonvm_types::ArrayElementType;

    /// Build a `String[]`; `None` becomes a genuine null element.
    fn string_array(ctx: &mut dyn NativeContext, items: &[Option<&str>]) -> Value {
        let arr = ctx.new_array(ArrayElementType::Reference, items.len());
        for (i, item) in items.iter().enumerate() {
            let v = match item {
                Some(s) => {
                    let obj = ctx.create_string(s);
                    Value::Object(Some(obj))
                }
                None => Value::Object(None),
            };
            ctx.set_array_element(arr, i, v);
        }
        Value::Object(Some(arr))
    }

    fn assert_npe(err: &MethodCallFailed) {
        match err {
            MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException { .. },
            )) => {}
            other => panic!("expected NullPointerException, got {other:?}"),
        }
    }

    #[test]
    fn a_null_command_array_is_a_null_pointer_exception() {
        let mut ctx = mock_ctx();
        let err = checked_cmdarray(&mut ctx, &Value::Object(None))
            .expect_err("a null cmdarray must throw");
        assert_npe(&err);
    }

    /// The old reader DROPPED a null element, so `exec(["/bin/echo", null])`
    /// silently ran `/bin/echo` with no arguments -- a different command line
    /// than the caller wrote. HotSpot throws.
    #[test]
    fn a_null_element_is_a_null_pointer_exception_not_a_shorter_command() {
        let mut ctx = mock_ctx();
        let arr = string_array(&mut ctx, &[Some("/bin/echo"), None]);
        let err = checked_cmdarray(&mut ctx, &arr).expect_err("a null element must throw");
        assert_npe(&err);
    }

    #[test]
    fn an_empty_command_array_is_an_array_index_out_of_bounds() {
        let mut ctx = mock_ctx();
        let arr = string_array(&mut ctx, &[]);
        let err = checked_cmdarray(&mut ctx, &arr).expect_err("an empty cmdarray must throw");
        match err {
            MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::ArrayIndexOutOfBoundsException { index, .. },
            )) => assert_eq!(index, 0, "the failing read is cmdarray[0]"),
            other => panic!("expected ArrayIndexOutOfBoundsException, got {other:?}"),
        }
    }

    /// `Runtime.exec` must return with the child STILL RUNNING.
    ///
    /// This is the regression test for the defect itself. `exec` used to call
    /// `Command::output()`, which runs the child to completion and buffers its
    /// output -- so `exec` blocked for the child's whole lifetime, `isAlive()`
    /// was never true, `exitValue()` never threw `IllegalThreadStateException`
    /// (the signal Tomcat's `CGIServlet` polls on), and a child reading stdin
    /// could never be fed, because the caller only gets the pipe once `exec`
    /// has returned. Against the old code this test spends five seconds inside
    /// `exec` and then fails.
    ///
    /// The bound is deliberately loose (2.5s against a 5s child): the claim is
    /// "returned before the child finished", not a latency budget, and this
    /// runs on a shared, heavily loaded host.
    ///
    /// The child is left to exit on its own. Reaping it would mean reaching
    /// into `native-io`'s private Process layout for the handle, and a stray
    /// `sleep 5` costs nothing.
    #[test]
    #[cfg(not(target_os = "windows"))]
    fn runtime_exec_returns_while_the_child_is_still_running() {
        // Serialize against the checkexec tests: the spawn policy hook is
        // process-global once installed, so a deny-all SecurityManager
        // installed by one of those tests would refuse this spawn.
        let _guard = crate::security_manager::security_state_test_lock();
        let mut ctx = mock_ctx();
        let arr = string_array(&mut ctx, &[Some("/bin/sleep"), Some("5")]);
        let started = std::time::Instant::now();
        let result = native_runtime_exec_array(&mut ctx, &[Value::Object(None), arr]);
        let elapsed = started.elapsed();
        assert!(
            result.is_ok(),
            "spawning /bin/sleep must succeed, got {result:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(2500),
            "Runtime.exec must not wait for the child: took {elapsed:?}"
        );
    }

    #[test]
    fn a_well_formed_command_array_reads_back_in_order() {
        let mut ctx = mock_ctx();
        let arr = string_array(&mut ctx, &[Some("/bin/echo"), Some("a b"), Some("c")]);
        let cmd = checked_cmdarray(&mut ctx, &arr).expect("a valid cmdarray must not throw");
        assert_eq!(cmd, vec!["/bin/echo", "a b", "c"]);
    }
}

#[cfg(test)]
mod checkexec_security_tests {
    use super::*;
    use crate::security_manager::{security_state_test_lock, set_security_manager_for_test};
    use crate::test_utils::mock_ctx;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use cratonvm_types::error::{MethodCallFailed, RuntimeError, VmError};

    /// Helper: assert the failure is a SecurityException (regardless of
    /// the exact message вЂ” the wrapping is `MethodCallFailed::InternalError(
    /// VmError::Runtime(RuntimeError::SecurityException { .. }))`).
    fn assert_security_exception(err: &MethodCallFailed) {
        match err {
            MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::SecurityException { .. },
            )) => {}
            other => panic!("expected RuntimeError::SecurityException, got {other:?}",),
        }
    }

    // -----------------------------------------------------------------------
    // (1) No SecurityManager вЂ” spawn gate is a no-op.
    // -----------------------------------------------------------------------

    #[test]
    fn no_security_manager_allows_check_exec() {
        let _guard = security_state_test_lock();
        // Ensure no SM is installed (defensive вЂ” other tests may have set one).
        // The slot is per-VM now, so the reset needs the ctx whose identity
        // owns it. `mock_ctx()` reports the default identity 0.
        let mut ctx = mock_ctx();
        let prev = set_security_manager_for_test(&ctx, None);

        let result = check_exec_or_throw(&mut ctx, "/usr/bin/ls");
        assert!(
            result.is_ok(),
            "check_exec_or_throw must be a no-op with no SecurityManager, got {result:?}",
        );

        let _ = set_security_manager_for_test(&ctx, prev);
    }

    // -----------------------------------------------------------------------
    // (2) SecurityManager that denies every checkExec вЂ” SecurityException
    //     propagates AND the spawn does NOT happen.
    // -----------------------------------------------------------------------

    #[test]
    fn denying_security_manager_blocks_check_exec() {
        let _guard = security_state_test_lock();
        let mut ctx = mock_ctx();
        let sm = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
        let prev = set_security_manager_for_test(&ctx, Some(sm));

        // Pre-arm the mock so the next invoke_virtual returns a SecurityException.
        // This simulates a SecurityManager whose checkExec(String) denies.
        unsafe {
            *ctx.invoke_virtual_result.get() = Some(Err(MethodCallFailed::InternalError(
                VmError::Runtime(RuntimeError::SecurityException {
                    message: "access denied (test policy denies all exec)".to_string(),
                }),
            )));
        }

        let result = check_exec_or_throw(&mut ctx, "/usr/bin/evil");
        let err = result.expect_err("denying SM must surface SecurityException");
        assert_security_exception(&err);

        let _ = set_security_manager_for_test(&ctx, prev);
    }

    #[test]
    fn denying_sm_blocks_runtime_exec_before_spawn() {
        let _guard = security_state_test_lock();
        // End-to-end check: a deny-all SM must short-circuit
        // native_runtime_exec_string with SecurityException вЂ” std::process::Command
        // is never invoked. Using a bogus program path proves no fallback
        // "Runtime.exec failed: ..." IOException can leak through, because
        // if the SM check is skipped the spawn would attempt the path and
        // surface IOException, not SecurityException.
        //
        // nio21 wave 5 L1: since `ac50933ed5` (2026-08-06) the check runs in
        // `spawn_and_wrap`, through the hook `install_spawn_policy_hook`
        // installs -- and nothing in a unit-test binary installs it but the
        // tests that call it. This test did not, so it passed only when
        // `denying_sm_blocks_processbuilder_start` happened to run first in
        // the same process, and failed alone (`cargo test ...
        // denying_sm_blocks_runtime_exec_before_spawn`) with the spawn's
        // `IOException`. Install it here, as that test does (idempotent).
        install_spawn_policy_hook();
        let mut ctx = mock_ctx();
        let sm = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
        let prev = set_security_manager_for_test(&ctx, Some(sm));
        unsafe {
            *ctx.invoke_virtual_result.get() = Some(Err(MethodCallFailed::InternalError(
                VmError::Runtime(RuntimeError::SecurityException {
                    message: "deny".to_string(),
                }),
            )));
        }

        // args[0] = Runtime instance (irrelevant here), args[1] = command.
        let runtime_instance =
            try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Runtime", 0).unwrap();
        let cmd = ctx.create_string("/path/to/definitely-nonexistent-binary-xyz");
        let result = native_runtime_exec_string(
            &mut ctx,
            &[
                Value::Object(Some(runtime_instance)),
                Value::Object(Some(cmd)),
            ],
        );

        let err = result.expect_err("deny-all SM must block Runtime.exec spawn");
        assert_security_exception(&err);

        let _ = set_security_manager_for_test(&ctx, prev);
    }

    #[test]
    fn denying_sm_blocks_processbuilder_start() {
        let _guard = security_state_test_lock();
        // Drives the REAL `ProcessBuilder.start` -- `native-io`'s, the one this
        // crate now registers. It used to drive `native_pb_start`, a stub that
        // never spawned, so what it proved was that a stub asked permission
        // before doing nothing.
        //
        // `install_spawn_policy_hook()` is the point: the gate reaches
        // `native-io` only through that hook, and nothing else in a unit-test
        // binary installs it. If the wiring regresses, the spawn goes through
        // and this test fails.
        install_spawn_policy_hook();
        let mut ctx = mock_ctx();
        let sm = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
        let prev = set_security_manager_for_test(&ctx, Some(sm));
        unsafe {
            *ctx.invoke_virtual_result.get() = Some(Err(MethodCallFailed::InternalError(
                VmError::Runtime(RuntimeError::SecurityException {
                    message: "deny".to_string(),
                }),
            )));
        }

        // Build a ProcessBuilder synthetic with a 4-slot layout and a
        // command list. `start` reads slot 0; we plant a String[] there with
        // command[0] = "/bin/anything", a path that does not exist -- so if the
        // gate ever fails to refuse, the spawn fails with an IOException rather
        // than running something, and the assertion below still catches it.
        let pb = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/ProcessBuilder", 4).unwrap();
        let cmd_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 1);
        let prog = ctx.create_string("/bin/anything");
        ctx.set_array_element(cmd_arr, 0, Value::Object(Some(prog)));
        ctx.set_field(pb, 0, Value::Object(Some(cmd_arr)));

        let result = cratonvm_native_io::process::native_process_builder_start(
            &mut ctx,
            &[Value::Object(Some(pb))],
        );
        let err = result.expect_err("deny-all SM must block ProcessBuilder.start");
        assert_security_exception(&err);

        let _ = set_security_manager_for_test(&ctx, prev);
    }

    // -----------------------------------------------------------------------
    // (3) Allow-specific-path SecurityManager вЂ” only listed paths spawn.
    // -----------------------------------------------------------------------
    //
    // The MockNativeContext's `invoke_virtual_result` is consumed by
    // `take()` per call, so for two back-to-back checkExec invocations we
    // pre-arm the mock once with Err (deny) for the first call, then leave
    // it unset so the second call falls through to the default `Ok(None)`
    // (allow). This mirrors a real SM that allows the second program but
    // denies the first.

    #[test]
    fn allow_listed_sm_lets_specific_paths_through() {
        let _guard = security_state_test_lock();
        let mut ctx = mock_ctx();
        let sm = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/SecurityManager", 0).unwrap();
        let prev = set_security_manager_for_test(&ctx, Some(sm));

        // First call: simulate a denial for the disallowed binary.
        unsafe {
            *ctx.invoke_virtual_result.get() = Some(Err(MethodCallFailed::InternalError(
                VmError::Runtime(RuntimeError::SecurityException {
                    message: "deny /usr/bin/danger".to_string(),
                }),
            )));
        }
        let denied = check_exec_or_throw(&mut ctx, "/usr/bin/danger");
        let err = denied.expect_err("disallowed path must surface SecurityException");
        assert_security_exception(&err);

        // Second call: invoke_virtual_result was take()n on the previous
        // call, so the mock now falls back to its default Ok(None) вЂ”
        // simulating the allow-list permitting this program.
        let allowed = check_exec_or_throw(&mut ctx, "/bin/allowed-program");
        assert!(
            allowed.is_ok(),
            "allow-listed path must pass the gate, got {allowed:?}",
        );

        let _ = set_security_manager_for_test(&ctx, prev);
    }
}

#[cfg(test)]
mod runtime_version_parse_tests {
    use super::parse_runtime_version_str;

    /// Parse and render as `numbers|pre|build|optional` for terse assertions.
    fn shape(s: &str) -> String {
        let p = parse_runtime_version_str(s).expect("parse");
        let numbers = p
            .numbers
            .iter()
            .map(i32::to_string)
            .collect::<Vec<_>>()
            .join(".");
        format!(
            "{numbers}|{}|{}|{}",
            p.pre.unwrap_or_default(),
            p.build.map(|b| b.to_string()).unwrap_or_default(),
            p.optional.unwrap_or_default(),
        )
    }

    /// Every shape the JDK's `$VNUM(-$PRE)?(\+$BUILD)?(-$OPT)?` grammar admits.
    /// The expectations are `Runtime.Version.parse(s)`'s own field values on
    /// HotSpot 25 (verified against a real JDK, not derived from this parser).
    #[test]
    fn parses_the_vstr_grammar() {
        assert_eq!(shape("25"), "25|||");
        assert_eq!(shape("25.0.3"), "25.0.3|||");
        // The two version strings this VM reports for itself.
        assert_eq!(shape("25.0.1+8"), "25.0.1||8|");
        assert_eq!(shape("25.0.1+8-LTS-27"), "25.0.1||8|LTS-27");
        // Temurin's, for a real-JDK cross-check.
        assert_eq!(shape("25.0.3+9-LTS"), "25.0.3||9|LTS");
        assert_eq!(shape("21-ea+3"), "21|ea|3|");
        assert_eq!(shape("17.0.2-ea+7-abc"), "17.0.2|ea|7|abc");
        // `+-$OPT` is the JDK's spelling for optional-without-build.
        assert_eq!(shape("17+-abc"), "17|||abc");
        // `-$PRE-$OPT` with no build at all.
        assert_eq!(shape("17-ea-abc"), "17|ea||abc");
    }

    #[test]
    fn rejects_strings_with_no_usable_version_number() {
        assert!(parse_runtime_version_str("").is_none());
        assert!(parse_runtime_version_str("nonsense").is_none());
        // `1.8.0_392` is the pre-JEP-223 spelling; the JDK rejects it too.
        assert!(parse_runtime_version_str("1.8.0_392").is_none());
    }
}

// ---------------------------------------------------------------------------
// P3-C — `defineClass0/1/2` must not flatten a typed linkage error
// (W7-31-enable-preview-wiring.md, Falsifier 3)
// ---------------------------------------------------------------------------
#[cfg(test)]
mod define_class_error_typing_tests {
    use super::*;
    use cratonvm_types::error::{ClassFileError, VmError};

    /// Reproduce EXACTLY what `NativeContextImpl::define_class_full` puts in its
    /// `Err(String)`: `class_manager::define_class_with_options`' `VmError`,
    /// rendered by `.map_err(|e| format!("{e:?}"))` (`vm/src/vm/vm_exec.rs`).
    ///
    /// This is deliberately not a hand-written literal. The recovery in
    /// `typed_define_class_error` is a parse of that rendering, so the coupling
    /// is real and these tests are the thing that notices if either side moves —
    /// in particular if that `map_err` is ever "tidied" to `{e}` (`Display`),
    /// which would silently return every arm below to `ClassFormatError`.
    fn as_backend_string(err: VmError) -> String {
        format!("{err:?}")
    }

    fn linkage_of(failed: &MethodCallFailed) -> &LinkageError {
        match failed {
            MethodCallFailed::InternalError(VmError::Linkage(l)) => l,
            other => panic!("expected a LinkageError, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_class_version_survives_the_string_boundary() {
        // HotSpot 25's wording verbatim, from the W7-31 falsifier. `<Unknown>`
        // is what HotSpot prints when the caller passed a null name, and the
        // version check runs before `this_class` is read — so the empty
        // `class_name` here is faithful, not lost.
        let hotspot = "Preview features are not enabled for <Unknown> (class file version \
                       69.65535). Try running with '--enable-preview'";
        let backend = as_backend_string(VmError::Linkage(
            LinkageError::UnsupportedClassVersionError {
                class_name: String::new(),
                message: hotspot.to_string(),
            },
        ));
        // The shape this test exists to defend against.
        assert!(
            backend.contains("Linkage(UnsupportedClassVersionError {"),
            "the backend rendering changed shape: {backend}"
        );

        let failed = define_class_linkage_error("", "defineClass1", backend);
        match linkage_of(&failed) {
            LinkageError::UnsupportedClassVersionError { message, .. } => {
                assert_eq!(message, hotspot, "the message must be HotSpot's, verbatim");
            }
            other => panic!("must stay an UnsupportedClassVersionError, got {other:?}"),
        }
    }

    /// gc-common w6-f: a `-XX:MaxMetaspaceSize` refusal reaches Java as
    /// HotSpot's `OutOfMemoryError: Metaspace`, not as a `ClassFormatError`.
    #[test]
    fn w6f_metaspace_refusal_is_an_out_of_memory_error() {
        let backend = as_backend_string(VmError::Runtime(RuntimeError::OutOfMemoryError {
            message: "Metaspace".to_string(),
        }));
        match define_class_linkage_error("p/Gen", "defineClass1", backend) {
            MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError {
                message,
            })) => assert_eq!(message, "Metaspace"),
            other => panic!("expected OutOfMemoryError: Metaspace, got {other:?}"),
        }
    }

    /// gc-common w11-e: the backend's `DuplicateClassDefinition` (its `Debug`
    /// text is what `define_class_full` hands back) is a duplicate define, as
    /// the older "already defined" wording is; anything else is not.
    #[test]
    fn w11e_both_duplicate_define_spellings_are_recognised() {
        let modern = format!(
            "{:?}",
            VmError::Linkage(LinkageError::DuplicateClassDefinition {
                class_name: "p/C".to_string(),
                loader: "Application".to_string(),
            })
        );
        assert!(is_duplicate_define_message("p/C", &modern));
        assert!(is_duplicate_define_message(
            "p/C",
            "IncompatibleClassChangeError: p/C already defined by application loader"
        ));
        assert!(!is_duplicate_define_message(
            "p/C",
            "ClassFormatError: truncated"
        ));
        assert!(!is_duplicate_define_message("", &modern));
    }

    /// Every `contains("already defined")` in `native-builtins/src` also
    /// accepts `DuplicateClassDefinition` (gc-common w11-e: two sites matched
    /// only the old wording for a month and their recovery arms were dead).
    #[test]
    fn w11e_every_already_defined_match_also_accepts_the_backend_variant() {
        let src_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
        let mut stack = vec![std::path::PathBuf::from(src_dir)];
        let mut offenders = Vec::new();
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("src is readable").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                let lines: Vec<&str> = text.lines().collect();
                for (i, line) in lines.iter().enumerate() {
                    if !line.contains(".contains(\"already defined") {
                        continue;
                    }
                    let window = lines[i.saturating_sub(3)..(i + 4).min(lines.len())].join(
                        "
",
                    );
                    if !window.contains("DuplicateClassDefinition") {
                        offenders.push(format!("{}:{}", path.display(), i + 1));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "matches only the old wording: {offenders:?}"
        );
    }

    #[test]
    fn no_recovered_message_carries_a_rust_debug_rendering() {
        for err in [
            VmError::Linkage(LinkageError::UnsupportedClassVersionError {
                class_name: "P".to_string(),
                message: "bad version".to_string(),
            }),
            VmError::Linkage(LinkageError::ClassFormatError {
                class_name: "P".to_string(),
                message: "truncated constant pool".to_string(),
            }),
            VmError::Linkage(LinkageError::IncompatibleClassChangeError {
                message: "already defined by application loader".to_string(),
            }),
            VmError::Linkage(LinkageError::VerifyError {
                class_name: "P".to_string(),
                method_name: "m".to_string(),
                message: "bad stack map".to_string(),
            }),
            VmError::Runtime(RuntimeError::SecurityException {
                message: "Prohibited package name: java.evil".to_string(),
            }),
            VmError::ClassFile(ClassFileError::InvalidClassFile {
                class_name: "P".to_string(),
                message: "circular class hierarchy detected: P".to_string(),
            }),
        ] {
            let rendered = format!("{err:?}");
            let failed = define_class_linkage_error("P", "defineClass1", rendered.clone());
            // `Display`, not `Debug`: this is the text that becomes the Java
            // exception's message. `Debug` of the *outcome* is a Rust value dump
            // by definition and asserting on it would measure nothing.
            let text = format!("{failed}");
            for artifact in [
                "class_name:",
                "message:",
                "Linkage(",
                "Runtime(",
                "ClassFile(",
            ] {
                assert!(
                    !text.contains(artifact),
                    "a Debug artifact {artifact:?} reached the Java-visible error \
                     for {rendered}: {text}"
                );
            }
        }
    }

    /// JVMS §5.3.5: the loading guard's circular hierarchy leaves the
    /// `defineClass` natives as a `ClassCircularityError`, not the
    /// `ClassFormatError` every unmapped backend shape falls back to.
    #[test]
    fn a_circular_hierarchy_stays_a_class_circularity_error() {
        let backend = as_backend_string(VmError::Linkage(LinkageError::ClassCircularityError {
            class_name: "p/A".to_string(),
        }));
        match linkage_of(&define_class_linkage_error("p/A", "defineClass1", backend)) {
            LinkageError::ClassCircularityError { class_name } => assert_eq!(class_name, "p/A"),
            other => panic!("JVMS §5.3.5 wants a ClassCircularityError, got {other:?}"),
        }
    }

    #[test]
    fn a_prohibited_package_stays_a_security_exception() {
        let backend = as_backend_string(VmError::Runtime(RuntimeError::SecurityException {
            message: "Prohibited package name: java.evil".to_string(),
        }));
        match define_class_linkage_error("java/evil/X", "defineClass1", backend) {
            MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::SecurityException { message },
            )) => {
                assert_eq!(message, "Prohibited package name: java.evil");
            }
            other => panic!("JVMS §5.3.5 wants a SecurityException, got {other:?}"),
        }
    }

    /// A `VmError::ClassFile` returned from a native is UNCATCHABLE —
    /// `classify_fastpath_invoke_error` sends it to `FastPathInvokeError::Fatal`.
    /// Every `ClassFile` arm must therefore leave as a `Linkage` variant.
    #[test]
    fn class_file_errors_are_re_homed_onto_catchable_linkage_variants() {
        for err in [
            VmError::ClassFile(ClassFileError::ClassNotFound {
                class_name: "Missing".to_string(),
            }),
            VmError::ClassFile(ClassFileError::InvalidClassFile {
                class_name: "P".to_string(),
                message: "circular class hierarchy detected: P".to_string(),
            }),
            VmError::ClassFile(ClassFileError::UnsupportedVersion {
                class_name: "P".to_string(),
                major: 99,
                minor: 0,
            }),
        ] {
            let failed = define_class_linkage_error("P", "defineClass1", format!("{err:?}"));
            assert!(
                matches!(
                    &failed,
                    MethodCallFailed::InternalError(VmError::Linkage(_))
                ),
                "a ClassFile error must be re-homed onto a Linkage variant, got {failed:?}"
            );
        }
        let failed = define_class_linkage_error(
            "P",
            "defineClass1",
            format!(
                "{:?}",
                VmError::ClassFile(ClassFileError::ClassNotFound {
                    class_name: "Missing".to_string(),
                })
            ),
        );
        match linkage_of(&failed) {
            LinkageError::NoClassDefFoundError { class_name } => assert_eq!(class_name, "Missing"),
            other => panic!("an unresolvable supertype is a NoClassDefFoundError, got {other:?}"),
        }
    }

    /// `define_class_full`'s own plain-string failures are not `Debug`
    /// renderings and must pass through as they always did.
    #[test]
    fn a_plain_backend_string_keeps_the_old_class_format_error() {
        let failed = define_class_linkage_error(
            "Foo",
            "defineClass1",
            "define_class_full failed for Foo".to_string(),
        );
        match linkage_of(&failed) {
            LinkageError::ClassFormatError {
                class_name,
                message,
            } => {
                assert_eq!(class_name, "Foo");
                assert_eq!(message, "defineClass1: define_class_full failed for Foo");
            }
            other => panic!("expected the unchanged ClassFormatError fallback, got {other:?}"),
        }
    }

    #[test]
    fn a_quoted_message_round_trips_through_the_debug_escaping() {
        let quoted = "class \"P\" has a \\ in it";
        let backend = as_backend_string(VmError::Linkage(LinkageError::ClassFormatError {
            class_name: "P".to_string(),
            message: quoted.to_string(),
        }));
        match linkage_of(&define_class_linkage_error("P", "defineClass1", backend)) {
            LinkageError::ClassFormatError { message, .. } => assert_eq!(message, quoted),
            other => panic!("expected ClassFormatError, got {other:?}"),
        }
    }

    #[test]
    fn the_backend_name_wins_over_the_callers_when_the_caller_has_none() {
        let backend = as_backend_string(VmError::Linkage(LinkageError::NoClassDefFoundError {
            class_name: "Super".to_string(),
        }));
        match linkage_of(&define_class_linkage_error("", "defineClass1", backend)) {
            LinkageError::NoClassDefFoundError { class_name } => assert_eq!(class_name, "Super"),
            other => panic!("expected NoClassDefFoundError, got {other:?}"),
        }
    }

    #[test]
    fn split_debug_error_rejects_a_human_written_string() {
        assert!(split_debug_error("initialize after define failed for Foo: boom").is_none());
        assert!(split_debug_error("").is_none());
        assert!(split_debug_error("no parens here").is_none());
    }

    #[test]
    fn debug_string_field_only_matches_at_a_field_boundary() {
        let body = r#"class_name: "A", message: "the class_name: \"B\" is wrong""#;
        assert_eq!(debug_string_field(body, "class_name").as_deref(), Some("A"));
        assert_eq!(
            debug_string_field(body, "message").as_deref(),
            Some(r#"the class_name: "B" is wrong"#)
        );
        assert!(debug_string_field(body, "loader").is_none());
    }
}

/// G11-1 (2026-08-17) — the shutdown-hook registration contract.
///
/// Everything here is a unit test of the REFUSAL LADDER and the exact text of
/// its four messages, because that is the part of W7-92's runner that can be
/// decided without a live VM. What these tests deliberately do NOT prove:
///
/// * that a hook ever RUNS. `run_shutdown_hooks` needs `invoke_virtual`, a
///   thread registry and a real `Thread.start()`; only `RShutdownHooks`
///   against a built binary can settle that.
/// * the `Hook already running` arm. `test_utils`' mock answers
///   `thread_is_alive == false` unconditionally, so that arm is reachable in
///   the VM and not from here; it is pinned by the constant test instead.
///
/// The registry is per VM since gc-common w16-e (`SHUTDOWN_STATE`). The tests
/// that touch it use their own VM identity (`hook_ctx`), so no other test in
/// the binary shares their row; they still take `hook_registry_guard()`
/// because they share that one row with each other.
#[cfg(test)]
mod shutdown_hook_contract_tests {
    use super::*;
    use crate::test_utils::mock_ctx;
    // `alloc_object` lives on `NativeHeapAccess`, one of the traits
    // `NativeContext` composes -- so importing `NativeContext` alone does NOT
    // bring it into scope on a concrete `MockNativeContext`. This is the same
    // import block every other test module in the crate uses; see
    // `lang_class.rs`'s `mod tests`.
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
        NativeSystemAccess, NativeThreadAccess,
    };

    /// The VM identity every registry test in this module runs as.
    const HOOK_TEST_VM: usize = 0xE16_0301;

    fn hook_ctx() -> crate::test_utils::MockNativeContext {
        let ctx = mock_ctx();
        ctx.set_vm_identity(HOOK_TEST_VM);
        ctx
    }

    /// Serialises every test in this module against their shared registry
    /// row. Poisoning is ignored on purpose: a panicking test must not turn
    /// the rest of the module red for the wrong reason.
    fn hook_registry_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Put the test VM's row back into the state a fresh VM has.
    fn reset_registry() {
        SHUTDOWN_STATE.forget(HOOK_TEST_VM);
    }

    fn hook_count() -> usize {
        registered_shutdown_hooks(HOOK_TEST_VM).len()
    }

    /// Unwrap to the `RuntimeError` and assert on the VARIANT and the message
    /// STRING, never on `format!("{failed:?}")`.
    ///
    /// Two reasons, and the first one is a trap this test module fell into on
    /// its first draft: `Debug` escapes the embedded quotes, so
    /// `HOOK_MSG_NULL_ADD` — which contains `"java.lang.Thread.isAlive()"` —
    /// never appears verbatim in a `Debug` rendering and a `.contains()` check
    /// against it silently fails no matter what the VM does. The second is
    /// that `Debug` of `Option<String>` is the only way to tell
    /// `getMessage() == null` from `getMessage() == ""` here, and asserting on
    /// the `Option` itself says it directly.
    fn runtime_error(failed: MethodCallFailed) -> RuntimeError {
        match failed {
            MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(e)) => e,
            other => panic!("expected a RuntimeError, got {other:?}"),
        }
    }

    // -- the four messages, transcribed --------------------------------

    #[test]
    fn refusal_messages_are_the_measured_hotspot_text() {
        assert_eq!(HOOK_MSG_SHUTDOWN_IN_PROGRESS, "Shutdown in progress");
        assert_eq!(HOOK_MSG_ALREADY_RUNNING, "Hook already running");
        assert_eq!(HOOK_MSG_PREVIOUSLY_REGISTERED, "Hook previously registered");
        assert_eq!(
            HOOK_MSG_NULL_ADD,
            "Cannot invoke \"java.lang.Thread.isAlive()\" because \"hook\" is null"
        );
    }

    /// HANDOFF-20260814 §7: a non-ASCII byte in a compared string makes the
    /// oracle comparison depend on the Windows console code page, and one
    /// differential once failed on a single em-dash with every assertion
    /// passing. These four strings cross that boundary, so they are checked
    /// rather than eyeballed.
    #[test]
    fn refusal_messages_are_pure_ascii() {
        for m in [
            HOOK_MSG_SHUTDOWN_IN_PROGRESS,
            HOOK_MSG_NULL_ADD,
            HOOK_MSG_ALREADY_RUNNING,
            HOOK_MSG_PREVIOUSLY_REGISTERED,
        ] {
            assert!(m.is_ascii(), "non-ASCII in a compared message: {m:?}");
        }
    }

    // -- argument decoding ---------------------------------------------

    /// A present-but-null slot 1 is a JAVA null and must reach the NPE arm; a
    /// short vector is a VM-side decode fault and must NOT be turned into one.
    /// Before G11-1 both were the same silent `Ok(None)`.
    #[test]
    fn a_java_null_and_a_short_vector_decode_differently() {
        let mut ctx = mock_ctx();
        let receiver = ctx.alloc_object(cratonvm_types::ClassId::new(0), 2);
        let hook = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);

        assert_eq!(
            shutdown_hook_argument(
                &[Value::Object(Some(receiver)), Value::Object(Some(hook))],
                "t"
            ),
            Some(Some(hook)),
            "slot 1 holding a reference is the hook"
        );
        assert_eq!(
            shutdown_hook_argument(&[Value::Object(Some(receiver)), Value::Object(None)], "t"),
            Some(None),
            "slot 1 holding null is a Java null, not a decode failure"
        );
        assert_eq!(
            shutdown_hook_argument(&[Value::Object(Some(receiver))], "t"),
            None,
            "a one-slot vector is not [receiver, Thread] and must not be decided"
        );
        assert_eq!(
            shutdown_hook_argument(&[], "t"),
            None,
            "an empty vector likewise"
        );
        assert_eq!(
            shutdown_hook_argument(&[Value::Object(Some(receiver)), Value::Int(7)], "t"),
            None,
            "slot 1 holding a primitive is not a Thread reference"
        );
    }

    // -- the refusal ladder --------------------------------------------

    #[test]
    fn null_add_throws_npe_with_the_helpful_message() {
        let _g = hook_registry_guard();
        reset_registry();
        let mut ctx = hook_ctx();

        let err = shutdown_hook_add(&mut ctx, None).expect_err("null must be refused");
        match runtime_error(err) {
            RuntimeError::NullPointerException { message } => assert_eq!(
                message.as_deref(),
                Some(HOOK_MSG_NULL_ADD),
                "add's NPE must carry HotSpot's helpful message verbatim"
            ),
            other => panic!("expected NullPointerException, got {other:?}"),
        }
        assert_eq!(hook_count(), 0, "a refused hook must not be registered");

        reset_registry();
    }

    /// `remove`'s NPE is a bare `new NullPointerException()` (pc 20), so its
    /// `getMessage()` is null — not the empty string, and not `add`'s helpful
    /// text. MEASURED: `NULL-REMOVE threw=java.lang.NullPointerException
    /// msg=null`.
    #[test]
    fn null_remove_throws_npe_with_no_message_at_all() {
        let _g = hook_registry_guard();
        reset_registry();
        let mut ctx = hook_ctx();

        let err = shutdown_hook_remove(&mut ctx, None).expect_err("null must be refused");
        match runtime_error(err) {
            // `None`, NOT `Some("")`. `msg=null` and `msg=` are different
            // cells and the differential prints them differently.
            RuntimeError::NullPointerException { message } => assert_eq!(
                message, None,
                "remove's NPE is a no-arg `new NullPointerException()` (pc 20)"
            ),
            other => panic!("expected NullPointerException, got {other:?}"),
        }

        reset_registry();
    }

    /// The JDK tests `hooks == null` at pc 0, BEFORE it touches the argument
    /// at pc 16/17. So a null hook offered during shutdown gets the ISE, not
    /// the NPE — on both methods. This is why the null check lives inside
    /// `shutdown_hook_add`/`_remove` rather than at the registration site.
    #[test]
    fn shutdown_in_progress_outranks_the_null_check() {
        let _g = hook_registry_guard();
        reset_registry();
        SHUTDOWN_STATE.with(HOOK_TEST_VM, |s| s.in_progress = true);
        let mut ctx = hook_ctx();

        let added = shutdown_hook_add(&mut ctx, None).expect_err("must refuse");
        match runtime_error(added) {
            RuntimeError::IllegalStateException { message } => {
                assert_eq!(message, HOOK_MSG_SHUTDOWN_IN_PROGRESS)
            }
            other => panic!("add during shutdown must answer ISE first, got {other:?}"),
        }

        let removed = shutdown_hook_remove(&mut ctx, None).expect_err("must refuse");
        match runtime_error(removed) {
            RuntimeError::IllegalStateException { message } => {
                assert_eq!(message, HOOK_MSG_SHUTDOWN_IN_PROGRESS)
            }
            other => panic!("remove during shutdown must answer ISE first, got {other:?}"),
        }

        reset_registry();
    }

    /// The positive path plus the duplicate refusal, in one test because the
    /// second only means anything after the first: registering twice must
    /// throw, and the registry must still hold exactly one entry afterwards.
    /// The two `remove` rows are `RShutdownHooks`' first two checks, MEASURED
    /// on the oracle as `REMOVE-1 true` / `REMOVE-2 false`.
    #[test]
    fn a_second_registration_of_the_same_hook_throws_and_does_not_grow_the_list() {
        let _g = hook_registry_guard();
        reset_registry();
        let mut ctx = hook_ctx();
        let hook = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);

        shutdown_hook_add(&mut ctx, Some(hook)).expect("first registration must be accepted");
        assert_eq!(hook_count(), 1);

        let err = shutdown_hook_add(&mut ctx, Some(hook)).expect_err("duplicate must be refused");
        match runtime_error(err) {
            RuntimeError::IllegalArgumentException { message } => {
                assert_eq!(message, HOOK_MSG_PREVIOUSLY_REGISTERED)
            }
            other => panic!("expected the duplicate IAE, got {other:?}"),
        }
        assert_eq!(
            hook_count(),
            1,
            "a refused registration must not leave a second entry"
        );

        assert!(shutdown_hook_remove(&mut ctx, Some(hook)).expect("must not throw"));
        assert!(!shutdown_hook_remove(&mut ctx, Some(hook)).expect("must not throw"));
        assert_eq!(hook_count(), 0);

        reset_registry();
    }

    /// gc-common w16-e: the registry is per VM. A hook one VM registers is not
    /// in another VM's list, one VM's shutdown does not refuse another VM's
    /// registration, and teardown drops the VM's row.
    #[test]
    fn one_vms_shutdown_neither_blocks_nor_sees_another_vms_registry() {
        const OTHER_VM: usize = 0xE16_0302;
        let _g = hook_registry_guard();
        reset_registry();
        SHUTDOWN_STATE.forget(OTHER_VM);
        let mut a = hook_ctx();
        let mut b = mock_ctx();
        b.set_vm_identity(OTHER_VM);

        let hook_a = a.alloc_object(cratonvm_types::ClassId::new(0), 4);
        shutdown_hook_add(&mut a, Some(hook_a)).expect("VM A registers its hook");
        assert_eq!(hook_count(), 1);
        assert!(
            registered_shutdown_hooks(OTHER_VM).is_empty(),
            "VM A's hook must not be in VM B's list"
        );

        // VM A's shutdown begins.
        SHUTDOWN_STATE.with(HOOK_TEST_VM, |s| s.in_progress = true);
        let refused = shutdown_hook_add(&mut a, None).expect_err("A is shutting down");
        assert!(matches!(
            runtime_error(refused),
            RuntimeError::IllegalStateException { .. }
        ));
        let hook_b = b.alloc_object(cratonvm_types::ClassId::new(0), 4);
        shutdown_hook_add(&mut b, Some(hook_b))
            .expect("VM A's shutdown must not refuse VM B's registration");
        assert_eq!(registered_shutdown_hooks(OTHER_VM).len(), 1);

        // Teardown drops the row.
        forget_vm_system_singletons(OTHER_VM);
        assert!(registered_shutdown_hooks(OTHER_VM).is_empty());
        assert!(!SHUTDOWN_STATE.has_row(OTHER_VM));

        reset_registry();
    }
}

// ---------------------------------------------------------------------------
// Monotonic clock origins (`System.nanoTime`, `Perf.highResCounter`)
// ---------------------------------------------------------------------------
#[cfg(test)]
mod monotonic_clock_tests {
    use super::*;
    use crate::test_utils::mock_ctx;

    fn nano_time() -> i64 {
        let mut ctx = mock_ctx();
        match native_system_nano_time(&mut ctx, &[]) {
            Ok(Some(Value::Long(v))) => v,
            other => panic!("nanoTime must answer a long, got {other:?}"),
        }
    }

    fn high_res_counter() -> i64 {
        let mut ctx = mock_ctx();
        match native_perf_high_res_counter(&mut ctx, &[]) {
            Ok(Some(Value::Long(v))) => v,
            other => panic!("highResCounter must answer a long, got {other:?}"),
        }
    }

    /// The clock never reads zero, INCLUDING on its very first call.
    ///
    /// This is the guard on `MONOTONIC_ORIGIN_NANOS`. Without the offset the
    /// first reading in a process is the gap between `Instant::now()` and the
    /// next statement, which on Windows is below the clock's resolution — so
    /// it came back as exactly 0 about half the time, disarming every
    /// `if (started == 0) started = System.nanoTime();` timer in Java.
    ///
    /// The assertion deliberately does not depend on being the first caller:
    /// `START` is a process-wide `OnceLock` that a sibling test may already
    /// have initialised, and the whole point of a fixed origin is that the
    /// answer is positive either way.
    #[test]
    fn nano_time_is_never_zero() {
        let t = nano_time();
        assert!(
            t >= MONOTONIC_ORIGIN_NANOS,
            "nanoTime must start at a non-zero origin, got {t}"
        );
    }

    /// `jdk.internal.perf.Perf.highResCounter` had the identical lazy-origin
    /// defect and now carries the identical guarantee.
    #[test]
    fn high_res_counter_is_never_zero() {
        let t = high_res_counter();
        assert!(
            t >= MONOTONIC_ORIGIN_NANOS,
            "highResCounter must start at a non-zero origin, got {t}"
        );
    }

    /// A constant offset must not cost monotonicity.
    #[test]
    fn nano_time_is_monotonic() {
        let t1 = nano_time();
        let t2 = nano_time();
        assert!(t2 >= t1, "nanoTime went backwards: {t1} then {t2}");
    }

    #[test]
    fn high_res_counter_is_monotonic() {
        let t1 = high_res_counter();
        let t2 = high_res_counter();
        assert!(t2 >= t1, "highResCounter went backwards: {t1} then {t2}");
    }
}

// ---------------------------------------------------------------------------
// gc-common w16-e: per-VM `Runtime.version()` singleton handle
// ---------------------------------------------------------------------------
#[cfg(test)]
mod w16e_runtime_version_root_tests {
    use super::*;
    use crate::test_utils::mock_ctx;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    /// Each VM keeps its own singleton, and teardown drops the row. Under the
    /// old process-wide slot, VM B's publish overwrote the one handle with a
    /// slot number of B's table, and VM A then resolved that number in ITS
    /// table (here: nothing, so A lost its singleton).
    #[test]
    fn each_vm_keeps_its_own_runtime_version_singleton() {
        const VM_A: usize = 0xE16_0311;
        const VM_B: usize = 0xE16_0312;
        RUNTIME_VERSION_ROOT.forget(VM_A);
        RUNTIME_VERSION_ROOT.forget(VM_B);
        let mut a = mock_ctx();
        a.set_vm_identity(VM_A);
        let mut b = mock_ctx();
        b.set_vm_identity(VM_B);
        // Offset B's handle numbering, so the two VMs' first singleton
        // handles are different numbers.
        let filler = b.fresh_object_ref();
        let _ = b.add_global_root(filler);

        let version_a = a.fresh_object_ref();
        let version_b = b.fresh_object_ref();
        assert_eq!(runtime_version_publish(&mut a, version_a), version_a);
        assert_eq!(runtime_version_publish(&mut b, version_b), version_b);
        assert_eq!(runtime_version_cached(&a), Some(version_a));
        assert_eq!(runtime_version_cached(&b), Some(version_b));
        // A second publish in a VM answers that VM's existing singleton.
        let another = a.fresh_object_ref();
        assert_eq!(runtime_version_publish(&mut a, another), version_a);

        forget_vm_system_singletons(VM_A);
        assert_eq!(runtime_version_cached(&a), None, "teardown drops A's row");
        assert_eq!(runtime_version_cached(&b), Some(version_b), "and only A's");
        forget_vm_system_singletons(VM_B);
    }
}

// ---------------------------------------------------------------------------
// gci i1/wr: the logging pre-warm arming is per VM
// ---------------------------------------------------------------------------
#[cfg(test)]
mod gci_wr_logging_prewarm_arming_tests {
    use super::*;

    /// `gch-h1ak-first-thread-start-initialises-java-util-logging-FIXED-20261012`:
    /// under the old process-wide `AtomicBool`, VM A's first thread start
    /// consumed VM B's arming. Now each VM's arming is its own, taken exactly
    /// once, and a never-armed VM (or identity 0, the mocks') is never armed.
    #[test]
    fn gci_wr_each_vm_takes_only_its_own_prewarm_arming_once() {
        const VM_A: usize = 0x6C1_0A01;
        const VM_B: usize = 0x6C1_0A02;
        const VM_C: usize = 0x6C1_0A03;
        for vm in [VM_A, VM_B, VM_C] {
            LOGGING_PREWARM_ARMED.forget(vm);
        }
        arm_logging_prewarm_on_first_thread_start(VM_A);
        arm_logging_prewarm_on_first_thread_start(VM_B);
        arm_logging_prewarm_on_first_thread_start(0);
        assert!(take_logging_prewarm_arming(VM_A), "A's first start runs the pre-warm");
        assert!(!take_logging_prewarm_arming(VM_A), "once per VM");
        assert!(take_logging_prewarm_arming(VM_B), "A did not consume B's arming");
        assert!(!take_logging_prewarm_arming(VM_C), "C was never armed");
        assert!(!LOGGING_PREWARM_ARMED.has_row(VM_C), "and the ask left no row");
        assert!(!take_logging_prewarm_arming(0), "identity 0 is never armed");
        arm_logging_prewarm_on_first_thread_start(VM_A);
        forget_vm_system_singletons(VM_A);
        assert!(!take_logging_prewarm_arming(VM_A), "teardown drops the arming");
        forget_vm_system_singletons(VM_B);
    }
}

/// `Thread.sleep(J)V` plus the `jdk.ThreadSleep` the JDK's own `Thread.sleep`
/// commits (SBR5 lane jfr; the native shadows that bytecode in `--compatible`).
/// Zero cost without a recording: one relaxed load.
pub(crate) fn native_thread_sleep_jfr(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if !crate::jfr_vm_events::any_recording() {
        return native_thread_sleep(ctx, args);
    }
    let millis = match args.first() {
        Some(Value::Long(ms)) => *ms,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Int(i)) => i64::from(*i),
        _ => 0,
    };
    let root = crate::jfr_vm_events::thread_sleep_begin(ctx, millis.saturating_mul(1_000_000));
    let result = native_thread_sleep(ctx, args);
    crate::jfr_vm_events::thread_sleep_end(ctx, root);
    result
}

/// `Thread.sleep(JI)V` with the same event.
pub(crate) fn native_thread_sleep_millis_nanos_jfr(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if !crate::jfr_vm_events::any_recording() {
        return native_thread_sleep_millis_nanos(ctx, args);
    }
    let millis = match args.first() {
        Some(Value::Long(ms)) => *ms,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Int(i)) => i64::from(*i),
        _ => 0,
    };
    let nanos = match args.get(1) {
        Some(Value::Int(n)) => i64::from(*n),
        _ => 0,
    };
    let root = crate::jfr_vm_events::thread_sleep_begin(
        ctx,
        millis.saturating_mul(1_000_000).saturating_add(nanos),
    );
    let result = native_thread_sleep_millis_nanos(ctx, args);
    crate::jfr_vm_events::thread_sleep_end(ctx, root);
    result
}

/// Interpreter round i2 wave 68, lane L1 (`CRATONVM_STAND_IN_WAIT_OBJECTS`):
/// a served join's region publishes the joined thread for the `wait` loop
/// kinds only.
#[cfg(test)]
mod i68_join_region_tests {
    use super::begin_join_region;
    use crate::test_utils::MockNativeContext;
    use cratonvm_native_api::{BlockKind, NativeHeapAccess, NativeThreadAccess};
    use cratonvm_types::ClassId;

    #[test]
    fn a_platform_join_publishes_the_target_and_a_virtual_one_does_not() {
        let mut ctx = MockNativeContext::new();
        let target = ctx.alloc_object(ClassId::new(0), 1);
        begin_join_region(&mut ctx, target, BlockKind::TimedObjectWait);
        ctx.end_blocking_region();
        begin_join_region(&mut ctx, target, BlockKind::Parked);
        ctx.end_blocking_region();
        assert_eq!(
            *ctx.region_wait_objects.borrow(),
            vec![(target, BlockKind::TimedObjectWait)]
        );
        assert_eq!(ctx.blocking_region_counts(), (2, 2));
    }
}

/// gci i11/ja (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
/// item 2): with `CRATONVM_EXIT_HOOK_GC_SAFE` the exit natives run the
/// pre-exit hook inside ONE blocking region on their own context; off, they
/// open none (the default path, unchanged).
#[cfg(test)]
mod gci_i11ja_exit_hook_region_tests {
    use super::invoke_pre_exit_hook_from;
    use crate::test_utils::MockNativeContext;

    #[test]
    fn gci_i11ja_the_exit_hook_runs_in_the_natives_own_region() {
        let mut ctx = MockNativeContext::new();
        cratonvm_types::flags::with_thread_overrides(&[("CRATONVM_EXIT_HOOK_GC_SAFE", None)], || {
            invoke_pre_exit_hook_from(&mut ctx, 3);
        });
        assert_eq!(ctx.blocking_region_counts(), (0, 0), "off: no region");
        cratonvm_types::flags::with_thread_overrides(&[("CRATONVM_EXIT_HOOK_GC_SAFE", Some("1"))], || {
            invoke_pre_exit_hook_from(&mut ctx, 3);
        });
        assert_eq!(ctx.blocking_region_counts(), (1, 1), "on: one paired region");
    }

    /// zt t12/tg5 (`zgc-zt11tg4-*` item 4, `CRATONVM_ZGC_TICK_EXIT_NATIVE_WAITS`):
    /// the exit natives' helper asks its context to stop the ZGC director
    /// (and wait for an in-flight tick pause) once, BEFORE the pre-exit hook
    /// and outside its region, on both arms of `CRATONVM_EXIT_HOOK_GC_SAFE`;
    /// the `=0` kill asks nothing.
    #[test]
    fn zt12tg5_the_exit_helper_stops_the_director_before_the_hook() {
        use crate::test_utils::mock_exit_director_stops;
        let _ = mock_exit_director_stops();
        for gc_safe in [None, Some("1")] {
            let mut ctx = MockNativeContext::new();
            cratonvm_types::flags::with_thread_overrides(
                &[
                    ("CRATONVM_EXIT_HOOK_GC_SAFE", gc_safe),
                    ("CRATONVM_ZGC_TICK_EXIT_NATIVE_WAITS", None),
                ],
                || invoke_pre_exit_hook_from(&mut ctx, 3),
            );
            assert_eq!(mock_exit_director_stops(), vec![0], "gc_safe={gc_safe:?}");
        }
        let mut ctx = MockNativeContext::new();
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_ZGC_TICK_EXIT_NATIVE_WAITS", Some("0"))],
            || invoke_pre_exit_hook_from(&mut ctx, 3),
        );
        assert!(mock_exit_director_stops().is_empty(), "kill: not asked");
    }

    /// Source pin: all three exit natives call the hook through the helper.
    #[test]
    fn gci_i11ja_every_exit_native_uses_the_helper() {
        let src = include_str!("lang_system.rs").replace("\r\n", "\n");
        for f in [
            "pub(crate) fn native_system_exit(",
            "pub(crate) fn native_runtime_exit(",
            "pub(crate) fn native_shutdown_halt0(",
        ] {
            let start = src.find(f).unwrap_or_else(|| panic!("{f}"));
            let rest = &src[start..];
            let body = &rest[..rest.find("\n}\n").expect("its end")];
            assert!(body.contains("invoke_pre_exit_hook_from(ctx, code);"), "{f}");
            assert!(!body.contains("    invoke_pre_exit_hook(code);"), "{f}");
        }
    }
}

/// gci i3/jd: `defineClass2` of a DIRECT buffer reads `address + offset`,
/// where `offset` is the caller's `b.position()` (JDK 25
/// `ClassLoader.defineClass(String, ByteBuffer, ProtectionDomain)`), as
/// HotSpot's native does; it used to add the position a second time
/// (`docs/feature-designs/native-dispatch-architecture.md`).
#[cfg(test)]
mod gci_i3jd_define_class2_direct_tests {
    use super::read_byte_buffer_define_class_slice;
    use crate::test_utils::{mock_ctx, MockNativeContext};
    use cratonvm_native_api::{FieldMetadata, NativeHeapAccess};
    use cratonvm_types::{ClassId, ObjectRef, Value};

    /// A class id no other mock object uses, declared with the direct
    /// buffer's fields so `bb_define_layout` and `get_field_by_name("address")`
    /// resolve them as on a real `DirectByteBuffer`.
    const DIRECT_BB: u32 = 0x6A_D3;

    fn direct_buffer(
        ctx: &mut MockNativeContext,
        backing: &[u8],
        position: i32,
        limit: i32,
    ) -> ObjectRef {
        let cid = ClassId::new(DIRECT_BB);
        let field = |name: &str, descriptor: &str, slot: usize| FieldMetadata {
            name: name.to_string(),
            descriptor: descriptor.to_string(),
            access_flags: 0,
            slot_index: slot,
            declaring_class_id: cid,
            is_static: false,
        };
        ctx.set_declared_fields(
            cid,
            vec![
                field("hb", "[B", 0),
                field("position", "I", 1),
                field("limit", "I", 2),
                field("capacity", "I", 3),
                field("address", "J", 4),
            ],
        );
        let bb = ctx.alloc_object(cid, 5);
        // `hb` stays null: a direct buffer has no backing array.
        ctx.set_field(bb, 0, Value::Object(None));
        ctx.set_field(bb, 1, Value::Int(position));
        ctx.set_field(bb, 2, Value::Int(limit));
        ctx.set_field(bb, 3, Value::Int(backing.len() as i32));
        ctx.set_field(bb, 4, Value::Long(backing.as_ptr() as usize as i64));
        bb
    }

    const CLASS_BYTES: [u8; 6] = [0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x01];

    #[test]
    fn a_direct_buffer_advanced_past_a_prefix_reads_from_its_position() {
        let mut ctx = mock_ctx();
        let mut backing = vec![0x5Au8; 8];
        backing.extend_from_slice(&CLASS_BYTES);
        let bb = direct_buffer(&mut ctx, &backing, 8, backing.len() as i32);
        // What the JDK passes: offset = b.position(), length = b.remaining().
        let bytes = read_byte_buffer_define_class_slice(&ctx, bb, 8, CLASS_BYTES.len(), "p/X")
            .expect("the class bytes at the buffer's position");
        assert_eq!(bytes, CLASS_BYTES.to_vec());
    }

    #[test]
    fn a_direct_buffer_at_position_zero_reads_from_its_address() {
        let mut ctx = mock_ctx();
        let backing = CLASS_BYTES.to_vec();
        let bb = direct_buffer(&mut ctx, &backing, 0, backing.len() as i32);
        let bytes = read_byte_buffer_define_class_slice(&ctx, bb, 0, CLASS_BYTES.len(), "p/X")
            .expect("the whole buffer");
        assert_eq!(bytes, CLASS_BYTES.to_vec());
    }

    #[test]
    fn a_direct_slice_past_the_limit_is_refused() {
        let mut ctx = mock_ctx();
        let mut backing = vec![0x5Au8; 8];
        backing.extend_from_slice(&CLASS_BYTES);
        // Limit one byte short of the class.
        let bb = direct_buffer(&mut ctx, &backing, 8, backing.len() as i32 - 1);
        let r = read_byte_buffer_define_class_slice(&ctx, bb, 8, CLASS_BYTES.len(), "p/X");
        assert!(r.is_err(), "a slice past the limit must not be read: {r:?}");
    }
}

/// gci i12/td: `jdk.internal.vm.ThreadSnapshot.create` (the JDK 25 thread
/// dumper's per-thread snapshot).
#[cfg(test)]
mod gci_i12td_thread_snapshot_tests {
    use super::*;

    /// `jdk.internal.misc.VM.toThreadState`, as JDK 25 writes it.
    fn to_thread_state(status: i32) -> i32 {
        if status & 0x0004 != 0 {
            1
        } else if status & 0x0400 != 0 {
            2
        } else if status & 0x0010 != 0 {
            3
        } else if status & 0x0020 != 0 {
            4
        } else if status & 0x0002 != 0 {
            5
        } else if status & 0x0001 == 0 {
            0
        } else {
            1
        }
    }

    #[test]
    fn gci_i12td_snapshot_status_decodes_to_the_same_state() {
        for ordinal in 0..=5 {
            assert_eq!(to_thread_state(thread_snapshot_status(ordinal)), ordinal, "ordinal {ordinal}");
        }
    }

    #[test]
    fn gci_i12td_snapshot_blocker_follows_hotspots_closure() {
        // BLOCKED: the monitor being entered.
        assert_eq!(thread_snapshot_blocker(2, false, true), Some(BLOCKER_WAITING_TO_LOCK));
        // Parked: the park blocker, ahead of any monitor.
        assert_eq!(thread_snapshot_blocker(3, true, false), Some(BLOCKER_PARK));
        assert_eq!(thread_snapshot_blocker(4, true, true), Some(BLOCKER_PARK));
        // `Object.wait`: the monitor waited on.
        assert_eq!(thread_snapshot_blocker(3, false, true), Some(BLOCKER_WAITING_ON));
        assert_eq!(thread_snapshot_blocker(4, false, true), Some(BLOCKER_WAITING_ON));
        // Sleeping, running, or a park with no blocker: none.
        assert_eq!(thread_snapshot_blocker(4, false, false), None);
        assert_eq!(thread_snapshot_blocker(1, true, true), None);
    }

    #[test]
    fn gci_i12td_thread_snapshot_create_is_a_bridge_on_the_real_jdk_path() {
        let mut registry = NativeMethodRegistry::new();
        register_runtime_natives(&mut registry);
        assert_eq!(
            registry.kind_of(
                "jdk/internal/vm/ThreadSnapshot",
                "create",
                "(Ljava/lang/Thread;)Ljdk/internal/vm/ThreadSnapshot;"
            ),
            Some(NativeKind::Bridge)
        );
    }
}

/// JIT round 21 wave 26, lane nesting (`CRATONVM_DEFINE_SUPERTYPE_MISS_CAUSE`).
#[cfg(test)]
mod r21w26_supertype_miss_cause_tests {
    use super::*;
    use crate::test_utils::mock_ctx;

    fn ncdfe(name: &str) -> MethodCallFailed {
        LinkageError::NoClassDefFoundError {
            class_name: name.to_string(),
        }
        .into()
    }

    fn ncdfe_name(failed: &MethodCallFailed) -> Option<&str> {
        match failed {
            MethodCallFailed::InternalError(cratonvm_types::error::VmError::Linkage(
                LinkageError::NoClassDefFoundError { class_name },
            )) => Some(class_name.as_str()),
            _ => None,
        }
    }

    /// No recorded miss, or a miss of another supertype: the define's error
    /// is handed back unchanged (no allocation, no new throwable).
    #[test]
    fn only_the_missed_supertypes_error_gets_a_cause() {
        let mut ctx = mock_ctx();
        let none = SupertypeAnswers::default();
        let back = supertype_miss_with_cause(&mut ctx, ncdfe("p/Gone"), &none);
        assert_eq!(ncdfe_name(&back), Some("p/Gone"));

        let other = SupertypeAnswers {
            miss_cause: Some((
                "p/Other".to_string(),
                "java/lang/ClassNotFoundException".to_string(),
                Some("p.Other".to_string()),
            )),
            ..SupertypeAnswers::default()
        };
        let back = supertype_miss_with_cause(&mut ctx, ncdfe("p/Gone"), &other);
        assert_eq!(ncdfe_name(&back), Some("p/Gone"));
        // Not a NoClassDefFoundError at all.
        let cfe: MethodCallFailed = LinkageError::ClassCircularityError {
            class_name: "p/Other".to_string(),
        }
        .into();
        let back = supertype_miss_with_cause(&mut ctx, cfe, &other);
        assert!(ncdfe_name(&back).is_none());
    }

    /// JIT round 22 wave 1, lane review
    /// (`CRATONVM_DEFINE_SUPERTYPE_MISS_CAUSE_ORIGINAL`): the define natives'
    /// answers keep the loader's own object under a global root, hand it out
    /// as the cause, and release it; any other user of the answers roots
    /// nothing.
    #[test]
    fn the_loaders_own_exception_is_kept_and_released() {
        use cratonvm_native_api::NativeHeapAccess;
        let mut ctx = mock_ctx();
        let thrown = ctx.create_string("stands in for the loader's ClassNotFoundException");
        let mut kept = SupertypeAnswers {
            keep_miss_object: true,
            ..SupertypeAnswers::default()
        };
        kept.miss_root = ctx.add_global_root(thrown);
        assert_eq!(kept.miss_object(&ctx), Some(thrown));
        kept.release_miss_root(&mut ctx);
        assert_eq!(kept.miss_root, 0);
        assert_eq!(kept.miss_object(&ctx), None, "released");
        kept.release_miss_root(&mut ctx); // idempotent

        let plain = SupertypeAnswers::default();
        assert!(!plain.keep_miss_object, "Lookup.defineClass's answers root nothing");
        assert_eq!(plain.miss_object(&ctx), None);
    }
}
