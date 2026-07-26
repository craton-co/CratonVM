# Observability audit — JFR, JVMTI, serviceability/attach, java.lang.instrument, HPROF

**Date:** 2026-07-26
**Scope:** `jfr/src/**`, `vm/src/runtime/jvmti.rs`, `vm/src/runtime/serviceability.rs`,
`vm/src/runtime/instrument.rs`, `vm/src/runtime/hprof.rs`
**Base:** `arch/wave1-integration-20260726` merged at `18b2f49d5e1b90857acb3aa0fb5c905918f962d3`

This wave found several subsystems that were documented as working while being
dead or inert (`jit/src/pgo.rs`, `jmm.rs`, `varhandle.rs`, `gc/src/metaspace.rs`,
`gc/src/class_unloading.rs`, `numa.rs`, `virtual_scheduler.rs`, `SoftReference`
clearing). The observability stack is the same shape of code. **Liveness was
established first, per subsystem, before any code change.**

---

## 1. Liveness table

| Subsystem | Reachable on a default build? | Produces real data? | Data trustworthy? |
|---|---|---|---|
| **JFR emit surface** (`jfr/src/builtin.rs`, ~30 VM call sites) | Compiled in and called — but every `emit_*` returns early on `is_enabled()`, which is **permanently false** | **No** | n/a |
| **JFR recording lifecycle** (`new_recording` / `start_recording`) | **No** — zero callers outside `#[cfg(test)]`; no `-XX:StartFlightRecording`; no working jcmd verb | No | n/a |
| **JFR file writer** (`dump_to_file`) | **No** — no caller outside the `jfr` crate | n/a | Format is a documented **bespoke** encoding, not stock JFR: not loadable by JMC / `jfr print` |
| **JVMTI event delivery** (`runtime/jvmti.rs`) | Yes — global manager installed from `SharedVm::new`; `fire_*` driven by interpreter / classloading / GC hooks | Fires; in-tree/test listeners always worked, and a real native agent now receives 9 of ~26 event kinds via the D14 bridge | Yes — see D1, D2, D14 |
| **JVMTI native agents** (`-agentpath:`) | Yes, via `vm/src/jvmti/agent.rs` (real `dlopen` + `Agent_OnLoad`), feature `experimental-debug` (default-on) | Yes | Agents reach a **different** `EventManager`, now bridged (D14) for VMInit/VMDeath/ThreadStart/ThreadEnd/ClassLoad/ClassPrepare/GC-start-finish/ObjectFree; `JvmtiCapabilities::potential()` was corrected to advertise `false` for the event kinds that remain unbridged, so `AddCapabilities` no longer over-promises |
| **JVMTI `GetLocalVariable*`** | Yes (capability advertised) | **No** — reads an agent-written side table, never real frames | n/a — see D2 |
| **jcmd / attach surface** (`runtime/serviceability.rs`) | **No** — `AttachListener` opens no socket; `JcmdProcessor` constructed only in tests | No | Several handlers returned **fabricated** data — see D3, D4 |
| **`java.lang.instrument`** (`runtime/instrument.rs`) | **Yes** — natives registered in both the synthetic and real-JDK paths; self-attach (`ByteBuddyAgent.install()`) supported | Yes | Yes, after D5 |
| **HPROF heap dump** (`runtime/hprof.rs`) | **Yes**, one trigger: `-XX:+HeapDumpOnOutOfMemoryError`, once per VM lifetime | Yes | **No** before this audit — see D6, D7, D8 |

### Which subsystems produce trustworthy data today

* **`java.lang.instrument`** — the only observability subsystem that is both
  live and correct. It is the one operators actually exercise (Mockito inline
  mock maker, JaCoCo, ByteBuddy self-attach).
* **HPROF** — live, and now correct after the fixes below. It was not before.
* **JVMTI** — the interpreter/GC/classloading-sourced event manager
  (`runtime/jvmti.rs`) is correct for in-tree/test listeners (D1, D2 fixed);
  a real native agent (`vm/src/jvmti/`) now receives 9 of ~26 event kinds via
  a bridge (D14) with an honest, correspondingly-narrowed capability set —
  full unification of the two implementations remains open.
* JFR captures nothing (D12, open); jcmd/attach is unreachable from another
  process (D15, open).

---

## 2. Defects

### D1 — `ClassLoad`/`ClassPrepare` fire under the class-manager write guard; a native agent would self-deadlock — **FIXED**

*Confirmed.* `classloading/src/class_manager.rs` calls `fire_class_load_hook` /
`fire_class_prepare_hook` from inside `define_class_shared_with_options`'s
`&mut self` region — the L10 `ClassManager` write lock is held for the whole
callback. That file's own comment claims the callback "never re-enters the
class manager", which is true only because every listener today is an in-tree
Rust function pointer.

**Failure scenario:** a real agent's `ClassLoad` handler calls
`GetClassSignature` / `GetLoadedClasses` / `RetransformClasses` — all of which
read the class manager. The thread blocks on an exclusive lock it already
holds: a hard self-deadlock that takes class loading down for the whole VM.

Also confirmed: **both call sites pass a literal `0` for the thread id**, so
every `ClassLoad`/`ClassPrepare` event reports the wrong `jthread`. Agents key
on that field to skip classes loaded by their own instrumentation thread and
avoid unbounded recursion.

Fixed (2026-07-26): `fire_class_load_hook`/`fire_class_prepare_hook`
(`classloading/src/class_manager.rs`) now queue the event on a thread-local
instead of invoking the installed hook synchronously; the queue drains only
after the L10 write guard is released, via a new `ClassRealm::class_manager_write()`
accessor (`vm/src/vm/realms/class_realm.rs`) that is now the *sole* way to
take that lock in the workspace — every prior `.class_manager.write()` call
site was mechanically switched to it, so no site can bypass the drain. A
listener may now safely call back into the class manager. Thread id is real
(bound once per Java thread at registration via
`cratonvm_classloading::set_current_thread_id`, main thread / `Thread.start`
workers / foreign JNI attach) rather than a hardcoded 0. See
`vm/src/runtime/jvmti.rs`'s `fire_class_load` doc comment. New tests:
`jvmti_fire_hooks_dispatch_when_installed` (updated for deferred firing),
`jvmti_current_thread_id_defaults_to_zero_and_is_settable`.

### D2 — `GetLocalVariable*` reads a side table, not real frames — **PARTIALLY FIXED (capability negotiation now honest)**

*Confirmed.* `JvmtiEnv::local_variables` is a
`HashMap<(ThreadId, u32), HashMap<u32, LocalValue>>` written only by
`set_local_variable_table` / `set_local_*` on the same `JvmtiEnv`. Nothing in
the interpreter, the JIT deopt path, or the stack walker writes to it. On a
live VM every `get_local_*` returned `JVMTI_ERROR_NO_MORE_FRAMES` — while
`JvmtiCapabilities::potentially_available()` advertised
`can_access_local_variables: true`.

Implementing this faithfully needs a per-slot *kind* (int/long/float/double/
ref), because `GetLocalInt` on a reference slot must return `TYPE_MISMATCH`
rather than a reinterpreted pointer. The verifier type maps in
`classloading/src/type_maps.rs` are **oop-vs-not only** — they answer the
GC's question ("is slot 3 a reference?") but cannot separate an `int` slot
from a `float` slot, nor identify the upper half of a `long`. So the type
maps are *not* sufficient to wire this up. The remaining options are the
optional `LocalVariableTable` class-file attribute (absent from most release
builds) or a widened slot-kind map — both larger, separate undertakings. That
part is **still open** and was out of scope for this pass.

**What was fixed (2026-07-26):** the failure scenario this defect actually
warned about — "a debugger negotiates the capability, is told local
inspection works, and then finds every frame empty" — is closed.
`JvmtiCapabilities::potentially_available()` now reports
`can_access_local_variables: false`, and `add_capabilities` rejects a request
for it with `NotAvailable` rather than silently granting it. A caller that
checks potential capabilities before requesting (the JVMTI-spec-correct
order) is told up front, not left to discover empty frames on its own. The
capability check was removed from the ten `get_local_*`/`set_local_*`
methods (it could otherwise never be satisfied again), so the side table
keeps working exactly as before as the embedder/test surface it always was —
it was never real JVMTI local-variable access, and gating it behind a
now-permanently-unavailable capability would have made even that unusable.

Documented in-file; the original contract pin
(`obsaudit_get_local_reads_side_table_not_real_frames`) still pins the
"side table, not real frames" behavior (now without a capability
negotiation step). New: `obsaudit_local_variable_capability_is_honestly_
unavailable` pins the capability-honesty half.

### D3 — `jcmd JFR.start` / `JFR.stop` / `JFR.dump` reported success without touching the recorder — **FIXED**

All three handlers returned `CommandResult::ok("Flight recording started: {name}")`
and friends. None called `new_recording`, `start_recording`, `stop_recording`,
or `dump_to_file`. `JFR.dump` reported a file path it never created. The
existing tests asserted the fake success strings, which would have carried the
lie through any future wiring.

**Failure scenario:** operator runs `JFR.start`, is told the recording started,
reproduces a production incident, runs `JFR.dump`, is told the file was
written — and finds nothing. The incident window is gone.

Fixed: the three verbs now return `CommandResult::err` naming the missing
binding. New test `obsaudit_jcmd_jfr_verbs_do_not_claim_false_success`.

### D4 — `jcmd Compiler.queue` returned a fabricated compile queue — **FIXED**

The handler returned a hard-coded listing naming
`java.lang.String.hashCode()I (tier 1)` … `com.example.Main.hotLoop()V (tier 4)`.
Nothing consulted the JIT. `com.example.Main.hotLoop` exists in no workload.

**Failure scenario:** an operator diagnosing a compile storm reads invented
method names as ground truth.

Fixed: returns an honest "not implemented". New test
`obsaudit_jcmd_compiler_queue_does_not_fabricate`.

### D5 — retransform could be seeded from a *different class's* bytes — **FIXED**

`retransformClasses0` seeds the transformer chain from
`original_class_bytes`, which prefers `ClassManager::class_bytes_cache`. That
cache is **FIFO-evicted at a 16 MiB soft cap**
(`DEFAULT_CLASS_BYTES_CACHE_CAP`), which any real application blows through
during startup — so by the time Mockito's inline mock maker or JaCoCo calls
`retransformClasses` (late, on demand), the target's bytes are very often
evicted. The fallback reads `find_resource("<name>.class")` off the
**application classpath**, which is not the class's defining loader, and the
result was used unconditionally.

**Failure scenario:** two loaders each define `com/foo/Bar`, or a shaded jar
carries a `com/foo/Bar.class` shadowing the loaded one. The chain is seeded
with a *different class body*, the transformer instruments that, and
`retransform_class` installs the result over the live class. A silently wrong
class definition, with no signal that the cache missed.

Fixed: fallback bytes are now parsed for their `this_class` name and rejected
unless they describe the requested class; the cache miss is logged. An empty
result makes the caller skip the class, which is the safe outcome. New helper
`class_file_this_class` with four tests, including long/double double-slot
handling and the shadowed-resource case.

### D6 — HPROF instance field values were written in the wrong order — **FIXED (format-breaking)**

The HPROF spec pins INSTANCE_DUMP's payload as
"instance field values (this class, followed by super class, etc)" — **leaf
first**. HotSpot's `DumperSupport::dump_instance_fields` walks `o->klass()`
then `java_super()`; MAT and VisualVM decode in that same order, pairing bytes
against each class's *own* declared field list (which `write_class_dumps`
already emits own-fields-only, correctly).

`write_instance_dump_with_values` serialized in `class_hierarchy_chain` order,
which is **root first**.

**Failure scenario:** every instance of a class with a field-carrying
superclass has its bytes decoded against the wrong field descriptors. Where the
two groups differ in width the misalignment cascades through the rest of the
record, so reference fields decode as garbage object IDs and MAT reports
dangling references / unknown objects. The existing tests missed it because
they dump a VM whose only loaded class is `java/lang/Object` — a single-level
hierarchy, where both orders agree.

Fixed: the *slot* walk stays root-first (CratonVM lays super fields at the low
slot indices — `compute_field_layout` sets
`first_field_index = superclass.num_total_fields`), but bytes are buffered per
class and emitted in reverse. New test
`obsaudit_instance_field_groups_emit_leaf_first`.

### D7 — every object array was labelled with its *component* class — **FIXED**

`anewarray` stores the **component** class id in the array object's header.
`write_obj_array_dump` wrote `class_obj_id_for(header.class_id)` straight into
OBJ_ARRAY_DUMP's "array class object ID" slot.

**Failure scenario:** every `String[]` in the dump is labelled
`java.lang.String`, every `Object[]` is labelled `java.lang.Object` — classes
that also have real instances. Class histograms and retained-size attribution
for arrays are unusable, and the array bytes inflate an unrelated class's
totals.

Fixed: `array_class_obj_id` resolves the real array class (`[L<component>;`)
from the class store, which already has LOAD_CLASS and CLASS_DUMP records. Falls
back to the component id when the array class has not been materialised —
still wrong, but guaranteed to have a LOAD_CLASS record; emitting a dangling ID
would make the whole dump unparseable. New test
`obsaudit_array_class_name_for_internal_form`.

### D8 — CLASS_DUMP instance size assumed HotSpot's layout — **FIXED**

The size was the sum of `hprof_type_size` over the hierarchy's instance fields:
HotSpot's packed layout, minus its header. CratonVM uses a **32-byte header
(`HEADER_SIZE`) and a 16-byte cell per instance field (`SLOT_SIZE`)** — the
audit prompt's hint, confirmed in `types/src/heap_types.rs`.

**Failure scenario:** a class with eight `boolean` fields was reported as
8 bytes when it occupies 160. An operator chasing a leak sizes the wrong
objects by more than an order of magnitude, and MAT's shallow/retained columns
are meaningless.

Fixed: reports `HEADER_SIZE + n_fields * SLOT_SIZE`. HPROF places no constraint
on this `u32` beyond "the instance's size in bytes" — readers decode field
values from the declared field list, not from this number — so the true
footprint is both spec-legal and the only useful answer. New test
`obsaudit_instance_byte_size_uses_real_footprint`.

### D9 — HPROF staged up to 1 GiB in memory on the OOM path — **FIXED**

`MAX_SEGMENT_SIZE` was `1 << 30`, and the dumper accumulated sub-records in a
single `Vec<u8>` until that threshold. The only production caller is
`-XX:+HeapDumpOnOutOfMemoryError`.

**Failure scenario:** the diagnostic demands a ~1 GiB contiguous native
allocation at exactly the moment the process is out of memory, turning a
diagnosable OOM into an allocation failure inside the diagnostic.

Fixed: 8 MiB (HotSpot flushes at 1 MiB). New test
`obsaudit_segment_threshold_is_bounded`.

### D10 — HPROF `write_gc_roots` was quadratic — **FIXED**

`alive_thread_objects(usize::MAX)` was called *inside* the per-thread loop,
re-snapshotting and re-allocating the whole registry once per thread. O(n²)
work and allocation on an app-server dump, on the OOM path. Hoisted.

### D11 — HPROF dumps are not taken at a safepoint — **FIXED**

`write_heap_segments` carried a comment claiming "the heap is not collected
while the HPROF dump is in progress — dump is serialized against concurrent GC
via the SharedVm's gc_barrier". Neither `dump_heap` nor
`maybe_dump_heap_on_oom` requested a safepoint or touched any GC barrier.

Fixed (2026-07-26): `dump_heap` (`vm/src/runtime/hprof.rs`) now takes the
calling thread's id and requests the same stop-the-world barrier real GC
cycles use (`GcBarrier::request_stw_counted_with_live_blocked`) before
walking the heap, via a `DumpSafepoint` RAII guard whose `Drop` releases the
barrier — with an empty, no-op pointer map, since nothing here relocates any
object — even on an early error or panic. Deliberately does *not* use the
interpreter's `stw_take_over_and_wait` forcible-freeze path for in-JIT peers
(that machinery exists so a *moving* collector can relocate objects safely
under a frozen peer; a non-moving HPROF walk doesn't need it, and skipping it
keeps this diagnostic path off the more experimental takeover code). If the
barrier is already held by a concurrent real GC, the request is declined and
the dump falls back to the pre-fix unpaused behaviour rather than trying to
join the other pause — a torn dump on that rare race is still better than the
OOM handler itself blocking. `maybe_dump_heap_on_oom` and its four call sites
in `runtime/interpreter.rs` now thread the calling thread through. Related,
still open: object IDs are raw heap addresses, so under a relocating
collector two dumps disagree and a recycled address can alias.

### D12 — JFR is unreachable; `RecordingSettings` has five inert fields — **DOCUMENTED**

`vm-cli` never calls `start_recording` (the code's own comment in
`vm_init.rs:5041` says so), there is no `-XX:StartFlightRecording`, and the
jcmd verbs were the stubs of D3. `is_enabled()` is permanently false, so the
~30 wired `emit_*` call sites are one relaxed atomic load each and capture
nothing.

Separately, `RecordingSettings::{max_age, max_size, disk, dump_on_exit,
duration}` are **read by no code**. `max_size` and `max_age` in particular are
honest-looking retention knobs that do nothing: a recording's only bound is
`EventRepository::default()`'s fixed 100 000-event ring. These must not be
surfaced as a CLI option, jcmd argument, or `jdk.jfr` API until enforced.
Contract pinned by `obsaudit_max_size_and_max_age_are_not_enforced`.

**Memory is bounded** (the audit's specific question): per-thread ring 1024
entries fixed, per-recording repository 100 000 events with eviction. One
latent hazard: `ThreadRingRegistry` reclaims retired+empty shards only from
inside `drain_all`, which runs at dump time — so reclamation never runs today.
Harmless only because the disabled gate keeps ordinary threads from registering
a shard at all; it becomes a real per-thread leak the moment a long-lived
recording starts on a thread-churning workload. Fix reclamation as part of
wiring the trigger, not after.

### D13 — `runtime::jvmti::AgentRegistry::load_agents` loads nothing — **FIXED (removed)**

No `dlopen`, no `Agent_OnLoad` symbol lookup. Every registered agent was
unconditionally marked `loaded = true` and counted a success — including
`-agentpath:/does/not/exist.so`. Only previously-registered Rust closures ran.

This was *not* the registry the VM bootstrap uses: `SharedVm::new` →
`load_startup_jvmti_agents` drives `vm/src/jvmti/agent.rs`, which does real
`libloading` loading. A repo-wide search confirmed `runtime::jvmti::AgentRegistry`
(and its `JvmtiEnv::agent_registry` field) had **zero callers outside its own
unit tests** — not wired to any bootstrap path, not part of the documented
embedding API. Rather than fix its behaviour in place (which would still
leave a same-named, same-method-shaped type sitting adjacent to the real one),
it was deleted outright: `AgentEntry`, `AgentRegistry`, `split_agent_arg`, the
`JvmtiEnv::agent_registry` field, and their 8 unit tests (including the
contract-pin `obsaudit_load_agents_does_not_actually_load_native_libraries`,
which pinned the bug's *existence* and is moot once the buggy code is gone).
Use `vm::jvmti::AgentRegistry` for anything agent-loading related.

### D14 — two parallel JVMTI implementations — **PARTIALLY FIXED (bridged)**

`vm/src/runtime/jvmti.rs` (4 710 lines) and `vm/src/jvmti/` (2 243 lines) are
both live and were **not connected**. The former owns interpreter/GC/classloading
event plumbing; the latter owns real native-agent loading and the
`create_jvmti_env` used at bootstrap. An agent attached via `-agentpath:`
received none of the events the interpreter fires — before this fix, only
`ClassLoad` and `ThreadEnd` reached it at all, each via its own ad hoc,
inconsistent call site elsewhere in `vm/` (one of which, in
`vm/src/vm/vm_init.rs`, incorrectly re-fired `ClassLoad` on every cache-hit
`load_class` call, not just new definitions).

Fixed (2026-07-26): `install_real_agent_env_bridge` (`vm/src/runtime/jvmti.rs`),
installed once from `Vm::new` as a `Weak<SharedVm>` (same idiom as
`set_process_vm` / `set_global_shared_vm_for_hooks`), forwards 9 event kinds —
VMInit, VMDeath, ThreadStart, ThreadEnd, ClassLoad, ClassPrepare,
GarbageCollectionStart/Finish, ObjectFree — from `JvmtiEventManager`'s `fire_*`
methods to the real env's `notify_*` functions, ahead of (not gated by) this
file's own listener-enabled check. Deliberately **not** bridged: MethodEntry/
MethodExit/SingleStep/Breakpoint/FramePop/FieldAccess/FieldModification
(per-bytecode/per-invocation hot paths — bridging would add a `Mutex<JvmtiEnv>`
lock to the interpreter's hottest paths for every VM, agent attached or not)
and MonitorWait/MonitorContendedEnter (per-contended-lock hot path; also
`vm/src/jvmti/mod.rs` has no `notify_monitor_waited`/
`notify_monitor_contended_entered`, so `can_generate_monitor_events` could
only ever be half-honest). `vm/src/jvmti/capabilities.rs`'s
`JvmtiCapabilities::potential()` was corrected to advertise `false` for
exactly the capabilities whose events are not bridged, so `AddCapabilities`
now reflects what an agent will actually receive instead of silently granting
capabilities that deliver nothing.

This is a bridge, not a merge — the two event enums, callback types, and
capability structs remain separate types. **Full unification (shared event
enum, shared capability set, one `JvmtiEnv`) remains open, as originally
scoped**; see the cross-owner request below, updated to reflect the bridge.

### D15 — the attach surface opens no socket — **DOCUMENTED**

`AttachListener::start_listening` only flips a `bool`; no socket, named pipe,
or `.attach_pid<N>` file is created, and `JcmdProcessor` is constructed only in
tests. `jcmd`/`jstack`/`jmap` from another process cannot connect to a
CratonVM at all. Pinned by `obsaudit_attach_listener_creates_no_socket`, with a
warning against "wiring up jcmd" by simply constructing a `JcmdProcessor` —
`register_default_commands` would then start reporting invented data.

---

## 3. Changes landed

| File | Change |
|---|---|
| `vm/src/runtime/hprof.rs` | D6, D7, D8, D9, D10, D11 fixed; module LIVENESS block; 4 tests (original pass) |
| `vm/src/runtime/serviceability.rs` | D3, D4 fixed; D15 + module LIVENESS block; 3 tests (replacing 4 that asserted the fabricated output) |
| `vm/src/runtime/instrument.rs` | D5 fixed (`class_file_this_class` validator); 4 tests |
| `vm/src/runtime/jvmti.rs` | D1 fixed (deferred hook firing + real thread id); D2 partially fixed (honest capability negotiation; frame access itself still open); D13 fixed (dead `AgentRegistry` removed, -8 tests); D14 bridged (`install_real_agent_env_bridge`, 9 event kinds); LIVENESS block updated throughout |
| `classloading/src/class_manager.rs` | D1: deferred-queue hook firing, `set_current_thread_id`/`current_thread_id`, `drain_pending_class_hooks`; 2 new tests |
| `vm/src/vm/realms/class_realm.rs` | D1: `ClassManagerWriteGuard` + `class_manager_write()`, now the sole L10 write-lock accessor workspace-wide |
| `vm/src/vm/vm_init.rs`, `vm/src/vm/vm_exec.rs`, `vm/src/native/jni.rs` | D1: bind real thread id at the 3 thread-registration sites; D14: install the bridge in `Vm::new`; removed a redundant/incorrect ad hoc `ClassLoad` notify in `vm_init.rs` |
| `vm/src/jvmti/capabilities.rs` | D14: `potential()` no longer advertises capabilities for unbridged event kinds; 2 new tests |
| `vm/src/jvmti/mod.rs` | D14: `test_full_workflow` updated (no longer requests a now-`false` capability) |
| `jfr/src/lib.rs` | D12 LIVENESS + memory-bounds block |
| `jfr/src/recording.rs` | D12 inert-field docs; 2 tests |

D1, D13, D14 rows above: built and tested on the Azure host
(`fix/observability-audit-20260726`) — `cratonvm-classloading` full suite,
`cratonvm-vm` `jvmti` test subset (117 passed after D13's removal), and
`vm/tests/lock_order_smoke` / `vm/tests/new19_module_access` all green. The
original D3–D12 rows above were not built or tested (concurrent builds OOM'd
the audit host at the time) — still true for that original work; not
re-verified in this pass.

---

## 4. Cross-owner requests

**D1 — resolved, no action needed.** See the D1 section above for what
changed (`classloading/src/class_manager.rs`, `vm/src/vm/realms/class_realm.rs`).

**To the owner of `classloading/src/type_maps.rs` (D2):**

If JVMTI local-variable inspection is ever to work, the verifier type maps need
to carry a per-slot *kind* (int / long / float / double / ref / long-upper-half),
not just oop-vs-not. Today they answer only the GC's question. No action
requested now — recorded so the requirement is visible if the maps are extended
for another reason.

**To the owner of `vm/src/jvmti/` (D14) — partially resolved.**

A one-way bridge (`install_real_agent_env_bridge` in `runtime/jvmti.rs`) now
forwards 9 event kinds to `vm/src/jvmti/`'s `EventManager`; see the D14
section above for exactly which and why not the rest. Still open, for
whoever picks this up next: full unification (one event enum, one capability
struct, one `JvmtiEnv`) so the remaining method-level tracing and monitor
events don't need a second bespoke bridge each. `vm/src/jvmti/mod.rs` also
has three `notify_*` functions with zero production callers even after this
fix (`notify_breakpoint`, `notify_method_entry`, `notify_exception`, and
others in the same file) — worth checking whether they should be wired too
before extending the bridge to cover them, since a wired-but-unfired
`notify_*` is exactly the kind of gap this whole audit exists to catch.
