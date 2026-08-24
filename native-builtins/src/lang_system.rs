// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! System, Runtime, ProcessBuilder, and Thread native method implementations.

use cratonvm_native_api::{NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::{LinkageError, MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

use crate::{try_alloc_concurrent_synthetic, obj_arg, platform_lib_name};

// ---------------------------------------------------------------------------
// System.exit / Runtime.exit pre-termination hook.
//
// `native_system_exit` and `native_runtime_exit` both `std::process::exit`
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
// ---------------------------------------------------------------------------
static SHUTDOWN_HOOKS: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());

/// Set the moment `run_shutdown_hooks` drains the list, i.e. once shutdown has
/// begun. From that point HotSpot's `ApplicationShutdownHooks.add`/`remove`
/// throw `IllegalStateException("Shutdown in progress")`.
///
/// MEASURED on 25.0.3+9 (lane C11, `HookContract addduring` / `removeduring`):
/// both calls, made from inside a running hook, threw
/// `java.lang.IllegalStateException: Shutdown in progress`, and in the `remove`
/// case the hook it tried to cancel ran anyway.
///
/// Without this flag a late registration would be accepted and then silently
/// dropped — the same "a marker that never appears" shape this whole record is
/// about, reintroduced by its own repair.
static SHUTDOWN_IN_PROGRESS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

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
    if SHUTDOWN_IN_PROGRESS.load(std::sync::atomic::Ordering::SeqCst) {
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
    {
        let hooks = SHUTDOWN_HOOKS.lock().unwrap_or_else(|e| e.into_inner());
        if hooks
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
    }
    let handle = ctx.add_global_root(hook);
    SHUTDOWN_HOOKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(handle);
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
    if SHUTDOWN_IN_PROGRESS.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(RuntimeError::IllegalStateException {
            message: HOOK_MSG_SHUTDOWN_IN_PROGRESS.to_string(),
        }
        .into());
    }
    let Some(hook) = hook else {
        return Err(RuntimeError::NullPointerException { message: None }.into());
    };
    let handle = {
        let mut hooks = SHUTDOWN_HOOKS.lock().unwrap_or_else(|e| e.into_inner());
        match hooks
            .iter()
            .position(|h| ctx.resolve_global_root(*h) == Some(hook))
        {
            Some(pos) => hooks.remove(pos),
            None => return Ok(false),
        }
    };
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
/// as "the VM hung" and produce a false finding, so the default here is a
/// bounded wait plus a loud line naming what was still running — a stated,
/// visible divergence rather than a silent hang.
///
/// `CRATONVM_SHUTDOWN_HOOK_TIMEOUT_MS=0` restores HotSpot's unbounded wait;
/// any other value sets the bound in milliseconds.
///
/// Read through `flags::runtime_var`. It used to use `std::env::var`, with a
/// note saying `nbflags()` lives in `lib.rs` which that lane did not own — but
/// the boundary this needs is `cratonvm_types::flags`, not `nbflags`, and this
/// crate already depends on it. Raw was wrong twice over: the name was declared
/// nowhere (so `CRATONVM_THREADS=shutdown-hook-timeout-ms=…` could not reach
/// it), and a raw read of a declared name is served by a live `getenv` instead
/// of the latched snapshot, which check 4 of `tools/flag-census/check-surface.sh`
/// flags for every core crate.
fn shutdown_hook_join_bound() -> Option<std::time::Duration> {
    static BOUND: std::sync::OnceLock<Option<std::time::Duration>> = std::sync::OnceLock::new();
    *BOUND.get_or_init(|| {
        let ms = cratonvm_types::flags::runtime_var("CRATONVM_SHUTDOWN_HOOK_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(30_000);
        if ms == 0 {
            None
        } else {
            Some(std::time::Duration::from_millis(ms))
        }
    })
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
/// `docs/known-issues/jdk-only/G5-1-inheritable-threadlocal-captures-at-
/// construction-20260816.md` §6, and read that record BEFORE assuming the
/// `new Thread` timing divergence is closed. It is not.
pub(crate) fn capture_inheritable_tl_at_construction(
    ctx: &mut dyn NativeContext,
    child: ObjectRef,
) {
    let child_hash = ctx.identity_hash_code(child);
    let snapshot = crate::phases_early::snapshot_inheritable_tl_entries(ctx).unwrap_or_default();
    crate::phases_early::queue_inherited_tl_for_child(child_hash, snapshot);
}

/// Did some construction-time site already answer the inheritance question for
/// this child Thread?
///
/// Asked of the pending-inheritance queue itself rather than a side flag: an
/// entry is present if and only if a capture happened, empty entries included,
/// so the predicate and the data it guards cannot disagree. This tree has a
/// written record of exactly that species of twin drifting
/// (`thread_already_started`, just above, exists for the same reason).
pub(crate) fn inheritable_tl_captured_at_construction(child_thread_hash: i32) -> bool {
    crate::phases_early::tl_inherited_pending()
        .lock()
        .contains_key(&child_thread_hash)
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
/// Idempotent: the list is drained under the lock and `SHUTDOWN_IN_PROGRESS`
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
pub fn run_shutdown_hooks(ctx: &mut dyn NativeContext, trigger: &str) {
    SHUTDOWN_IN_PROGRESS.store(true, std::sync::atomic::Ordering::SeqCst);
    let handles: Vec<usize> = {
        let mut hooks = SHUTDOWN_HOOKS.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *hooks)
    };

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
                eprintln!(
                    "[cratonvm] shutdown hook could not be started; continuing with the \
                     remaining hooks: {e:?}"
                );
            }
        }
    }

    // Phase 2 — join. Poll `thread_is_alive` rather than `ctx.thread_join`
    // so the wait can be bounded; `thread_start` registers the thread as alive
    // BEFORE spawning it, so there is no "not yet visible" race here.
    let bound = shutdown_hook_join_bound();
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
            ctx.begin_blocking_region();
            std::thread::sleep(std::time::Duration::from_millis(2));
            ctx.end_blocking_region();
        }
    }
    let ran = started.len() - unjoined;

    for handle in handles {
        ctx.remove_global_root(handle);
    }

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
    eprintln!(
        "[cratonvm] shutdown hooks: ran={ran} threw={threw} skipped={skipped} \
         unjoined={unjoined} trigger={trigger}"
    );
    report_vector_intrinsics();
    report_filechannel_fast_io();
    crate::craton_gpu::dispatch_timing::report();
    // The corrupt-cell census. HERE and not in `vm-cli`, because a JUnit runner
    // exits through `System.exit` and never reaches `vm-cli`'s normal-return
    // arm -- which is how a 1975-class sweep produced this line in zero logs
    // and read as a clean run.
    cratonvm_types::cell_census::exit_summary();
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
    if handled == 0 && fell_back == 0 && !asked {
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
    if !asked && !cratonvm_native_io::file_channel_fast_read::stats::touched() {
        return;
    }
    eprintln!("{}", cratonvm_native_io::file_channel_fast_read::stats::report());
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
/// `std::process::exit` has **seven** call sites in this crate, not three: four
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
    let mut dims = 0usize;
    let mut rest = internal;
    while let Some(stripped) = rest.strip_prefix('[') {
        dims += 1;
        rest = stripped;
    }
    let base = if dims == 0 {
        rest.replace('/', ".")
    } else {
        match rest.as_bytes().first() {
            Some(b'L') => rest
                .trim_start_matches('L')
                .trim_end_matches(';')
                .replace('/', "."),
            Some(b'Z') => "boolean".to_string(),
            Some(b'B') => "byte".to_string(),
            Some(b'C') => "char".to_string(),
            Some(b'S') => "short".to_string(),
            Some(b'I') => "int".to_string(),
            Some(b'J') => "long".to_string(),
            Some(b'F') => "float".to_string(),
            Some(b'D') => "double".to_string(),
            _ => rest.replace('/', "."),
        }
    };
    format!("{base}{}", "[]".repeat(dims))
}

/// HotSpot's per-element `ArrayStoreException` text for a reference copy whose
/// source holds an element the destination component type cannot accept.
///
/// It names the source ARRAY and the destination COMPONENT — not the offending
/// index, which is why the previous "source element at index N" wording could
/// not have come from HotSpot.
fn element_type_mismatch_message(
    ctx: &mut dyn NativeContext,
    src: ObjectRef,
    dst_elem_class: cratonvm_types::ClassId,
) -> String {
    let src_component = ctx
        .class_name_of_id(ctx.class_id_of_object(src))
        .unwrap_or_default();
    let dst_component = ctx.class_name_of_id(dst_elem_class).unwrap_or_default();
    format!(
        "arraycopy: element type mismatch: can not cast one of the elements of {}[] to the type of the destination array, {}",
        external_class_name(&src_component),
        external_class_name(&dst_component)
    )
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
    use cratonvm_types::ObjectKind;
    use cratonvm_types::error::arraycopy_message;
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
            let trace = ctx.capture_stack_trace(0);
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
            (src_pos, arraycopy_message::source_index(src_pos, ty, src_len))
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
        let object_class_id = ctx.class_id_by_name("java/lang/Object");

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
            if ctx.heap_kind_of(elem) == ObjectKind::Array {
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
            // Forward direction is safe вЂ” check-then-write per element.
            for i in 0..length {
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
    let trace = ctx.capture_stack_trace(0);
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
    let trace = ctx.capture_stack_trace(0);
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
    let millis = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let nanos = match args.get(1) {
        Some(Value::Int(v)) => *v,
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
    if !(0..=999_999).contains(&nanos) {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: "nanosecond timeout value out of range".to_string(),
            }
            .into(),
        );
    }
    // RD.9: combine millis + nanos into a single nanosecond value and delegate
    // to `sleepNanos0` so sub-millisecond sleeps honour the requested
    // precision (the previous round-up-to-millis path lost precision for
    // sub-ms sleeps вЂ” Thread.sleep(0, 500_000) used to block for 1ms).
    let total_nanos = (millis as i128)
        .saturating_mul(1_000_000)
        .saturating_add(nanos as i128);
    if total_nanos <= 0 {
        return Ok(None);
    }
    let clamped = total_nanos.min(i64::MAX as i128) as i64;
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
fn sleep_interrupted(ctx: &mut dyn NativeContext) -> cratonvm_types::error::MethodCallFailed {
    let msg = ctx.create_string("sleep interrupted");
    if let Ok(Some(Value::Object(Some(exc)))) = ctx.new_object_initialized(
        "java/lang/InterruptedException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(msg))],
    ) {
        return cratonvm_types::error::MethodCallFailed::ExceptionThrown(exc);
    }
    cratonvm_types::error::MethodCallFailed::InternalError(
        cratonvm_types::error::VmError::Runtime(
            cratonvm_types::error::RuntimeError::InterruptedException,
        ),
    )
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
                let st = ctx.capture_stack_trace(0);
                let frames: Vec<String> = st
                    .iter()
                    .take(12)
                    .map(|e| format!("{}.{}:{}", e.class_name, e.method_name, e.line_number))
                    .collect();
                eprintln!("[SLEEP-TRACE #{n} millis={millis}] {}", frames.join(" <- "));
            }
        }
        // Check interrupted before sleeping
        if ctx.is_interrupted(true) {
            return Err(sleep_interrupted(ctx));
        }
        // NEW-15.4: virtual-thread aware sleep.
        //
        // A non-pinned virtual thread releases its carrier permit before
        // blocking so another virtual thread can run on the carrier pool.
        // A pinned virtual thread (inside a monitor / JNI call) keeps the
        // carrier and emits `jdk.VirtualThreadPinned` per JEP 491.
        let is_virtual = ctx.is_current_virtual();
        let pinned = is_virtual && ctx.vt_pin_count() > 0;
        if pinned {
            ctx.emit_virtual_thread_pinned_jfr("Thread.sleep while pinned");
        }
        let release = is_virtual && !pinned;
        let effective_millis = crate::async_handoff_sleep_millis(millis);
        let target = std::time::Duration::from_millis(effective_millis as u64);
        if release && ctx.vt_park_for(target) {
            return Err(cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::ContinuationYield {
                    wake_after_nanos: target.as_nanos().min(u64::MAX as u128) as u64,
                },
            ));
        }
        if release {
            ctx.vt_release_carrier();
        }
        let sleep_start = std::time::Instant::now();
        // WP4.5 вЂ” pump in 10ms slices so any
        // `ScheduledExecutorService.scheduleAtFixedRate` registrations
        // get a chance to fire while the caller is asleep. The pump
        // is a no-op when the registry is empty so the unmodified
        // sleep cost is just one Mutex::lock per slice.
        let pump_slice = std::time::Duration::from_millis(10);
        // CompletableFuture.runAsync users commonly use short sleeps as
        // scheduling barriers. CratonVM's workers start quickly, but cold Java
        // proxy/linkage and real CompletableFuture submission overhead can make
        // 10 ms handoff sleeps and 100 ms worker sleeps too tight. When a sleep
        // immediately follows async submission, give those short sleeps a small
        // scheduling floor; Thread.sleep only promises to sleep at least the
        // requested duration.
        let deadline = sleep_start + target;
        let mut interrupted = false;
        loop {
            crate::scheduled_pump::registry().pump(ctx);
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            if ctx.is_interrupted(false) {
                interrupted = true;
                break;
            }
            // `Thread.sleep` has a bounded duration, so `Thread.getState()`
            // must report `TIMED_WAITING`, not plain `WAITING` (real JDK
            // distinguishes them; `SpringApplicationShutdownHookTests`
            // polls for exactly this state via Awaitility).
            ctx.begin_timed_blocking_region();
            std::thread::sleep(remaining.min(pump_slice));
            ctx.end_blocking_region();
        }
        let actual_dur = sleep_start.elapsed();
        if release {
            ctx.vt_acquire_carrier();
        }
        ctx.record_thread_sleep(millis * 1_000_000, actual_dur.as_nanos() as u64);
        // Check interrupted after sleeping (with clear).
        let interrupted_after_sleep = ctx.is_interrupted(true);
        if interrupted || interrupted_after_sleep {
            return Err(cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    cratonvm_types::error::RuntimeError::InterruptedException,
                ),
            ));
        }
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

pub(crate) fn native_thread_start0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
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
        return Err(RuntimeError::IllegalThreadStateException {
            // HotSpot throws the no-arg constructor here: the message is
            // NULL, not a description. Measured.
            message: String::new(),
        }
        .into());
    }
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
    let child_hash = ctx.identity_hash_code(this);
    let captured_at_construction = inheritable_tl_captured_at_construction(child_hash);
    if !captured_at_construction {
        if let Some(snap) = crate::phases_early::snapshot_inheritable_tl_entries(ctx) {
            crate::phases_early::queue_inherited_tl_for_child(child_hash, snap);
        }
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
    if inherit_ccl
        && !matches!(
            ctx.get_field_by_name(this, "contextClassLoader"),
            Value::Object(Some(_))
        )
    {
        let parent = ctx.current_thread_object();
        if let Value::Object(Some(parent_ccl)) = ctx.get_field_by_name(parent, "contextClassLoader")
        {
            ctx.set_field_by_name(this, "contextClassLoader", Value::Object(Some(parent_ccl)));
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
    // (gated) rather than deleted because it becomes live the moment the
    // nominated fix demotes those Intrinsics — see
    // `docs/known-issues/jdk-only/G5-1-inheritable-threadlocal-captures-at-
    // construction-20260816.md` §5.
    //
    // Known trade-off: `characteristics` (which flags an explicit
    // `Thread(group, target, name, stackSize, false)` opt-out of
    // inheritance) isn't retained anywhere observable post-construction, so
    // this can't distinguish "buggy" from "intentionally opted out". A
    // legitimate opt-out combined with non-null group+name would incorrectly
    // regain inheritance. That combination is rare in practice (the 5-arg
    // opt-out constructor itself is rarely used); documented pending a real
    // interpreter-level fix. Opt-out: `CRATONVM_INHERIT_TL_WORKAROUND=0`.
    let apply_itl_workaround = crate::nbflags().inherit_tl_workaround;
    // The buggy path doesn't leave this field as `Object(None)` (the normal
    // "never written" value real bytecode `getfield` would observe) — a raw
    // native heap read here sees `Int(0)` instead, matching CratonVM's
    // zero-fill representation for a slot the constructor's `putfield`
    // (offset 201 in the real master constructor) never actually reached.
    // Treat either as "not yet inherited".
    let child_itl_unset = matches!(
        ctx.get_field_by_name(this, "inheritableThreadLocals"),
        Value::Object(None) | Value::Int(0)
    );
    // G5-1: and never when the construction-time capture already answered for
    // this child. Once the nominated demotion lands, the master constructor
    // does the copy itself at construction; a start0-time copy on top of it
    // would be exactly the timing divergence this record exists to remove.
    if apply_itl_workaround && child_itl_unset && !captured_at_construction {
        let parent = ctx.current_thread_object();
        if let Value::Object(Some(parent_map)) =
            ctx.get_field_by_name(parent, "inheritableThreadLocals")
        {
            if let Ok(Some(new_map)) = ctx.invoke(
                "java/lang/ThreadLocal",
                "createInheritedMap",
                "(Ljava/lang/ThreadLocal$ThreadLocalMap;)Ljava/lang/ThreadLocal$ThreadLocalMap;",
                &[Value::Object(Some(parent_map))],
            ) {
                ctx.set_field_by_name(this, "inheritableThreadLocals", new_map);
            }
        }
    }
    ctx.thread_start(this)
}

pub(crate) fn native_thread_join(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    ctx.thread_join(this)
}

pub(crate) fn native_thread_join_timed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mut this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let millis = match args.get(1) {
        Some(Value::Long(ms)) => *ms,
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
        // join(0) means wait forever (same as join())
        return ctx.thread_join(this);
    }
    // RD.10: timed join вЂ” return after the specified timeout even if the
    // target thread is still alive. Poll isAlive at a small cadence so we
    // don't block beyond the deadline.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(millis as u64);
    if crate::nbflags().dbg_sleep_trace {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        if N.fetch_add(1, Ordering::Relaxed) % 64 == 0 {
            let st = ctx.capture_stack_trace(0);
            let frames: Vec<String> = st
                .iter()
                .take(10)
                .map(|e| format!("{}.{}:{}", e.class_name, e.method_name, e.line_number))
                .collect();
            eprintln!("[JOIN-TIMED-TRACE millis={millis}] {}", frames.join(" <- "));
        }
    }
    loop {
        if !ctx.thread_is_alive(this) {
            break;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        // Honour an interrupt that arrived while we were waiting вЂ” throw
        // InterruptedException so caller code behaves like HotSpot.
        if ctx.is_interrupted(true) {
            return Err(cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    cratonvm_types::error::RuntimeError::InterruptedException,
                ),
            ));
        }
        let sleep_time = remaining.min(std::time::Duration::from_millis(2));
        let mut blocked_refs = [Value::Object(Some(this))];
        ctx.begin_blocking_region();
        std::thread::sleep(sleep_time);
        ctx.end_blocking_region_refs(&mut blocked_refs);
        if let Value::Object(Some(cur)) = blocked_refs[0] {
            this = cur;
        }
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
    let millis = match args.get(1) {
        Some(Value::Long(ms)) => *ms,
        _ => 0,
    };
    let nanos = match args.get(2) {
        Some(Value::Int(n)) => *n,
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
    if !(0..=999_999).contains(&nanos) {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: "nanosecond timeout value out of range".to_string(),
            }
            .into(),
        );
    }
    let effective_ms = if nanos > 0 {
        millis.saturating_add(1)
    } else {
        millis
    };
    let this = args.first().copied().unwrap_or(Value::Object(None));
    native_thread_join_timed(ctx, &[this, Value::Long(effective_ms)])
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
    ctx.set_field_by_name(this, "interrupted", Value::Int(1));
    Ok(None)
}

pub(crate) fn native_thread_get_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            let name = ctx.create_string("main");
            return Ok(Some(Value::Object(Some(name))));
        }
    };
    match ctx.get_field_by_name(this, "name") {
        Value::Object(Some(str_ref)) => Ok(Some(Value::Object(Some(str_ref)))),
        _ => match ctx.get_field(this, 0) {
            Value::Object(Some(str_ref)) => Ok(Some(Value::Object(Some(str_ref)))),
            _ => {
                let name = ctx.create_string("main");
                Ok(Some(Value::Object(Some(name))))
            }
        },
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

pub(crate) fn native_system_nano_time(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    use std::time::Instant;
    // Use a monotonic clock. We return the elapsed nanos since the first call.
    // Rust's Instant doesn't have a fixed epoch, but nano deltas work.
    use std::sync::OnceLock;
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    let nanos = start.elapsed().as_nanos() as i64;
    Ok(Some(Value::Long(nanos)))
}

pub(crate) fn native_system_exit(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let code = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let trace = ctx.capture_stack_trace(0);

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
    eprintln!("[cratonvm] System.exit({code}) called вЂ” process terminating");
    // W7-92: hooks first, and BEFORE the slot-map sweep — a hook is Java code
    // that can load classes and allocate, so sweeping first would census a
    // heap the hooks are about to change. `System.exit` from a non-main thread
    // reaches this same native on that thread's ctx, so this one line covers
    // two of the five exit paths (measured on HotSpot: rc 3 and rc 4, hooks
    // ran on both).
    run_shutdown_hooks(ctx, "System.exit");
    sweep_declared_slot_maps_before_exit(&*ctx, "System.exit");
    invoke_pre_exit_hook(code);
    // LAST, and after the pre-exit hook, because that hook's own job is to
    // dump diagnostics. `std::process::exit` runs no destructors, so anything
    // still sitting in the fd table's stdout/stderr buffers dies here. `code`
    // is untouched: this call cannot fail out and cannot change it.
    flush_console_streams(&*ctx);
    std::process::exit(code);
}

// ---------------------------------------------------------------------------
// Phase 13 Step 4: Runtime + System.lineSeparator
// ---------------------------------------------------------------------------

pub(crate) fn native_system_line_separator(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let sep = if cfg!(windows) { "\r\n" } else { "\n" };
    let s = ctx.create_string(sep);
    Ok(Some(Value::Object(Some(s))))
}

pub(crate) fn register_runtime_natives(registry: &mut NativeMethodRegistry) {
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
    registry.register_with_kind("java/lang/Runtime", "gc", "()V", |ctx, _args| {
        ctx.force_gc();
        Ok(None)
    }, NativeKind::Bridge);
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
    registry.register_with_kind("java/lang/Shutdown", "beforeHalt", "()V", |_ctx, _args| {
        // HotSpot's does nothing an application can observe.
        Ok(None)
    }, NativeKind::Bridge);

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
                load_library_or_throw(ctx, &name, LibrarySpelling::BareName, None, LoaderScoping::On)
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
// (the `docs/known-issues/jdk-only/README.md` loader-identity cluster).
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
    // Report the configured `-Xmx` (container-aware once sized from a cgroup
    // limit) instead of a hardcoded 256 MB, matching HotSpot's contract that
    // `maxMemory()` reflects the actual heap ceiling.
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
/// `committed_heap_bytes` is a CAPACITY and must stay one — see
/// `gc::vm_heap::VmHeap::committed_bytes` for why a value that moved on every
/// collection would be a behavioural change for callers that gc until
/// `totalMemory()` settles.
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
/// * never below `used`, so `freeMemory()` cannot be 0 while the heap is
///   plainly serving allocations (a collector that reports a fixed arena is
///   already above `used`; the clamp is for the growable one, sampled mid-grow);
/// * never above `maxMemory()`, which is the configured `-Xmx` and is the
///   ceiling every caller compares against.
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
    Ok(Some(Value::Object(Some(runtime_version_publish(ctx, version)))))
}

/// Global-root handle for the `Runtime.version()` singleton.
///
/// A `ctx.set_static_field` into the JDK's own `Runtime.version` field does not
/// stick (the write is accepted and the next read returns null), so the
/// singleton lives in the JNI global-ref table instead — a persistent root the
/// moving collector remaps, which is why this holds a *handle* and never a raw
/// `ObjectRef`.
static RUNTIME_VERSION_ROOT: std::sync::Mutex<Option<usize>> = std::sync::Mutex::new(None);

/// The already-built `Runtime.version()` singleton, if there is one.
fn runtime_version_cached(ctx: &dyn NativeContext) -> Option<ObjectRef> {
    let handle = (*RUNTIME_VERSION_ROOT.lock().unwrap_or_else(|e| e.into_inner()))?;
    ctx.resolve_global_root(handle)
}

/// Publish `version` as the singleton and return whichever instance won — a
/// concurrent first call may have published its own, and identity is the whole
/// point of memoising.
fn runtime_version_publish(ctx: &mut dyn NativeContext, version: ObjectRef) -> ObjectRef {
    let mut slot = RUNTIME_VERSION_ROOT.lock().unwrap_or_else(|e| e.into_inner());
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
        let trace = ctx.capture_stack_trace(0);
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
    eprintln!("[cratonvm] Runtime.exit({code}) called вЂ” process terminating");
    // W7-92: see `native_system_exit` for why the hooks run before the sweep.
    run_shutdown_hooks(ctx, "Runtime.exit");
    sweep_declared_slot_maps_before_exit(&*ctx, "Runtime.exit");
    invoke_pre_exit_hook(code);
    // See `native_system_exit` — same reason, same position, same `code`.
    flush_console_streams(&*ctx);
    std::process::exit(code);
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
    eprintln!("[cratonvm] Runtime.halt({code}) called - process terminating");
    sweep_declared_slot_maps_before_exit(&*ctx, "Runtime.halt");
    invoke_pre_exit_hook(code);
    // NO `run_shutdown_hooks` here, and that is the whole point of `halt`
    // (MEASURED, `HookProbe halt`: `HOOK-OUT h-1 MUST-NOT-APPEAR` does not
    // appear, rc 5). The FLUSH is a different question and HotSpot answers it
    // the other way: `HookProbe haltnoflush` writes an unterminated,
    // unflushed `System.out.print` and then halts, and the bytes are still
    // delivered. So flush, run nothing.
    flush_console_streams(&*ctx);
    std::process::exit(code);
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
    let sm = crate::security_manager::get_security_manager(&*ctx);
    let Some(sm_ref) = sm else { return Ok(()) };

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
    let cmd_str = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "Runtime.exec: null command".to_string(),
            }
            .into())
        }
    };
    let parts: Vec<String> = cmd_str.split_whitespace().map(String::from).collect();
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
    let cmd_str = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "Runtime.exec: null command".to_string(),
            }
            .into())
        }
    };
    let parts: Vec<String> = cmd_str.split_whitespace().map(String::from).collect();
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
    let cmd_str = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "Runtime.exec: null command".to_string(),
            }
            .into())
        }
    };
    let parts: Vec<String> = cmd_str.split_whitespace().map(String::from).collect();
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
        _ => return Ok(Some(Value::Object(None))),
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
    if allow_java_call && ctx.ensure_class_initialized("java/util/Collections").is_ok() {
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
    let buckets = ctx.new_ref_array(ClassId::new(0), cap);

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
        let map = ctx.alloc_object(hashmap_class_id, map_n_fields);
        ctx.set_field(map, f_table, Value::Object(Some(buckets)));
        ctx.set_field(map, f_size, Value::Int(0));
        // threshold = (int)(capacity * 0.75) for the default load factor.
        ctx.set_field(map, f_threshold, Value::Int((cap as i32 * 3) / 4));
        ctx.set_field(map, f_loadfactor, Value::Float(0.75));
        ctx.set_field(map, f_entryset, Value::Object(None));

        // Fallible since 2026-08-10 (JDK-only wave 2, step 3). This arm is the
        // REAL-layout path — every field index above came from the real
        // `java.util.HashMap`/`HashMap$Node` — so `HashMap$Node` is already
        // loaded here and the ask resolves to the real class rather than
        // fabricating. The refusal only fires on an image where it is not.
        let node_class_id = crate::util_concurrent_ext::refused_class(
            ctx,
            "java/util/HashMap$Node",
            node_n_fields,
        )?;

        for (key, value) in std::env::vars() {
            let key_obj = ctx.create_string(&key);
            let val_obj = ctx.create_string(&value);
            let hash = jdk_string_hash(&key);
            // (n-1) & hash, since cap=16 is power of two.
            let idx = ((cap as u32 - 1) & hash as u32) as usize;
            let node = ctx.alloc_object(node_class_id, node_n_fields);
            ctx.set_field(node, n_hash, Value::Int(hash));
            ctx.set_field(node, n_key, Value::Object(Some(key_obj)));
            ctx.set_field(node, n_value, Value::Object(Some(val_obj)));
            let existing = ctx.get_array_element(buckets, idx);
            ctx.set_field(node, n_next, existing);
            ctx.set_array_element(buckets, idx, Value::Object(Some(node)));

            let old_size = match ctx.get_field(map, f_size) {
                Value::Int(s) => s,
                _ => 0,
            };
            ctx.set_field(map, f_size, Value::Int(old_size + 1));
        }

        // Cache the OpenJDK-shaped process-wide singleton (double-checked
        // publish). The wrapper's field 0 is the private `m` backing field that
        // libraries such as System Rules reach via reflection.
        //
        // `true`: this is the real-layout arm, so every `java/util/HashMap`
        // field index above came off the real class and running
        // `java.util.Collections` bytecode here is safe. See
        // `wrap_system_env_map`'s boot-ordering note.
        let env = wrap_system_env_map(ctx, map, true);
        let env = set_system_env_singleton(ctx.vm_identity(), env);
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
        crate::util_concurrent_ext::refused_class(ctx, "java/util/HashMap", 3)?;
    let fallback_node_class_id =
        crate::util_concurrent_ext::refused_class(ctx, "java/util/HashMap$Node", 4)?;
    let map = ctx.alloc_object(fallback_map_class_id, 3); // MAP_NUM_FIELDS = 3
    ctx.set_field(map, 0, Value::Object(Some(buckets))); // MAP_FIELD_BUCKETS
    ctx.set_field(map, 1, Value::Int(0)); // MAP_FIELD_SIZE
    ctx.set_field(map, 2, Value::Int(cap as i32)); // MAP_FIELD_CAPACITY

    for (key, value) in std::env::vars() {
        let key_obj = ctx.create_string(&key);
        let val_obj = ctx.create_string(&value);
        let hash = jdk_string_hash(&key);
        let idx = ((cap as u32 - 1) & hash as u32) as usize;
        let node = ctx.alloc_object(fallback_node_class_id, 4); // hash, key, value, next
        ctx.set_field(node, 0, Value::Int(hash));
        ctx.set_field(node, 1, Value::Object(Some(key_obj)));
        ctx.set_field(node, 2, Value::Object(Some(val_obj)));

        let existing = ctx.get_array_element(buckets, idx);
        ctx.set_field(node, 3, existing); // next = existing bucket head
        ctx.set_array_element(buckets, idx, Value::Object(Some(node)));

        let old_size = match ctx.get_field(map, 1) {
            Value::Int(s) => s,
            _ => 0,
        };
        ctx.set_field(map, 1, Value::Int(old_size + 1));
    }

    // `false`: this arm was reached BECAUSE the real `java/util/HashMap` layout
    // was not resolvable, so the class library cannot be asked to run
    // `java.util.Collections.unmodifiableMap` — and a real wrapper delegating to
    // a 3-field synthetic map would be worse than the stand-in whose shims read
    // slot 0. Uncached, as before, so a later call retries the real path.
    let env = wrap_system_env_map(ctx, map, false);
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

/// Materialize a `java/lang/StackTraceElement[]` from a captured frame trace
/// (innermost frame first, as `getStackTrace()` expects index 0 = current
/// call). Shared by `Thread.getStackTrace0()` and `Thread.dumpThreads()`.
pub(crate) fn build_stack_trace_element_array(
    ctx: &mut dyn NativeContext,
    trace: &[cratonvm_native_api::StackTraceEntry],
) -> Result<cratonvm_types::ObjectRef, MethodCallFailed> {
    let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), trace.len());
    // The captured trace is outermost-first (frame[0] = bottom of stack);
    // getStackTrace()/getAllStackTraces() want index 0 = the innermost (current)
    // call, so materialize reversed вЂ” matching HotSpot ordering.
    for (i, e) in trace.iter().rev().enumerate() {
        let ste = crate::try_alloc_concurrent_synthetic(ctx, "java/lang/StackTraceElement", 4)?;
        let cls_dotted = match ctx.class_id_by_name(&e.class_name) {
            Some(cid) => crate::lang_class::dotted_class_name(ctx.vm_identity(), cid, &e.class_name),
            None => std::sync::Arc::from(e.class_name.replace('/', ".")),
        };
        crate::lang_misc::fill_stack_trace_element(
            ctx,
            ste,
            &e.class_name,
            &cls_dotted,
            &e.method_name,
            e.source_file.as_deref(),
            e.line_number,
        );
        ctx.set_array_element(arr, i, Value::Object(Some(ste)));
    }
    Ok(arr)
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
            let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let trace = ctx.thread_stack_trace(this);
    let arr = build_stack_trace_element_array(ctx, &trace);
    Ok(Some(Value::Object(Some(arr?))))
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
        _ => return Ok(Some(Value::Object(None))),
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
    let nanos = match args.first() {
        Some(Value::Long(n)) => *n,
        _ => 0,
    };
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
                let st = ctx.capture_stack_trace(0);
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
        // Check interrupted before sleeping вЂ” clear flag and throw
        if ctx.is_interrupted(true) {
            return Err(cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    cratonvm_types::error::RuntimeError::InterruptedException,
                ),
            ));
        }
        let requested_millis = ((nanos as u128) + 999_999) / 1_000_000;
        let effective_millis =
            crate::async_handoff_sleep_millis(requested_millis.min(i64::MAX as u128) as i64);
        let effective_nanos = (effective_millis as u128)
            .saturating_mul(1_000_000)
            .max(nanos as u128)
            .min(u64::MAX as u128) as u64;
        let duration = std::time::Duration::from_nanos(effective_nanos);
        // NEW-15.4: virtual-thread aware nanosecond sleep (mirrors Thread.sleep(long)).
        let is_virtual = ctx.is_current_virtual();
        let pinned = is_virtual && ctx.vt_pin_count() > 0;
        if pinned {
            ctx.emit_virtual_thread_pinned_jfr("Thread.sleep(nanos) while pinned");
        }
        let release = is_virtual && !pinned;
        if release && ctx.vt_park_for(duration) {
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
        ctx.begin_blocking_region();
        std::thread::sleep(duration);
        ctx.end_blocking_region();
        let actual_dur = sleep_start.elapsed();
        if release {
            ctx.vt_acquire_carrier();
        }
        ctx.record_thread_sleep(nanos, actual_dur.as_nanos() as u64);
        // Check interrupted after sleeping вЂ” clear flag and throw
        if ctx.is_interrupted(true) {
            return Err(cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    cratonvm_types::error::RuntimeError::InterruptedException,
                ),
            ));
        }
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
//   * Sleep is performed in 100ms chunks so an interrupt delivered from
//     another thread is observed within в‰¤ ~100ms. This is a trade-off
//     between interrupt-responsiveness and syscall cost.
//   * The interrupt flag is CLEARED when we throw `InterruptedException`
//     per JDK spec.
pub(crate) fn native_thread_sleep0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // WP4.5 вЂ” see `native_thread_sleep`: long args from the operand-stack
    // get re-decoded as Doubles by `CompactValue::to_value()`.
    let raw_millis: i64 = match args.first() {
        Some(Value::Long(ms)) => *ms,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Int(i)) => *i as i64,
        _ => {
            return Err(
                cratonvm_types::error::RuntimeError::IllegalArgumentException {
                    message: "sleep0: missing long millis arg".to_string(),
                }
                .into(),
            );
        }
    };
    if raw_millis < 0 {
        return Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: "timeout value is negative".to_string(),
            }
            .into(),
        );
    }
    let millis = raw_millis as u64;

    // 0ms case: check interrupt status (and clear) then return immediately.
    if millis == 0 {
        if ctx.is_interrupted(true) {
            return Err(cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    cratonvm_types::error::RuntimeError::InterruptedException,
                ),
            ));
        }
        return Ok(None);
    }

    // NEW-15.4: virtual-thread aware sleep вЂ” release the carrier if this
    // is a non-pinned virtual thread so another VT can run on the pool.
    let is_virtual = ctx.is_current_virtual();
    let pinned = is_virtual && ctx.vt_pin_count() > 0;
    if pinned {
        ctx.emit_virtual_thread_pinned_jfr("Thread.sleep0 while pinned");
    }
    let release = is_virtual && !pinned;
    let effective_millis = crate::async_handoff_sleep_millis(millis as i64) as u64;
    let sleep_duration = std::time::Duration::from_millis(effective_millis);
    if release && ctx.vt_park_for(sleep_duration) {
        return Err(cratonvm_types::error::MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::ContinuationYield {
                wake_after_nanos: sleep_duration.as_nanos().min(u64::MAX as u128) as u64,
            },
        ));
    }
    if release {
        ctx.vt_release_carrier();
    }

    // Chunked sleep: poll the interrupt flag every ~10ms so an interrupt
    // delivered by another thread is observed promptly without spinning,
    // and so the WP4.5 scheduled-pump fires periodic tasks during the
    // sleep window. (Pre-WP4.5 the chunk was 100ms; the smaller chunk
    // matches the resolution of `scheduleAtFixedRate`.)
    let start = std::time::Instant::now();
    let deadline = start + sleep_duration;
    let result = loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            break Ok(None);
        }
        crate::scheduled_pump::registry().pump(ctx);
        // Poll interrupt flag before each chunk вЂ” clear + throw if set.
        if ctx.is_interrupted(true) {
            break Err(cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    cratonvm_types::error::RuntimeError::InterruptedException,
                ),
            ));
        }
        let remaining = deadline - now;
        let chunk = std::cmp::min(remaining, std::time::Duration::from_millis(10));
        ctx.begin_blocking_region();
        std::thread::sleep(chunk);
        ctx.end_blocking_region();
    };

    let actual_dur = start.elapsed();
    if release {
        ctx.vt_acquire_carrier();
    }
    // Record for JFR even on interrupted paths so the sleep duration is
    // visible to profilers.
    ctx.record_thread_sleep(
        (millis as i64).saturating_mul(1_000_000),
        actual_dur.as_nanos() as u64,
    );
    result
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
    fn install_charset(ctx: &mut dyn NativeContext, stream: ObjectRef) {
        // Ensure the real PrintStream class is loaded so `charset` is a
        // resolvable field name. (No-op in pure synthetic-jdk mode.)
        let _ = ctx.ensure_class_initialized("java/io/PrintStream");
        let cs_class = match ctx.ensure_class_initialized("java/nio/charset/Charset") {
            Ok(cid) => cid,
            Err(_) => return,
        };
        let cs_fields = ctx.class_num_total_fields(cs_class).max(1);
        let cs_obj = ctx.alloc_object(cs_class, cs_fields);
        let name = ctx.create_string("UTF-8");
        ctx.set_field(cs_obj, 0, Value::Object(Some(name)));
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

    if let Some(out_stream) = ctx.get_system_stream("out") {
        install_charset(ctx, out_stream);
        ctx.set_static_field_by_name("java/lang/System", "out", Value::Object(Some(out_stream)));
    }
    if let Some(err_stream) = ctx.get_system_stream("err") {
        install_charset(ctx, err_stream);
        ctx.set_static_field_by_name("java/lang/System", "err", Value::Object(Some(err_stream)));
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
        // Reuse a pinned stdin object if `GETSTATIC System.in` / `ensure_system_stdin_object`
        // materialised it before `initPhase1` (Surefire fork bootstrap).
        let in_obj = if let Some(o) = ctx.get_system_stream("in") {
            o
        } else {
            let fis_class_id = ctx.ensure_class_initialized("java/io/FileInputStream")?;
            let num_fields = ctx.class_num_total_fields(fis_class_id).max(2);
            let new_in = ctx.alloc_object(fis_class_id, num_fields);
            let fd_class_id = ctx.ensure_class_initialized("java/io/FileDescriptor")?;
            let fd_fields = ctx.class_num_total_fields(fd_class_id).max(2);
            let fd_obj = ctx.alloc_object(fd_class_id, fd_fields);
            ctx.set_field_by_name(fd_obj, "fd", Value::Int(0));
            ctx.set_field_by_name(fd_obj, "handle", Value::Long(0));
            ctx.set_field_by_name(new_in, "fd", Value::Object(Some(fd_obj)));
            if !matches!(ctx.get_field_by_name(new_in, "fd"), Value::Object(Some(_))) {
                // Legacy synthetic fallback: encode stdin as `fd + 1` so the
                // reference-slot coercion path cannot collapse fd 0 to null.
                ctx.set_field(new_in, 1, Value::Int(1));
            }
            ctx.cache_system_stdin(new_in);
            new_in
        };
        let fd_obj = match ctx.get_field_by_name(in_obj, "fd") {
            Value::Object(Some(fd_obj)) => fd_obj,
            _ => {
                let fd_class_id = ctx.ensure_class_initialized("java/io/FileDescriptor")?;
                let fd_fields = ctx.class_num_total_fields(fd_class_id).max(2);
                let fd_obj = ctx.alloc_object(fd_class_id, fd_fields);
                ctx.set_field_by_name(in_obj, "fd", Value::Object(Some(fd_obj)));
                fd_obj
            }
        };
        ctx.set_field_by_name(fd_obj, "fd", Value::Int(0));
        ctx.set_field_by_name(fd_obj, "handle", Value::Long(0));
        ctx.cache_system_stdin(in_obj);
        ctx.set_static_field_by_name("java/lang/System", "in", Value::Object(Some(in_obj)));
    }

    // Step 3: Set System.lineSeparator from the line.separator property
    let line_sep = ctx
        .get_system_property("line.separator")
        .unwrap_or_else(|| if cfg!(windows) { "\r\n" } else { "\n" }.to_string());
    let line_sep_obj = ctx.create_string(&line_sep);
    ctx.set_static_field_by_name(
        "java/lang/System",
        "lineSeparator",
        Value::Object(Some(line_sep_obj)),
    );

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

/// `VM.getRuntimeArguments()[String` вЂ” return the VM runtime arguments.
///
/// Returns an empty String array (we don't expose internal runtime args).
pub(crate) fn native_vm_get_runtime_arguments(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 0);
    Ok(Some(Value::Object(Some(arr))))
}

// ---------------------------------------------------------------------------
// T15 вЂ” Remaining missing natives
// ---------------------------------------------------------------------------

/// `java/lang/ref/Finalizer.register(Object)V`
///
/// Registers an object for finalization. In our VM, we track finalizable
/// objects via the GC's reference discovery mechanism. This native is called
/// by the JDK's `Finalizer.register` to add the object to the finalization queue.
pub(crate) fn native_finalizer_register(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = the object to register for finalization
    if let Some(Value::Object(Some(obj))) = args.first() {
        // Use the reference discovery mechanism to track this object.
        // ref_type 3 = phantom-like (finalizer reference)
        // We create a synthetic finalizer reference wrapper.
        ctx.discover_reference(3, *obj, *obj, None);
    }
    Ok(None)
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
    let comp_name = match comp_mirror {
        Some(mirror) => crate::lang_class::mirror_class_name(&*ctx, mirror)
            .filter(|s| !s.is_empty())
            .or_else(|| ctx.read_string(mirror))
            .or_else(|| match ctx.get_field(mirror, 1) {
                Value::Object(Some(name_obj)) => ctx.read_string(name_obj),
                _ => None,
            })
            .map(|s| s.replace('.', "/"))
            .unwrap_or_else(|| "java/lang/Object".to_string()),
        _ => "java/lang/Object".to_string(),
    };

    if crate::nbflags().dbg_toarray_ok {
        eprintln!(
            "[DBG_TOARRAY] newArray comp_name={:?} len={}",
            comp_name, length
        );
    }

    // Map primitive type names to ArrayElementType. Accept both the human name
    // (`int`) and the JVM descriptor (`I`) вЂ” `mirror_class_name` may return
    // either depending on how the primitive mirror was registered.
    let arr = match comp_name.as_str() {
        "int" | "I" => ctx.new_array(cratonvm_types::ArrayElementType::Int, length),
        "long" | "J" => ctx.new_array(cratonvm_types::ArrayElementType::Long, length),
        "float" | "F" => ctx.new_array(cratonvm_types::ArrayElementType::Float, length),
        "double" | "D" => ctx.new_array(cratonvm_types::ArrayElementType::Double, length),
        "boolean" | "Z" => ctx.new_array(cratonvm_types::ArrayElementType::Boolean, length),
        "byte" | "B" => ctx.new_array(cratonvm_types::ArrayElementType::Byte, length),
        "char" | "C" => ctx.new_array(cratonvm_types::ArrayElementType::Char, length),
        "short" | "S" => ctx.new_array(cratonvm_types::ArrayElementType::Short, length),
        _ => {
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
            let comp_id = comp_mirror
                .and_then(|m| crate::lang_class::mirror_class_id(&*ctx, m))
                .or_else(|| ctx.ensure_class_initialized(&comp_name).ok())
                .unwrap_or(cratonvm_types::ClassId::new(0));
            ctx.new_ref_array(comp_id, length)
        }
    };
    Ok(Some(Value::Object(Some(arr))))
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

    // args[1] = int[] of per-dimension lengths.
    let dims_arr = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        // Defensive: a single Int (1-D shape) вЂ” defer to the single-dim path.
        _ => return native_array_new_array(ctx, args),
    };
    let ndims = ctx.array_length(dims_arr);
    if ndims <= 1 {
        // A single dimension behaves exactly like `newArray`; reuse it so the
        // (already well-tested) Class-mirror element-type resolution applies.
        let len = if ndims == 1 {
            ctx.get_array_element(dims_arr, 0)
        } else {
            Value::Int(0)
        };
        let one_dim = [args.first().copied().unwrap_or(Value::Object(None)), len];
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
    let comp_name = match args.first() {
        Some(Value::Object(Some(mirror))) => crate::lang_class::mirror_class_name(&*ctx, *mirror)
            .filter(|s| !s.is_empty())
            .or_else(|| ctx.read_string(*mirror))
            .map(|s| s.replace('.', "/"))
            .unwrap_or_else(|| "java/lang/Object".to_string()),
        _ => "java/lang/Object".to_string(),
    };

    // Either a primitive leaf (single-letter descriptor) or a reference leaf
    // (`LвЂ¦;`). The descriptor letter is what the nested array-class names are
    // built from.
    let (prim_et, leaf_desc) = match comp_name.as_str() {
        "int" | "I" => (Some(cratonvm_types::ArrayElementType::Int), "I".to_string()),
        "long" | "J" => (
            Some(cratonvm_types::ArrayElementType::Long),
            "J".to_string(),
        ),
        "float" | "F" => (
            Some(cratonvm_types::ArrayElementType::Float),
            "F".to_string(),
        ),
        "double" | "D" => (
            Some(cratonvm_types::ArrayElementType::Double),
            "D".to_string(),
        ),
        "boolean" | "Z" => (
            Some(cratonvm_types::ArrayElementType::Boolean),
            "Z".to_string(),
        ),
        "byte" | "B" => (
            Some(cratonvm_types::ArrayElementType::Byte),
            "B".to_string(),
        ),
        "char" | "C" => (
            Some(cratonvm_types::ArrayElementType::Char),
            "C".to_string(),
        ),
        "short" | "S" => (
            Some(cratonvm_types::ArrayElementType::Short),
            "S".to_string(),
        ),
        other => (None, format!("L{other};")),
    };

    // Reference leaf: resolve the base component class id once (used for the
    // innermost `String[]`-style array). Primitives ignore it.
    let base_ref_id = if prim_et.is_none() {
        ctx.ensure_class_initialized(&comp_name)
            .unwrap_or(cratonvm_types::ClassId::new(0))
    } else {
        cratonvm_types::ClassId::new(0)
    };

    fn build(
        ctx: &mut dyn NativeContext,
        sizes: &[usize],
        level: usize,
        prim_et: Option<cratonvm_types::ArrayElementType>,
        base_ref_id: cratonvm_types::ClassId,
        leaf_desc: &str,
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
        // array type one level down вЂ” descriptor `'['*(ndims-1-level) + leaf`.
        let comp_desc: String = "[".repeat(ndims - 1 - level) + leaf_desc;
        let comp_id = ctx
            .ensure_class_initialized(&comp_desc)
            .unwrap_or(cratonvm_types::ClassId::new(0));
        let mut arr = ctx.new_ref_array(comp_id, len);
        // Pin the parent across each sub-array allocation: under a moving
        // collector `arr` may relocate while `build` allocates.
        let pin = ctx.pin_native_root(arr);
        for i in 0..len {
            arr = ctx.read_native_pin(pin, arr);
            let sub = build(ctx, sizes, level + 1, prim_et, base_ref_id, leaf_desc)?;
            arr = ctx.read_native_pin(pin, arr);
            ctx.set_array_element(arr, i, Value::Object(Some(sub)));
        }
        ctx.unpin_native_roots(pin);
        Ok(arr)
    }

    let arr = build(ctx, &sizes, 0, prim_et, base_ref_id, &leaf_desc)?;
    Ok(Some(Value::Object(Some(arr))))
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
        ("Linkage", "DuplicateClassDefinition") => LinkageError::DuplicateClassDefinition {
            class_name: named("class_name"),
            loader: debug_string_field(body, "loader").unwrap_or_else(|| "<unknown>".to_string()),
        }
        .into(),
        ("Linkage", "NoSuchFieldError") => LinkageError::NoSuchFieldError {
            class_name: named("class_name"),
            field_name: debug_string_field(body, "field_name").unwrap_or_default(),
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
            message: format!("{} has an unsupported class file version", named("class_name")),
        }
        .into(),

        // Everything else recognised-but-unmapped keeps `ClassFormatError` —
        // today's answer — but with a readable message rather than the dump.
        //
        // KNOWN GAP, and it is the one shape in this set that HotSpot gives its
        // own type: a circular hierarchy is
        // `ClassFile(InvalidClassFile { message: "circular class hierarchy
        // detected: ..." })` here and `java.lang.ClassCircularityError` on
        // HotSpot. `LinkageError` has no `ClassCircularityError` variant — the
        // spelling does not occur anywhere in this tree — so it cannot be
        // produced from this side. Adding the variant plus its
        // `linkage_throwable` arm is nominated in the lane report.
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

    let mut bytes = Vec::with_capacity(length);
    for i in 0..length {
        match ctx.get_array_element(array, offset + i) {
            Value::Int(b) => bytes.push((b & 0xFF) as u8),
            _ => bytes.push(0),
        }
    }
    Ok(bytes)
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
        Value::Long(v) => v,
        _ => 0,
    };
    let pos = ctx.get_field(bb, pos_slot).as_int().unwrap_or(0).max(0) as usize;
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
    let absolute_off = pos
        .checked_add(offset)
        .ok_or_else(|| RuntimeError::aioobe_index_only(i32::MAX))?;
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

/// `ClassLoader.defineClass1(ClassLoader, String, byte[], int, int, ProtectionDomain, String) в†’ Class`
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
fn preload_supertypes_via_loader(ctx: &mut dyn NativeContext, loader_obj: ObjectRef, bytes: &[u8]) {
    if !crate::classloader::is_user_defined_loader(ctx, loader_obj) {
        return;
    }
    let cf = match cratonvm_reader::read_class(bytes) {
        Ok(c) => c,
        Err(_) => return, // malformed bytes вЂ” let define_class_full report it
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
    if supertypes.is_empty() {
        return;
    }
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
    let loader_ns = if loader_faithful {
        let ns = crate::classloader::loader_namespace_id(ctx, loader);
        loader = ctx.read_native_pin(p_loader, loader);
        ns
    } else {
        0
    };
    for internal in &supertypes {
        loader = ctx.read_native_pin(p_loader, loader);
        let already = if loader_faithful {
            ctx.class_id_defined_by_loader_exact(internal, loader_ns)
                .is_some()
        } else {
            ctx.class_id_by_name(internal).is_some()
        };
        if already {
            continue; // already present for this loader вЂ” define_class_full links it
        }
        let dotted = internal.replace('/', ".");
        let name_str = ctx.create_string(&dotted); // may relocate `loader`
        loader = ctx.read_native_pin(p_loader, loader);
        let p_name = ctx.pin_native_root(name_str);
        loader = ctx.read_native_pin(p_loader, loader);
        let name_str = ctx.read_native_pin(p_name, name_str);
        // Best-effort: a genuine miss/throw is swallowed here and left for
        // `define_class_full` to surface as the spec-mandated linkage error.
        let _ = ctx.invoke_virtual(
            loader,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[Value::Object(Some(name_str))],
        );
        loader = ctx.read_native_pin(p_loader, loader); // invoke may relocate
        ctx.unpin_native_roots(p_name);
    }
    ctx.unpin_native_roots(p_loader);
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
        let result = ctx.invoke_virtual(
            loader,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[Value::Object(Some(dotted_name))],
        );
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
fn classify_duplicate_define(
    ctx: &mut dyn NativeContext,
    loader_obj: ObjectRef,
    internal_name: &str,
    loader_id: u32,
    msg: &str,
) -> DuplicateDefine {
    if internal_name.is_empty() || !msg.contains("already defined") {
        return DuplicateDefine::NotDuplicate;
    }
    // `CRATONVM_DBG_DUPDEF=1` -- name every "already defined" backend error and
    // the verdict it got. This exists because the two verdicts are otherwise
    // indistinguishable from outside: a workload that never reaches
    // `ServeExisting` proves nothing about the tolerance still working, and one
    // that never reaches `SameLoaderObject` proves nothing about the refusal.
    // Both arms print, so a probe can assert it exercised the arm it claims to.
    let dbg = cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DUPDEF").is_some();
    // THE one question that may raise: did this exact object define it?
    if crate::classloader::class_defined_by_this_loader_object(ctx, loader_obj, internal_name)
        .is_some()
    {
        if dbg {
            eprintln!("[DUPDEF] SameLoaderObject {internal_name} loader_id={loader_id}");
        }
        return DuplicateDefine::SameLoaderObject;
    }
    // A namespace hit whose recorded defining loader IS this object is the same
    // fact reached the other way round — the record can be keyed by namespace
    // before the object association is made.
    if loader_id != 0 {
        if let Some(class_id) = ctx.class_id_defined_by_loader_exact(internal_name, loader_id) {
            let same = crate::classloader::defining_loader_for(ctx.vm_identity(), class_id.as_u32())
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
                ctx.vm_identity(),
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

/// The `LinkageError` a duplicate definition raises, named after the loader the
/// way HotSpot names it.
fn duplicate_define_error(
    ctx: &mut dyn NativeContext,
    loader_obj: ObjectRef,
    internal_name: &str,
) -> MethodCallFailed {
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
            for entry in ctx.capture_stack_trace(0) {
                eprintln!("    at {}.{}", entry.class_name, entry.method_name);
            }
        }
    }

    let byte_array = match args.get(2) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("defineClass1: byte[] is null".to_string()),
            }
            .into());
        }
    };

    let offset = read_define_class_nonnegative_int(args, 3)?;
    let length = read_define_class_nonnegative_int(args, 4)?;
    let bytes = read_byte_array_define_class_slice(ctx, byte_array, offset, length)?;
    validate_classfile_header(&name, "defineClass1", &bytes)?;

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
    if cratonvm_classloading::is_bootstrap_appended_class(&name) {
        if let Some(cid) = ctx.class_id_by_name(&name) {
            return Ok(Some(Value::Object(Some(ctx.get_class_mirror(cid)))));
        }
    }

    // Real JDK ClassLoader instances have no VM-owned integer field. Derive a
    // namespace solely through the stable loader-identity map; reading a raw
    // instance slot here can mistake an unrelated JDK field for a loader id.
    let loader_id = match args.first() {
        Some(Value::Object(Some(loader_obj)))
            if crate::classloader::is_user_defined_loader(ctx, *loader_obj)
                && (crate::classloader::loader_aware_resolution()
                    || name.is_empty()
                    || ctx.class_id_by_name(&name).is_some()) =>
        {
            crate::classloader::loader_namespace_id(ctx, *loader_obj)
        }
        _ => 0,
    };
    // The caller's ProtectionDomain, decoded through the ONE reader that
    // understands both PD shapes. Six copies of an inline decode used to
    // stand here, and all six read `CodeSource.location` with
    // `read_string` -- which fails on a real `java.net.URL`, a different
    // concrete class -- so every real-JDK-constructed CodeSource silently
    // lost its URL and the defined class came back carrying the
    // synthesised `file:/runtime-defined/<name>.class` instead of the
    // caller's. See `extract_pd_code_source_url`.
    let pd_url = match args.get(5) {
        Some(Value::Object(Some(pd))) => crate::classloader::extract_pd_code_source_url(ctx, *pd),
        _ => None,
    };

    // JVMS В§5.3.5 вЂ” resolve direct supertypes through the DEFINING loader before
    // linking. `define_class_full` resolves the superclass/interfaces only via the
    // global classpath; a user loader whose classes are invisible there (Tomcat's
    // `WebappClassLoader` serving `/WEB-INF/lib` jars) would otherwise fail to
    // define a class whose super lives in the same jar (JSTL `JstlCoreTLV` в†’
    // `JstlBaseTLV`). No-op for built-in/app-loader defines.
    if let Some(Value::Object(Some(loader_obj))) = args.first() {
        preload_supertypes_via_loader(ctx, *loader_obj, &bytes);
    }

    let opts = cratonvm_native_api::DefineClassFull {
        code_source_url: pd_url,
        ..Default::default()
    };
    match ctx.define_class_full(&name, &bytes, loader_id, opts) {
        Ok(class_id) => {
            let mirror = ctx.get_class_mirror(class_id);
            // The JDK's Class.getClassLoader bytecode reads this instance
            // field directly. Keep it aligned with the VM's loader registry.
            if let Some(Value::Object(Some(loader_obj))) = args.first() {
                crate::classloader::register_defining_loader(ctx.vm_identity(), class_id.as_u32(), *loader_obj);
                ctx.set_field_by_name(mirror, "classLoader", Value::Object(Some(*loader_obj)));
            }
            Ok(Some(Value::Object(Some(mirror))))
        }
        Err(msg) => {
            if let Some(Value::Object(Some(loader_obj))) = args.first() {
                match classify_duplicate_define(ctx, *loader_obj, &name, loader_id, &msg) {
                    DuplicateDefine::SameLoaderObject => {
                        return Err(duplicate_define_error(ctx, *loader_obj, &name));
                    }
                    DuplicateDefine::ServeExisting(mirror) => {
                        return Ok(Some(Value::Object(Some(mirror))));
                    }
                    DuplicateDefine::NotDuplicate => {}
                }
            }
            tracing::warn!("ClassLoader.defineClass1({name}) failed: {msg}");
            Err(define_class_linkage_error(&name, "defineClass1", msg))
        }
    }
}

/// `ClassLoader.defineClass2(ClassLoader, String, ByteBuffer, int, int, ProtectionDomain, String) -> Class`
pub(crate) fn native_classloader_define_class2(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
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
    validate_classfile_header(&name, "defineClass2", &bytes)?;

    let loader_id = match args.first() {
        Some(Value::Object(Some(loader_obj)))
            if crate::classloader::is_user_defined_loader(ctx, *loader_obj)
                && (crate::classloader::loader_aware_resolution()
                    || name.is_empty()
                    || ctx.class_id_by_name(&name).is_some()) =>
        {
            crate::classloader::loader_namespace_id(ctx, *loader_obj)
        }
        _ => 0,
    };

    // The caller's ProtectionDomain, decoded through the ONE reader that
    // understands both PD shapes. Six copies of an inline decode used to
    // stand here, and all six read `CodeSource.location` with
    // `read_string` -- which fails on a real `java.net.URL`, a different
    // concrete class -- so every real-JDK-constructed CodeSource silently
    // lost its URL and the defined class came back carrying the
    // synthesised `file:/runtime-defined/<name>.class` instead of the
    // caller's. See `extract_pd_code_source_url`.
    let pd_url = match args.get(5) {
        Some(Value::Object(Some(pd))) => crate::classloader::extract_pd_code_source_url(ctx, *pd),
        _ => None,
    };

    if let Some(Value::Object(Some(loader_obj))) = args.first() {
        preload_supertypes_via_loader(ctx, *loader_obj, &bytes);
    }

    let opts = cratonvm_native_api::DefineClassFull {
        code_source_url: pd_url,
        ..Default::default()
    };
    match ctx.define_class_full(&name, &bytes, loader_id, opts) {
        Ok(class_id) => {
            let mirror = ctx.get_class_mirror(class_id);
            if let Some(Value::Object(Some(loader_obj))) = args.first() {
                crate::classloader::register_defining_loader(ctx.vm_identity(), class_id.as_u32(), *loader_obj);
                ctx.set_field_by_name(mirror, "classLoader", Value::Object(Some(*loader_obj)));
            }
            Ok(Some(Value::Object(Some(mirror))))
        }
        Err(msg) => {
            if let Some(Value::Object(Some(loader_obj))) = args.first() {
                match classify_duplicate_define(ctx, *loader_obj, &name, loader_id, &msg) {
                    DuplicateDefine::SameLoaderObject => {
                        return Err(duplicate_define_error(ctx, *loader_obj, &name));
                    }
                    DuplicateDefine::ServeExisting(mirror) => {
                        return Ok(Some(Value::Object(Some(mirror))));
                    }
                    DuplicateDefine::NotDuplicate => {}
                }
            }
            tracing::warn!("ClassLoader.defineClass2({name}) failed: {msg}");
            Err(define_class_linkage_error(&name, "defineClass2", msg))
        }
    }
}

/// `ClassLoader.defineClass0(ClassLoader, Class, String, byte[], int, int, ProtectionDomain, boolean, int, Object) в†’ Class`
///
/// JDK 21+ variant of defineClass with additional flags. WP2.3:
/// shares the same backend via `define_class_full`. The `flags`
/// argument is decoded (bit 0 = HIDDEN, bit 1 = STRONG, bit 2 =
/// NESTMATE) and translated into options.
pub(crate) fn native_classloader_define_class0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: loader(0), lookup(1), name(2), bytes(3), offset(4),
    //       length(5), pd(6), init(7), flags(8), classData(9)
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
    validate_classfile_header(&name, "defineClass0", &bytes)?;

    let loader_id = match args.first() {
        Some(Value::Object(Some(loader_obj)))
            if crate::classloader::is_user_defined_loader(ctx, *loader_obj)
                && (crate::classloader::loader_aware_resolution()
                    || name.is_empty()
                    || ctx.class_id_by_name(&name).is_some()) =>
        {
            crate::classloader::loader_namespace_id(ctx, *loader_obj)
        }
        _ => 0,
    };

    // The caller's ProtectionDomain, decoded through the ONE reader that
    // understands both PD shapes. Six copies of an inline decode used to
    // stand here, and all six read `CodeSource.location` with
    // `read_string` -- which fails on a real `java.net.URL`, a different
    // concrete class -- so every real-JDK-constructed CodeSource silently
    // lost its URL and the defined class came back carrying the
    // synthesised `file:/runtime-defined/<name>.class` instead of the
    // caller's. See `extract_pd_code_source_url`.
    let pd_url = match args.get(6) {
        Some(Value::Object(Some(pd))) => crate::classloader::extract_pd_code_source_url(ctx, *pd),
        _ => None,
    };

    // `init` (boolean) at arg 7: run <clinit> after define.
    let initialize = matches!(args.get(7), Some(Value::Int(v)) if *v != 0);
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

    // If hidden, mangle the name uniquely.
    let (effective_name, override_name) = if hidden {
        let id = crate::classloader::HIDDEN_CLASS_COUNTER
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mangled = format!("{name}/0x{id:x}");
        (mangled.clone(), Some(mangled))
    } else {
        (name.clone(), None)
    };

    // Resolve nest-host name when NESTMATE is set: the lookup class
    // at arg 1 supplies the nest host.
    let nest_host_class_name = if nestmate {
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

    let opts = cratonvm_native_api::DefineClassFull {
        override_name,
        hidden,
        code_source_url: pd_url,
        initialize,
        nest_host_class_name,
        ..Default::default()
    };
    match ctx.define_class_full(&effective_name, &bytes, loader_id, opts) {
        Ok(class_id) => {
            let mirror = ctx.get_class_mirror(class_id);
            if let Some(Value::Object(Some(loader_obj))) = args.first() {
                crate::classloader::register_defining_loader(ctx.vm_identity(), class_id.as_u32(), *loader_obj);
                ctx.set_field_by_name(mirror, "classLoader", Value::Object(Some(*loader_obj)));
            }
            Ok(Some(Value::Object(Some(mirror))))
        }
        Err(msg) => {
            // `hidden` is excluded on purpose and always was: a hidden class
            // is never registered under its name, so "already defined" cannot
            // be about it, and JVMS §5.3.5's rule does not apply to a class
            // that has no binary name in any loader's namespace.
            if !hidden {
                if let Some(Value::Object(Some(loader_obj))) = args.first() {
                    match classify_duplicate_define(
                        ctx,
                        *loader_obj,
                        &effective_name,
                        loader_id,
                        &msg,
                    ) {
                        DuplicateDefine::SameLoaderObject => {
                            return Err(duplicate_define_error(ctx, *loader_obj, &effective_name));
                        }
                        DuplicateDefine::ServeExisting(mirror) => {
                            return Ok(Some(Value::Object(Some(mirror))));
                        }
                        DuplicateDefine::NotDuplicate => {}
                    }
                }
            }
            tracing::warn!("ClassLoader.defineClass0({effective_name}) failed: {msg}");
            Err(define_class_linkage_error(
                &effective_name,
                "defineClass0",
                msg,
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// jdk.internal.perf.Perf natives (C27)
//
// The JDK's performance-counter infrastructure uses `Perf.getPerf().createLong(...)`
// to register internal counters. We don't track perf counters, so we return
// benign defaults вЂ” empty/zero-filled direct ByteBuffers for createLong /
// createByteArray (so callers can still write into them), zeros / no-ops for
// the rest. Perf counters in the real JDK only drive diagnostic output; no
// program correctness depends on their values.
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
    _args: &[Value],
) -> MethodCallResult {
    // private attach0(int) -> ByteBuffer
    ctx.invoke(
        "java/nio/ByteBuffer",
        "allocateDirect",
        "(I)Ljava/nio/ByteBuffer;",
        &[Value::Int(0)],
    )
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
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // highResCounter() -> long nanos-since-start
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    let nanos = start.elapsed().as_nanos() as i64;
    Ok(Some(Value::Long(nanos)))
}

pub(crate) fn native_perf_high_res_frequency(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // highResFrequency() -> 1_000_000_000 (ticks per second, we use nanoseconds).
    Ok(Some(Value::Long(1_000_000_000)))
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
        let r = native_thread_sleep0(&mut ctx, &[Value::Long(10)]);
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
        let r = native_thread_sleep0(&mut ctx, &[Value::Long(2000)]);
        let elapsed = start.elapsed();
        assert!(r.is_err(), "sleep0 with interrupt must throw");
        match r {
            Err(cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    cratonvm_types::error::RuntimeError::InterruptedException,
                ),
            )) => { /* expected */ }
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
    }

    #[test]
    fn finalizer_register_with_null() {
        let mut ctx = mock_ctx();
        let r = native_finalizer_register(&mut ctx, &[Value::Object(None)]);
        assert!(r.is_ok()); // null is silently ignored
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
        let result = native_runtime_exec_array(
            &mut ctx,
            &[Value::Object(None), arr],
        );
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
        let runtime_instance = try_alloc_concurrent_synthetic(&mut ctx, "java/lang/Runtime", 0).unwrap();
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
            for artifact in ["class_name:", "message:", "Linkage(", "Runtime(", "ClassFile("] {
                assert!(
                    !text.contains(artifact),
                    "a Debug artifact {artifact:?} reached the Java-visible error \
                     for {rendered}: {text}"
                );
            }
        }
    }

    #[test]
    fn a_prohibited_package_stays_a_security_exception() {
        let backend = as_backend_string(VmError::Runtime(RuntimeError::SecurityException {
            message: "Prohibited package name: java.evil".to_string(),
        }));
        match define_class_linkage_error("java/evil/X", "defineClass1", backend) {
            MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::SecurityException {
                message,
            })) => {
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
                matches!(&failed, MethodCallFailed::InternalError(VmError::Linkage(_))),
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
/// `SHUTDOWN_HOOKS` and `SHUTDOWN_IN_PROGRESS` are PROCESS-global (W7-92 §9.5
/// nominates fixing that) and `cargo test` runs these on threads of one
/// process, so every test that touches either takes `hook_registry_guard()`
/// and restores what it changed. Without that, one test flipping
/// `SHUTDOWN_IN_PROGRESS` makes a sibling assert the wrong exception.
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

    /// Serialises every test in this module against the two process-global
    /// statics. Poisoning is ignored on purpose: a panicking test must not
    /// turn the rest of the module red for the wrong reason.
    fn hook_registry_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Put the two globals back into the state a fresh VM has.
    fn reset_registry() {
        SHUTDOWN_IN_PROGRESS.store(false, std::sync::atomic::Ordering::SeqCst);
        SHUTDOWN_HOOKS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    fn hook_count() -> usize {
        SHUTDOWN_HOOKS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
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
        let mut ctx = mock_ctx();

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
        let mut ctx = mock_ctx();

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
        SHUTDOWN_IN_PROGRESS.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut ctx = mock_ctx();

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
        let mut ctx = mock_ctx();
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
}
