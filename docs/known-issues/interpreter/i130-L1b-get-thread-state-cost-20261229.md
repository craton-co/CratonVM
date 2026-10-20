# C JVMTI `GetThreadState` costs about 40x HotSpot's on one sampler

**Status: OPEN, a first fix built behind `CRATONVM_JVMTI_THREAD_LOOKUP_VM_ONLY` (performance
switch, default OFF). Not yet compiled or measured.** Filed 2026-12-29 by interpreter round i6
wave 130, lane L1b, from the run lane's i64-L1 bench row
(`docs/internal/reviews/interpreter-i6-run-results-20261224.md`, "i64-L1 bench").

## Symptom

`tools/probes/interp/L1/L1W91JvmtiStateSamplerBench.java` (+ `.c` agent, MSVC `/O2`): a JNI native
calls `GetThreadState` over 8 parked or sleeping threads, 20,000 rounds.

| | `sampler-1` ns/call | `sampler-4` ns/call (wall, 4 samplers) |
|---|---|---|
| CratonVM, default | 6320 | 1855 |
| HotSpot 25.0.3 | 159 | 823 |

`sampler-4` divides wall time by every call made by the 4 samplers. CratonVM's drops 3.4x with 4
samplers, so each call burns about 7 us of its own thread's CPU. A global lock would not scale
like that. The cost is per-call work on the calling thread, not contention.

## Where the time goes (by reading)

One `GetThreadState(env, thread, &state)` from a JNI native's C code, with the JNI in-native
package on (`CRATONVM_JNI_NATIVE_TRANSITIONS`, default ON). In order:

1. **`live_phase_vm`** (`vm/src/jvmti/native_env.rs`):
   - `jni::hold_in_native_while_suspended`: a TLS read and a gate load;
   - `live_env`: the env's `state` mutex;
   - `JvmtiNativeEnv::vm`: the mutex again, and a `Weak::upgrade` (a CAS on the `SharedVm` strong
     count, a line every sampler writes).

   Cheap: about 50-100 ns.
2. **`lookup_thread`**, under `jni::ForeignJniEntry::enter()`. **This is the bulk.** The calling
   thread is in a JNI native in native, so `enter` is the full native -> VM -> native round trip:
   - on the way in, `native_call_leave_native`: a barrier-lock hold;
   - on the way back, in the entry's drop, **one full root deposit**
     (`NativeContextImpl::deposit_root_snapshot`). That deposit is a TLAB retire, the conservative
     JIT band scan from the deposit's SP up to the outermost compiled entry (every word probed with
     `is_object_address`), the frame roots and the `slot_origins` rebuild, the JMX monitor list,
     the process-global pinned-roots map lock with a `HashSet` allocation, and the barrier-lock
     hold of `mark_in_native_enter`.

   `docs/internal/gc/gcd-d10j-proposal-in-native-entry-without-a-deposit-DONE-20261015.md` puts
   this full round trip at about 8 us ("every other function still pays one full deposit on the way
   back in"). Only the leaf JNIEnv functions (two fences) and the `enter_vm_only` ones (two
   barrier-lock holds, an append of new locals) skip it. The JVMTI thread lookup was neither.

   Inside the entry, the lookup itself is small:
   - `replace_jni_context` / `restore_jni_context`: an `Arc` upgrade and a drop;
   - `jobject_to_obj`;
   - `read_java_thread_tid`: `is_heap_addr`, the header, the layout memo's `tid` slot, the field;
   - `find_thread_id_by_java_tid`: the registry's `java_tid_to_id` mutex;
   - `is_alive`: one `threads` read lock.
3. **`live_thread_state`**, after the entry has dropped:
   - `java_block_state`, `published_wait_kind_of` (`CRATONVM_PUBLISHED_ALL_WAIT_KINDS`, default ON:
     one atomic, no frame trace) and `get_interrupted_flag` each take the registry's `threads` read
     lock (3 acquisitions; the last also clones and drops an `Arc<AtomicBool>`, two RMWs);
   - `c_in_native_armed`, a gate load;
   - **`shared.debug.debug_state.lock()`**, the VM-global debugger mutex, for the `SUSPENDED` bit
     (unless `CRATONVM_JVMTI_STATE_SUSPENSION_HINT=1`, i64-L1). It is uncontended with no debugger
     attached, which is why the hint switch measured flat (1.02 / 0.98).

So about 6 of the 6.3 us is the deposit of item 2. Items 1 and 3 together are about 10 lock or
RMW operations, which is a few hundred ns. HotSpot's `GetThreadState` is a
`ThreadInVMfromNative` transition (two state stores and a safepoint poll) and
`JvmtiEnvBase::get_thread_state`, which reads the `JavaThread`'s fields with no lock.

## The fix built (`CRATONVM_JVMTI_THREAD_LOOKUP_VM_ONLY`, default OFF)

`lookup_thread` takes `ForeignJniEntry::enter_vm_only()` in place of `enter()`. The lookup runs no
Java, mints no local, writes no `JvmThread` field and cannot collect. It decodes a handle, reads
`Thread.tid` and asks the registry, which is the shape the tag functions' `with_object_tags`
already runs under `enter_vm_only` (gci i48 lane jl48). On a quiet leave the drop then goes back
into native incrementally, with no deposit.

- Per VM: read once into `DebuggerGates::jvmti_thread_lookup_vm_only` (`vm/src/debug/mod.rs`).
- Scope: every C JVMTI function that resolves a `jthread` through `lookup_thread` /
  `thread_id_of`. The entry still covers only the lookup. What a caller does after it is unchanged.
- Outside a JNI native in native (an event callback, an agent thread), `enter_vm_only` behaves as
  `enter` did, so nothing changes there.

Expected, not measured: `sampler-1` down to about 1 us. That is two barrier-lock holds and items
1 and 3.

## Not built (next steps if the switch wins)

- Answer `get_interrupted_flag`, `java_block_state` and `published_wait_kind_of` from **one**
  `threads` read, and read the interrupted flag without cloning its `Arc`.
- The i64-L1 per-thread suspended word, so no reader takes `debug_state` for the `SUSPENDED` bit
  while another thread is suspended.
- A leaf window (`enter_leaf`) for the decode and the `tid` read alone, with the registry asked
  after the window closes (a `tid` never aliases). This is not done here: `thread_field_slot`'s
  cold path takes the class-manager lock, and `GenHeap::is_heap_addr`'s slow arm takes a heap lock.
  A pausing thread can hold either, which a leaf window must never wait on.

## A/B commands

Windows, no debug build needed. Both modes, interleaved arms, 5 reps, P-cores
(`start /affinity FFFF`), medians of the per-run minimums:

    cl /nologo /O2 /LD /I"%JAVA_HOME%\include" /I"%JAVA_HOME%\include\win32" tools\probes\interp\L1\L1W91JvmtiStateSamplerBench.c /Fe:%TEMP%\L1W91JvmtiStateSamplerBench.dll
    javac -d %TEMP%\l1w91 tools\probes\interp\L1\L1W91JvmtiStateSamplerBench.java
    cratonvm [--compatible] -agentpath:%TEMP%\L1W91JvmtiStateSamplerBench.dll -cp %TEMP%\l1w91 L1W91JvmtiStateSamplerBench %TEMP%\L1W91JvmtiStateSamplerBench.dll
    set CRATONVM_JVMTI_THREAD_LOOKUP_VM_ONLY=1 & cratonvm [--compatible] ... (same)

Flip rule:
- `TIME sampler-1` is at most half the off arm's, in both modes;
- `TIME sampler-4` is not worse;
- `checksum sampler-1=0x2f1 sampler-4=0x2f1` and `done` in both arms.

Correctness with it on, unchanged from off:
- `L1W57JvmtiWaitStates`, `L1W64JvmtiAllWaitKinds`, `L1W58JvmtiInNative`;
- the JVMTI suspend probes;
- the C JVMTI battery (`runner/jvmti-win.sh`);
- the JNI-library gate (an agent's thread functions called from natives while a daemon loops
  `System.gc()`; the `vi11-ja` `ja11_w_tag` shape).

## Wave 132 (lane L9 review), 2026-10-19

`lookup_thread` under `enter_vm_only` is safe against thread exit. It returns
a thread id, never a `JvmThread` pointer. `registry.is_alive` is asked under
the registry's own lock, and the window between that answer and the caller's
use is the same in both arms. The function runs no Java, mints no local and
allocates nothing. `is_thread_object` takes only the class-manager read lock,
as the `enter()` arm already did. Both arms are "in VM" while the lookup runs,
so neither adds a lock a pause can hold against it. The only difference is the
re-entry into native, a quiet leave instead of a full deposit. The switch is
default OFF and is registered. No defect found.

## Wave 132 (lane L1), 2026-10-19: next step 1 built (`CRATONVM_JVMTI_THREAD_STATE_ONE_READ`)

**Status: OPEN. Built, not compiled or measured by the lane.** Of the three "Not built" items only
the first is clearly correct by reading, so only it is built:

- **Built.** `ThreadRegistry::jvmti_state_facts(id)` answers `(java_block_state, published wait
  kind, interrupted)` from ONE `threads` read. The block state comes from the same code
  (`java_block_state` now calls a shared `java_block_state_of_entry` under its own read, a pure
  move); the wait kind is `gc_block_state_wait_kind` of the same entry, asked only for a WAITING /
  TIMED_WAITING thread and only with `CRATONVM_PUBLISHED_ALL_WAIT_KINDS` (else the caller's
  `wait_kind` falls back exactly as before); the interrupted flag is loaded in place, with no `Arc`
  clone. `native_env::live_thread_state` uses it behind a new performance switch,
  `CRATONVM_JVMTI_THREAD_STATE_ONE_READ` (default OFF; `DebuggerGates::jvmti_thread_state_one_read`,
  per VM). Saves two read-lock acquisitions and two RMWs per call: a few tens of ns, small next to
  the deposit `CRATONVM_JVMTI_THREAD_LOOKUP_VM_ONLY` removes, so measure it ON TOP of that switch.
- **Not built: the per-thread suspended word.** It is i64-L1's design and touches every
  suspension mutator; not a lane-sized step.
- **Not built: the leaf window.** The page's own reason stands (`thread_field_slot`'s cold path and
  `GenHeap::is_heap_addr`'s slow arm take locks a pausing thread can hold).

**Unit test.** `threading::thread_registry::tests::the_one_read_state_facts_equal_the_three_reads`
(both all-kinds settings, every published-wait state, interrupted, an unknown thread).

**A/B.** As above, with `CRATONVM_JVMTI_THREAD_LOOKUP_VM_ONLY=1` in both arms and
`CRATONVM_JVMTI_THREAD_STATE_ONE_READ` unset vs `=1`. Flip rule: `TIME sampler-1` better by more
than the run-to-run spread in both modes, `sampler-4` not worse, `checksum ... =0x2f1` and `done`
in both arms; correctness rows `L1W57JvmtiWaitStates`, `L1W64JvmtiAllWaitKinds`,
`L1W58JvmtiInNative` unchanged with it on.
