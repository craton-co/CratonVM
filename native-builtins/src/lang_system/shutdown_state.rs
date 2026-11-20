#[allow(unused_imports)]
use super::*;


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
pub struct ShutdownState {
    /// Global-root handles of the registered hook `Thread`s, in THIS VM's
    /// global-root table.
    pub hooks: Vec<usize>,
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
    pub in_progress: bool,
    /// Set the first time [`run_jdk_shutdown_slots`] walks
    /// `java.lang.Shutdown`'s slot array, so a second entry into shutdown (a
    /// `System.exit` from inside a hook, or `Shutdown.runHooks` after the
    /// launcher's own drain) cannot run the JDK's hooks twice. The hook list
    /// needs no such flag: it is drained with `mem::take`, and an empty list is
    /// its own guard.
    pub jdk_slots_ran: bool,
    /// The thread that began shutdown (`Shutdown.exit` holds the monitor of
    /// `java.lang.Shutdown` for the whole of it). A DIFFERENT thread calling
    /// `System.exit` / `Runtime.exit` meanwhile blocks forever on HotSpot (SBR5
    /// sysprops, `Sbr5SyspropsExit nested`: a hook that starts a thread calling
    /// `System.exit(2)` printed `hook-done` and the process left with rc 0; this VM's
    /// native exit left at once with rc 2). The same thread re-entering is a
    /// recursive monitor enter and proceeds.
    pub exiting_thread: Option<u64>,
}

/// Claim the right to run shutdown for the calling thread, or block forever.
///
/// The `--compatible` twin of `Shutdown.exit`'s `synchronized (Shutdown.class)`: under
/// `--real-jdk` `System.exit` runs the JDK's own bytecode and this is never reached.
pub fn claim_exit_or_block_forever(ctx: &mut dyn NativeContext) {
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
    {
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
pub static SHUTDOWN_STATE: VmScoped<ShutdownState> = VmScoped::new();

/// Has `vm`'s shutdown begun?
pub fn shutdown_in_progress(vm: usize) -> bool {
    SHUTDOWN_STATE.peek(vm, |s| s.in_progress).unwrap_or(false)
}

/// A copy of `vm`'s registered hook handles, taken so the caller can resolve
/// them with the table lock released.
pub fn registered_shutdown_hooks(vm: usize) -> Vec<usize> {
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
pub const HOOK_MSG_SHUTDOWN_IN_PROGRESS: &str = "Shutdown in progress";

/// NOT an explicit check — this is HotSpot's helpful-NPE rendering of the
/// `invokevirtual java/lang/Thread.isAlive` at `ApplicationShutdownHooks.add`
/// pc 17, which is the first thing that touches the argument. MEASURED
/// (`HookProbe dup`, `NULL-ADD`). The quoted `hook` is `add`'s parameter name.
pub const HOOK_MSG_NULL_ADD: &str =
    "Cannot invoke \"java.lang.Thread.isAlive()\" because \"hook\" is null";

/// `ApplicationShutdownHooks.add` pc 27. MEASURED (`HookProbe dup`,
/// `RUNNING-ADD`) by registering a `Thread` that had been `start()`ed and was
/// parked on a latch.
pub const HOOK_MSG_ALREADY_RUNNING: &str = "Hook already running";

/// `ApplicationShutdownHooks.add` pc 47. MEASURED (`HookProbe dup`,
/// `DUP-ADD`).
pub const HOOK_MSG_PREVIOUSLY_REGISTERED: &str = "Hook previously registered";

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
pub fn shutdown_hook_add(
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
pub fn shutdown_hook_remove(
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
pub fn shutdown_hook_argument(args: &[Value], which: &str) -> Option<Option<ObjectRef>> {
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
pub fn flush_console_streams(ctx: &dyn NativeContext) {
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
pub fn shutdown_hook_join_bound(ctx: &dyn NativeContext) -> Option<std::time::Duration> {
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
pub fn thread_already_started(ctx: &mut dyn NativeContext, thread: ObjectRef) -> bool {
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
pub fn capture_inheritable_tl_at_construction(
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
pub fn capture_inheritable_tl_at_construction_body(
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
pub fn inheritable_tl_captured_at_construction(
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
/// wave 17, `L4FileTempExit.java`): with `java.io.File.deleteOnExit`
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
pub fn run_jdk_shutdown_slots(ctx: &mut dyn NativeContext) -> (usize, usize) {
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
/// retired under `--real-jdk`)? Then the hooks, the console-restore slot and the
/// `deleteOnExit` slot all live in `java.lang.Shutdown`/`ApplicationShutdownHooks`, and the
/// launcher drives them the way HotSpot's `DestroyJavaVM` does: `Shutdown.shutdown()`.
pub fn jdk_runs_the_shutdown_hooks(ctx: &dyn NativeContext) -> bool {
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
        false,
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
pub fn report_vector_intrinsics() {
    let (handled, fell_back) = crate::vector_support_intrinsics::vector_intrinsic_counts();
    let asked = Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
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
pub fn report_filechannel_fast_io() {
    let asked = Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
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
    false}

pub type PreExitHook = fn(code: i32);
pub static PRE_EXIT_HOOK: std::sync::OnceLock<PreExitHook> = std::sync::OnceLock::new();

/// Install a pre-`std::process::exit` callback that fires from
/// `native_system_exit` / `native_runtime_exit` immediately before the
/// process is torn down. Intended for diagnostic dumps (dispatch_trace
/// ring, last-N-bytecodes printout). First-installer wins вЂ” subsequent
/// calls are silent no-ops, matching `OnceLock` semantics.
pub fn set_pre_exit_hook(hook: PreExitHook) {
    let _ = PRE_EXIT_HOOK.set(hook);
}

pub fn invoke_pre_exit_hook(code: i32) {
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
pub fn invoke_pre_exit_hook_from(ctx: &mut dyn NativeContext, code: i32) {
    // zt t12/tg5 (`zgc-zt11tg4-*` item 4, `CRATONVM_ZGC_TICK_EXIT_NATIVE_WAITS`,
    // default ON, `=0` kill): HotSpot's `before_exit` stops its ZGC director
    // and waits for a running cycle on every exit path; the launcher's
    // main-thread arms did, the exit natives' arm only stopped (in the hook,
    // whose thread state is not known there). Here the context is this
    // thread's own, so the wait can be GC-safe. Before the hook and outside
    // its region; a no-op off ZGC and without a tick.
    {
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
pub fn exit_hook_gc_safe() -> bool {
    false}



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
/// `pub` since 2026-08-12, and the reason is the third exit door.
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
pub fn sweep_declared_slot_maps_before_exit(ctx: &dyn NativeContext, trigger: &str) {
    if cratonvm_native_api::layout_alias::enabled() {
        let _ = cratonvm_native_api::read_alias::sweep_declared_slot_maps_at(ctx, trigger);
    }
}

// ---------------------------------------------------------------------------
// java.lang.System natives
// ---------------------------------------------------------------------------

pub fn native_system_identity_hash_code(
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
pub fn native_system_identity_hash_code_leaf(
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
pub fn identity_hash_leaf_enabled() -> bool {
    true
}

#[inline]
pub fn current_time_millis_raw() -> i64 {
    #[cfg(unix)]
    {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: valid initialized pointer on stack.
        unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut ts) };
        (ts.tv_sec as i64) * 1_000 + (ts.tv_nsec as i64) / 1_000_000
    }
    #[cfg(not(unix))]
    {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64
    }
}

pub fn native_system_current_time_millis(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Long(current_time_millis_raw())))
}

/// HotSpot's `Klass::external_name()` for an object's class: dotted, and an
/// array rendered as `component[]` rather than as its `[L…;` descriptor.
///
/// Used only on `arraycopy`'s cold error paths, where the message names the
/// offending class the way a Java stack trace would.
pub fn external_class_name_of(ctx: &mut dyn NativeContext, obj: ObjectRef) -> String {
    let class_id = ctx.class_id_of_object(obj);
    let internal = ctx.class_name_of_id(class_id).unwrap_or_default();
    external_class_name(&internal)
}

/// Descriptor -> HotSpot external name. `[Ljava/lang/String;` ->
/// `java.lang.String[]`, `[I` -> `int[]`, `java/lang/String` ->
/// `java.lang.String`.
pub fn external_class_name(internal: &str) -> String {
    cratonvm_types::error::arraycopy_message::external_class_name(internal)
}

/// HotSpot's per-element `ArrayStoreException` text for a reference copy whose
/// source holds an element the destination component type cannot accept.
///
/// It names the source ARRAY and the destination COMPONENT — not the offending
/// index, which is why the previous "source element at index N" wording could
/// not have come from HotSpot.
pub fn element_type_mismatch_message(
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
    // 29, lane L4; `L4W29ArraycopyArrayElements.java`).
    // Only the wording depends on it: the copy is element by element either
    // way, so the prefix before the refused element is committed in both.
    let src_component_id = ctx.class_id_of_object(src);
    // JIT round 21 wave 26 (lane vhase; `CRATONVM_ARRAYCOPY_HIDDEN_COMPONENT_TEXT`):
    // a HIDDEN component cannot be looked up by name, so the name rule below
    // answered "unknown, element mismatch" for `String[] -> H[]`, and the
    // sentence dotted the hidden class's `/0x<hex>` tail. With either
    // component hidden, decide on class ids and render each component by its
    // class's own hidden flag.
    let hidden = ctx.is_class_hidden(src_component_id) || ctx.is_class_hidden(dst_elem_class);
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



/// JVMS §4.10.1.2 subtyping between two reference COMPONENT types given by
/// internal name, for `element_type_mismatch_message`'s choice of wording:
/// arrays are covariant in reference components, identical in primitive ones,
/// and subtypes of `Object`, `Cloneable` and `Serializable`. Two plain classes
/// are asked of the class table by name; a name it does not know answers
/// `true`, which keeps the historical "element type mismatch" sentence.
pub fn arraycopy_component_is_subtype(ctx: &mut dyn NativeContext, sub: &str, sup: &str) -> bool {
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

pub fn native_system_arraycopy(
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
        && false
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
                    true
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
            // `L4W29ArraycopyArrayElements.java`).
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
            let res = {
                static CHECKED_BULK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
                *CHECKED_BULK.get_or_init(|| {
                    true
                })
            };
            if res {
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

pub fn is_abstract_string_builder_capacity_copy(ctx: &mut dyn NativeContext) -> bool {
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

pub fn is_abstract_string_builder_append_copy(ctx: &mut dyn NativeContext) -> bool {
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

pub fn native_thread_current_thread(
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
pub fn native_thread_sleep_millis_nanos(
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
pub fn sleep_interrupted(
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
        cratonvm_native_api::throwable_builder::construct_new(
            ctx,
            "java/lang/InterruptedException",
            "()V",
            &[],
        )
    } else {
        let msg = ctx.create_string("sleep interrupted");
        cratonvm_native_api::throwable_builder::construct_new(
            ctx,
            "java/lang/InterruptedException",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(msg))],
        )
    };
    if let Ok(Some(exc)) = built {
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

pub fn native_thread_sleep(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    // (`L7W23Interrupts.java`, row `pre sleep0`).
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
pub fn native_thread_get_state(
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
pub fn canonical_enum_constant(
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
pub fn canonical_enum_values(
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

pub fn native_thread_is_alive(
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
pub static LOGGING_PREWARM_ARMED: VmScoped<bool> = VmScoped::new();

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
pub fn take_logging_prewarm_arming(vm_identity: usize) -> bool {
    if vm_identity == 0 || LOGGING_PREWARM_ARMED.peek(vm_identity, |armed| *armed) != Some(true) {
        return false;
    }
    LOGGING_PREWARM_ARMED.with(vm_identity, |armed| std::mem::take(armed))
}

/// Consume the arming, once, and drive the logging first-use path on the
/// calling thread. Best effort, exactly like the launcher's old boot-time call.
/// The name is `global`, a logger the JDK itself registers, so the logger list
/// an application sees is unchanged.
pub fn run_armed_logging_prewarm(ctx: &mut dyn NativeContext, starting: ObjectRef) {
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

pub fn native_thread_start0(
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
    // container route, including the `--real-jdk` case where the real
    // `start()` bytecode runs; a refusal here does not leak a container
    // registration, because that bytecode's `finally` calls
    // `container.onExit(this)`. See W7-27-thread-exit-java-cleanup.md §13.
    if thread_already_started(ctx, this) {
        ctx.unpin_native_roots(this_pin);
        return Err(RuntimeError::IllegalThreadStateException {
            // HotSpot throws the no-arg constructor here: the message is
            // NULL, not a description. Measured.
            message: String::new(),        }
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
    // `Intrinsic` over real bytecode even under `--real-jdk`
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
    // SBR round 2 (lane `thread`): NEVER under `--real-jdk`. There the
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

pub fn native_thread_join(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
/// census frames are then exactly what `--real-jdk` executes (and what HotSpot
/// prints under `-XX:-VMContinuations`), and are kept.
pub fn join_target_runs_join_nanos(ctx: &dyn NativeContext, target: ObjectRef) -> bool {
    let name = ctx.class_name_arc_of_id(ctx.class_id_of_object(target));
    name.as_deref() == Some("java/lang/VirtualThread")
}

/// Did a served join end with the `InterruptedException` of an interrupt?
pub fn is_interrupted_join(result: &MethodCallResult) -> bool {
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
pub fn join_poll_registry_id(ctx: &dyn NativeContext, this: ObjectRef) -> Option<u64> {
    let id = ctx.thread_registry_id(this)?;
    if !ctx.thread_registry_id_is_alive(id) && ctx.thread_is_alive(this) {
        return None;
    }
    Some(id)
}

/// The interruptible untimed join shared by the served `join()` and
/// `join(long)` (the latter through [`join_with_served_hint`]); leaves no hint.
pub fn join_until_dead(ctx: &mut dyn NativeContext, mut this: ObjectRef) -> MethodCallResult {
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
pub fn join_region_kind(
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
pub fn begin_join_region(
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
pub fn leave_join_region(ctx: &mut dyn NativeContext, this: ObjectRef) -> ObjectRef {
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
pub fn join_with_served_hint(
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
pub fn served_join_declines(declines: bool) -> bool {
    declines
}

pub fn native_thread_join_timed(
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
        // Round 14 wave 6 (lane trace5; TR5-3): HotSpot's `join(0)` is
        // interrupted in its `wait(0)` loop.
        return join_with_served_hint(ctx, this, false);
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
pub fn native_thread_join_millis_nanos(
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
pub fn served_two_site_argument_check(
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

pub fn native_thread_interrupt(
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

pub fn native_thread_get_name(
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

pub fn native_thread_is_interrupted(
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

pub fn native_system_get_property(
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

pub fn native_system_get_property_default(
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

pub fn native_system_set_property(
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
pub const MONOTONIC_ORIGIN_NANOS: i64 = 1 << 40;

pub fn native_system_nano_time(
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
    #[cfg(unix)]
    {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: valid initialized pointer on stack.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        MONOTONIC_ORIGIN_NANOS
            + (ts.tv_sec as i64).saturating_mul(1_000_000_000)
            + (ts.tv_nsec as i64)
    }
    #[cfg(not(unix))]
    {
        use std::sync::OnceLock;
        use std::time::Instant;
        static START: OnceLock<Instant> = OnceLock::new();
        let start = START.get_or_init(Instant::now);
        MONOTONIC_ORIGIN_NANOS + start.elapsed().as_nanos() as i64
    }
}

pub fn native_system_exit(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    if exit_census_on() || None::<std::ffi::OsString>.is_some() {
        eprintln!("[cratonvm] System.exit({code}) called вЂ” process terminating");    }
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
/// `SysPropsVmConsumers.java` against HotSpot 25.0.4+7:
///
/// ```text
///                                                  HotSpot   before   after
/// API write to line.separator then lineSeparator   unchanged changed  unchanged
/// OBJECT write to line.separator then lineSeparator unchanged changed unchanged
/// after both writes line.separator restored        true      false*   true
///                                              (* --real-jdk only)
/// ```
///
/// The `--real-jdk` row is the one worth reading twice: an OBJECT write
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
pub fn native_system_line_separator(
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

pub fn register_runtime_natives(registry: &mut NativeMethodRegistry) {
    // gci i12/td: the JDK 25 thread dumper's per-thread snapshot (a genuine
    // `native` declaration, so a Bridge serves it in both modes; there is no
    // bytecode under it). Registered here because this registrar runs on the
    // real-JDK boot path (`register_essential_natives_with_shims`).
    registry.register(
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
        native_runtime_get_runtime, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Runtime",
        "availableProcessors",
        "()I",
        native_runtime_available_processors,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Runtime",
        "maxMemory",
        "()J",
        native_runtime_max_memory,
        NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Runtime",
        "totalMemory",
        "()J",
        native_runtime_total_memory,
        NativeKind::Bridge,
    );
    registry.register(
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
        native_runtime_version, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Runtime$Version",
        "feature",
        "()I",
        native_runtime_version_feature, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Runtime$Version",
        "build",
        "()Ljava/util/Optional;",
        native_runtime_version_build, cratonvm_native_api::NativeKind::Bridge,
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
        }, cratonvm_native_api::NativeKind::Bridge,
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
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "java/lang/Runtime",
        "gc",
        "()V",
        |ctx, _args| {
            // zs s7/jf: on ZGC `jdk.SystemGC` is this call's, as HotSpot's
            // `JVM_GC` commits it (calling thread, its stack, the call's span).
            crate::hooks::jfr_force_gc_as_system_gc(ctx);
            Ok(None)
        },
        NativeKind::Bridge,
    );
    registry.register("java/lang/Runtime", "exit", "(I)V", native_runtime_exit, cratonvm_native_api::NativeKind::Bridge);

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
    registry.register(
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
    registry.register(
        "java/lang/Shutdown",
        "runHooks",
        "()V",
        |ctx, _args| {
            run_shutdown_hooks(ctx, "Shutdown.runHooks");
            Ok(None)
        },
        NativeKind::Bridge,
    );
    registry.register(
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
            }, cratonvm_native_api::NativeKind::Bridge,
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
            }, cratonvm_native_api::NativeKind::Bridge,
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
            }, cratonvm_native_api::NativeKind::Bridge,
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
            }, cratonvm_native_api::NativeKind::Bridge,
        );    } else {
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
            }, cratonvm_native_api::NativeKind::Bridge,
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
            }, cratonvm_native_api::NativeKind::Bridge,
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
            }, cratonvm_native_api::NativeKind::Bridge,
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
            }, cratonvm_native_api::NativeKind::Bridge,
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
/// --real-jdk       no cratonvm_probe_zzz in java.library.path       /  net LOADED
/// --fast-boot       no  in java.library.path                         /  net "no  in java.library.path"
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
pub fn runtime_load_args(ctx: &dyn NativeContext, args: &[Value]) -> (Option<ObjectRef>, String) {
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
pub enum LibrarySpelling {
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
// (the `docs/feature-designs/native-dispatch-architecture.md` loader-identity cluster).
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
// global — real-jdk-mode.md §2 forbids process globals for this feature's
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
pub enum LoaderScoping {
    Off,
    On,
}

/// `loader id -> the library keys that loader has been told it holds`, one row
/// per VM. `BTreeMap`/`BTreeSet` rather than hash maps because the rows are
/// tiny (a JVM loads a handful of JNI libraries) and a deterministic iteration
/// order makes the "which loader owns it" answer reproducible when a key has
/// somehow been recorded twice.
pub static LOADED_LIBRARIES: VmScoped<
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
pub fn claim_library(vm_identity: usize, loader: i32, key: &str) -> Option<i32> {
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
pub fn requesting_loader_id(ctx: &mut dyn NativeContext, from_class: Option<ObjectRef>) -> Option<i32> {
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
pub fn loaded_by(
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
pub fn is_vm_provided_jdk_library(name: &str) -> bool {
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
pub const DYNAMIC_ALREADY_LOADED: &[&str] = &["zip"];

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
pub fn jdk_image_ships_library(ctx: &dyn NativeContext, name: &str) -> bool {
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
/// raise UnsatisfiedLinkError") failed in BOTH `--fast-boot` and `--real-jdk`
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
pub fn load_library_or_throw(
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
    // MEASURED by `SystemRuntimeObjectSweep.java`.
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
    let load_err = match ctx.load_native_library(&target) {
        Ok(lib_index) => {
            let loader_id = jni_library_loader_id(ctx, from_class);
            ctx.set_native_library_loader(lib_index, Some(loader_id));
            return loaded_by(ctx, requested, from_class, scoping);
        }
        Err(err) => err,
    };
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
    if matches!(
        load_err,
        MethodCallFailed::ExceptionThrown(_)
            | MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(RuntimeError::UnsatisfiedLinkError { .. }))
    ) {
        return Err(load_err);
    }
    // NOT memoised as a failure: the JDK re-attempts the lookup on every call
    // (`RJdkFailure.java:269` asserts the second attempt throws too), and a
    // library can legitimately appear on `java.library.path` between calls.
    Err(RuntimeError::UnsatisfiedLinkError {
        message: format!("no {requested} in java.library.path"),
    }
    .into())
}

pub fn native_runtime_get_runtime(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let class_id = match ctx.ensure_class_initialized("java/lang/Runtime") {
        Ok(id) => id,
        // Fallible since 2026-08-10 (Real-JDK wave 2, step 3): `java.lang.Runtime`
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

pub fn native_runtime_available_processors(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Container-aware: respects the cgroup CPU quota under
    // `-XX:+UseContainerSupport` (falls back to the host thread count).
    Ok(Some(Value::Int(ctx.available_processor_count())))
}

pub fn native_runtime_max_memory(
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
pub fn native_runtime_total_memory(
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
pub fn native_runtime_free_memory(
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
pub fn runtime_committed_heap(ctx: &mut dyn NativeContext) -> i64 {
    let used = ctx.heap_allocated_bytes() as i64;
    let max = ctx.max_heap_bytes();
    let committed = ctx.committed_heap_bytes() as i64;
    committed.max(used).min(max.max(used))
}

/// The four real `java.lang.Runtime$Version` field values, parsed out of a
/// version string without the JDK's regex engine.
pub struct RuntimeVersionParts {
    /// `version` — the dot-separated `$VNUM` sequence.
    pub numbers: Vec<i32>,
    /// `pre` — the `-$PRE` pre-release identifier.
    pub pre: Option<String>,
    /// `build` — the `+$BUILD` build number.
    pub build: Option<i32>,
    /// `optional` — the trailing `-$OPT` build information.
    pub optional: Option<String>,
}

/// Hand-parse `$VNUM(-$PRE)?(\+$BUILD)?(-$OPT)?` — the JDK's `VSTR_FORMAT`.
///
/// `Runtime.Version.parse` compiles a `java.util.regex` pattern for this
/// grammar; running it is exactly what `native_runtime_version` exists to
/// avoid (see its doc comment). The grammar is small enough to split by hand:
/// `+` separates `$VNUM(-$PRE)` from `$BUILD(-$OPT)`, and without a `+` the
/// only `-`-introduced parts are `$PRE` then `$OPT`.
pub fn parse_runtime_version_str(s: &str) -> Option<RuntimeVersionParts> {
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
pub fn runtime_version_parts(ctx: &dyn NativeContext) -> RuntimeVersionParts {
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
pub fn optional_of_str(ctx: &mut dyn NativeContext, value: Option<&str>) -> Option<Value> {
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
pub fn optional_of_int(ctx: &mut dyn NativeContext, value: Option<i32>) -> Option<Value> {
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
pub fn boxed_int_list(ctx: &mut dyn NativeContext, numbers: &[i32]) -> Option<Value> {
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
pub fn native_runtime_version(
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
pub static RUNTIME_VERSION_ROOT: VmScoped<Option<usize>> = VmScoped::new();

/// The already-built `Runtime.version()` singleton, if there is one.
pub fn runtime_version_cached(ctx: &dyn NativeContext) -> Option<ObjectRef> {
    let handle = RUNTIME_VERSION_ROOT
        .peek(ctx.vm_identity(), |slot| *slot)
        .flatten()?;
    ctx.resolve_global_root(handle)
}


pub(crate) fn jni_library_loader_id(
    ctx: &mut dyn NativeContext,
    from_class: Option<ObjectRef>,
) -> u32 {
    if let Some(mirror) = from_class {
        if let Some(cid) = ctx.class_id_from_mirror(mirror) {
            if let Some(loader_obj) = crate::classloader::defining_loader_for(
                crate::classloader::loader_tables(&*ctx),
                cid.as_u32(),
            ) {
                return crate::classloader::loader_namespace_id(ctx, loader_obj);
            }
            return ctx
                .loader_id_of_class_definite(cid)
                .and_then(|id| u32::try_from(id).ok())
                .unwrap_or(0);
        }
    }
    0
}

