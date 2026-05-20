# Round-9 VM Performance Review

Scope: `vm/` crate. (A) round-8 regressions, (B) residual HIGH/MED, (C) new.

## (A) Round-8 regression audit

### CRIT-1 — `class_init_state_handle` fast path still hits RwLock
`classloading/src/class_manager.rs:3005-3010` (called from
`vm/src/vm/vm_util.rs:92-101`). The comment claims "single atomic load — no
lock"; the impl ALWAYS acquires `init_states.read()` first and only falls
back to `class.init_state` if the side-table has no entry. But
`set_class_init_state → class_init_state_handle → write-lock insert` mints
a side-table entry for every class touched during init, so the side-table
branch is permanent for every hot class — the per-class embedded
`init_state: Arc<AtomicU8>` is dead code. Fix: probe
`class_store.get(class_id).init_state` first, fall back to the side-table
only for pre-registration synthetic classes; stop auto-populating the
side-table in `set_class_init_state`.

### CRIT-2 — `take_jit_pending_npe` discarded on OSR return
`vm/src/runtime/interpreter.rs:11488`. `let _ = take_jit_pending_npe();`
throws the flag away. If OSR-executed code hit a null array via a
void-return store helper (`jit_iastore`/`jit_bastore`/`jit_aastore`), the
fall-back interpreter re-execution from the OSR entry PC may take a
different path (different branch profile, hoisted load was the null source)
and silently lose the NPE. Fix: `if take_jit_pending_npe() { return Err(NPE) }`
— mirror sites at lines 2228 and 12680.

### CRIT-3 — Inline null-check stub relies on undocumented helper ABI
`jit/src/x64.rs:7603-7635`. The stub zeros only the array_ptr arg register,
calls `helpers.bastore`, then `MOV RAX, i64::MIN`. Correct only because
`jit_bastore` returns void (RAX clobber harmless) AND its null path doesn't
touch index/val regs. Pin this in `vm/src/jit/helpers.rs:729` with a
"`jit_bastore` MUST short-circuit on null without reading index/val" comment
and a `debug_assert!(array_ptr != 0 || index == 0)` invariant — otherwise a
future "harmless" reorder of the null check in the helper silently breaks
the stub.

### HIGH-4 — `jni_get_field_id` cache key uses `""` for descriptor
`vm/src/native/jni.rs:1496-1525`. Field cache reuses the method
`LinkResolver` with descriptor `""`. Today methods never have empty
descriptors so no collision, but the structural assumption is fragile.
Split into a dedicated `field_cache: RwLock<HashMap<(ClassId, Arc<str>),
ResolvedMember>>` on `LinkResolver`.

## (B) Remaining HIGH/MED

### HIGH-5 — `class_manager` is `std::sync::RwLock`, not `parking_lot`
`vm/src/vm/vm_init.rs:242`. Poisoning RwLock — every JNI `GetMethodID`
takes `class_manager.read()` (round-8 LinkResolver wiring at
`vm/src/native/jni.rs:1046-1076`); convoys against concurrent class
loaders. Swap to `parking_lot::RwLock`, matching the round-8 `init_states`
fix.

### HIGH-6 — Exception alloc takes `class_manager.write()` per throw
`vm/src/runtime/exceptions.rs:82-90`. `load_class()` write-locks every
time, even for the seven canonical RuntimeException classes. Cache their
`ClassId`s on `SharedVm` in a `OnceLock<[ClassId; 7]>` populated at
bootstrap; JIT NPE materialization (`interpreter.rs:12680`) is the hottest
caller.

### MED-7 — VT `schedule_wakeup` spawns one OS thread per `Thread.sleep`
`vm/src/threading/virtual_threads.rs:811-825`. Already TODO'd in code;
correctness fine, perf gap is enormous on sleep-heavy VT workloads. Land
the single-timer-wheel design referenced in the TODO.

### MED-8 — JIT direct-call >4 args still stubbed
`TODO(round-8)`. Everything with 5+ args (most Spring lambdas) falls back
to `jit_invoke_dispatch` which allocates a `Vec<Value>`. Emit RSP-relative
MOVs after the register-arg block; spill-slot accounting already exists.

## (C) New angles

### MED-9 — `Vm::new` does sync JFR emit under `flight_recorder.lock()`
`vm/src/vm/vm_init.rs:3538-3554`. With JFR on, blocks startup on a global
mutex for a non-essential telemetry write. Defer to the JFR worker once a
recording begins.

### MED-10 — `Frame::new` re-allocates padded bytecode per call
`vm/src/runtime/frame.rs:446-481`. Hot interpreter sites use
`new_from_arcs`; reflection / JNI / `MethodHandle.invoke` still call `new`
which runs `padded_bytecode(&code)` (allocates a fresh `Arc<[u8]>`).
Migrate the remaining callers — they already hold an `Arc<[u8]>` via
cached `ClassFileMethod.code`.

### LOW-11 — `LinkResolver::resolve_or_compute` double-inserts on race
`classloading/src/resolution.rs:592-608`. Race losers still pay the Arc
intern + insert. Use `raw_entry_mut().or_insert_with`.

### LOW-12 — No production IC hit/miss counter
Test exists (`jit_integration.rs:1120`) but no `AtomicU64` on `SharedVm`
nor JFR field. Add `ic_hit` / `ic_miss` counters for MIC/PIC capacity
planning without recompiling.
