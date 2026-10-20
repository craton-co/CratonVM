# An `Exception` event's object is collected before the debugger reads it

**Status: fixed behind `CRATONVM_JDWP_EVENT_OBJECT_HOLD` (default ON, `=0` restores the old
behaviour). Not yet compiled or run.** Filed 2026-12-28 by interpreter round i6 wave 127, lane L1,
from the run lane's w122-dbg / w124-dbg finding. Once the orchestrator has verified it, move this
page to `docs/internal/`.

## Symptom

`tools/probes/interp/L1/L1W93JdiExceptionFromCompiled.java` under `--compatible` with the JIT
failed in 4 of 4 runs, both with defaults and with `CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL=1`.
The debugger threw on the first event, and every event row was lost:

    com.sun.jdi.ObjectCollectedException
      at com.sun.tools.jdi.ObjectReferenceImpl.referenceType(ObjectReferenceImpl.java:174)
      at L1W93JdiExceptionFromCompiled$Debugger.run(L1W93JdiExceptionFromCompiled.java:176)

Line 176 is `x.exception().referenceType()`. `--jdk-only` and both `--nojit` modes differed only on
the `bench` timing line (log: `/c/cvt-i6/runs/jdi-w122/i18-extra-off.log`).

## Which object, and why it was not held

The object is the **exception** that the `ExceptionEvent` names. It is the only object id the probe
reads. JDI's `referenceType()` sends `ObjectReference.ReferenceType` (9/1), and
`inspect::or_reference_type` answers `INVALID_OBJECT` only when `object_for_id` resolves nothing.
JDI turns that answer into `ObjectCollectedException`.

1. The event is built in `interpreter::deliver_jdwp_exception_event`. It exports the throwable
   through `inspect::put_tagged_with_tag`, then `debug::export_object(.., Sent)`.
2. That export is backed by a **weak** JNI global (wave 8, as HotSpot's `commonRef` nodes).
3. The request is `SUSPEND_NONE`, so the thread runs on. The exception is caught at once
   (`L1W93XPair.caller` returns `-1`), so nothing reaches it any more.
4. The probe's `timeThrow` loop has just allocated 100k throwables. A collection then ends between
   the event packet and the debugger's first command. That collection finds the exception
   unmarked and clears the weak handle. `object_for_id` answers `None`, and JDI throws.

Nothing else drops the id. The id table's accounting was checked: `dispose`, `unpin` and
`release_all` do not touch a `Sent` entry here, and the thread's `reported_exception` handle is a
separate handle. So the only way the id resolves to nothing is the weak handle being cleared.

Why `--compatible` with the JIT, and not the other modes: the collection has to land in a window of
about one round trip, right after the allocation-heavy `timeThrow`. How the heap is laid out and
when the cycle runs depend on the mode. This has not been measured. The confirming run is under
"Verify" below.

HotSpot: its back end's `eventHelper` holds strong references to an event's objects until the packet
is written. After that its ids are weak too. A HotSpot collection does not land in that window
(the HotSpot transcript prints all 5 events), so a debugger always sees the exception.

## The fix (`CRATONVM_JDWP_EVENT_OBJECT_HOLD`, default ON)

- `debug::hold_event_object`: when an `Exception` event is built, its object is held with its
  collection disabled. This uses the same `DisableCollection` count that the debugger's own
  `ObjectReference.DisableCollection` uses, so the two nest. The handle becomes strong.
  - It is called from `deliver_jdwp_exception_event` in `vm/src/runtime/interpreter.rs`, a
    one-line cross-lane call made under the same debug-state lock as the export.
- `debug::release_event_hold_named`: the hold is released by the first heap command whose leading
  id is that object. The commands counted are those of `ObjectReference`, `StringReference`,
  `ArrayReference`, `ClassLoaderReference` and `ClassObjectReference`. It runs at the end of
  `inspect::execute`, after the command was answered, with no collection able to run. The id is
  then weak again, exactly as before the event.
- Bounds:
  - at most `EVENT_OBJECT_HOLD_CAP` (1024) holds; past that, the oldest is released;
  - the session's end drops them all;
  - a `DisposeObjects` that frees the id frees the handle as before (a later release of that id is
    a no-op).
- `=0`: no hold. The id is bare and weak, as before wave 127.

What a debugger can see differently from HotSpot: an `ObjectReference.IsCollected` that is the
**first** command to name an event's exception, sent after a collection, now answers `false`. On
HotSpot it could answer `true` if a collection had run since the packet was written. Every later
command answers exactly as before.

Unit test: `debug::tests::an_event_object_is_held_until_the_first_command_naming_it`. It checks:

- the event's object is held strong, and held only once;
- a command that names another id, or that belongs to another command set, leaves the hold;
- the first object command that names the id makes it weak again;
- `=0` holds nothing.

## Verify

    javac -g -d out tools/probes/interp/L1/L1W93JdiExceptionFromCompiled.java
    cratonvm --compatible --java-home $JDK --jdwp-port 5894 -cp out L1W93JdiExceptionFromCompiled wait &
    java -cp out L1W93JdiExceptionFromCompiled debug 5894

Pass: the debugger prints the `HOTSPOT_EXPECTED` block (all 5 rows, `events: 5`). Only the `bench`
line may differ. Run the four modes (`--jdk-only` / `--compatible`, JIT / `--nojit`), 4 runs of
`--compatible` JIT. Negative control: `CRATONVM_JDWP_EVENT_OBJECT_HOLD=0 --compatible` should
fail as before.

To confirm the cause, run the `=0` arm with `CRATONVM_GC_STATS=1 --verbose:gc` placed before the
mode flag. A collection should end between the first `Exception` event and the failing
`ReferenceType`.

    cargo test -j 5 -p cratonvm-vm --features experimental-debug --lib an_event_object_is_held_until_the_first_command_naming_it

## Not covered

Other events that name an object hold nothing yet:

- field watches (the object and the value);
- the monitor events (the monitor).

The same race can hit them under `SUSPEND_NONE`. Extending the hold means calling
`hold_event_object` where those events export their objects.

## Wave 128 (lane L9 review)

Reviewed, no code change. The HotSpot claim is right as far as it goes:

- `commonRef` nodes are weak, and `eventHelper` keeps its own global refs to an event's objects
  only until the packet is written.
- A VM-wide suspend (`threadControl_suspendAll`, so a `SUSPEND_ALL` event) additionally runs
  `commonRef_pinAll`. Every id is strong until the matching resume.
- Under `SUSPEND_NONE` / `SUSPEND_EVENT_THREAD`, nothing holds the exception after the packet.
  HotSpot "always sees it" by timing, not by contract.

So the hold is a CratonVM extension that makes the timing argument hold here. It is not a HotSpot
mechanism. Besides `IsCollected` (above), one more thing is observable to the debuggee: an event's
exception lives until the debugger first names it, or until 1023 later holds push it out. On
HotSpot it can die as soon as the packet is out. This shows only to a debuggee that watches its own
exceptions through a `WeakReference` / `Cleaner` while a debugger with an exception request is
attached. That is accepted as negligible.

The rest checked out:

- **Lock order.** It is debug state, then the JNI table, the established order.
- **Sessions.** Nothing leaks across sessions: the deque is cleared before `release_all`.
- **`DisposeObjects`.** It removes the entry whatever its disable count, so a stale id in the
  deque is a no-op.
- **Bounded cost.** The 1024 cap bounds a client that never looks. The linear `contains` costs at
  most 1024 compares per exception event.

## Wave 130 (lane L1b)

**Status unchanged: fixed behind the switch, still not compiled or run.** Checked by reading
whether the hold covers `L1W93JdiExceptionFromCompiled`'s path under `--compatible` with the JIT.
It does, so nothing was extended.

- The w124 runs that failed 4 of 4 predate wave 127. They say nothing about the hold.
- Under `--compatible` with the JIT, every JDWP `Exception` event is built in one place,
  `interpreter::deliver_jdwp_exception_event`. That includes a compiled `athrow` (reported through
  `CompiledThrowSite`, `CRATONVM_JVMTI_EXCEPTION_FROM_COMPILED`) and a compiled catch door
  (`known_handler`). `deliver_exception_events` reaches it for every throw a request may report.
  No other builder of an `EventKind::Exception` packet exists (`rg 'EventKind::Exception'` over
  `vm/src`: the rest are request matching and tests).
- In that function, the export (`put_tagged_with_tag`) and `hold_event_object` run in the same
  `debug_state` critical section. No pause can run between them: the thread is in VM code and not
  at a safepoint.
- The probe's first object command is `ObjectReference.ReferenceType` (9/1). That is in
  `CS_OBJECT_REF`, so `release_event_hold_named` releases the hold only after the reply was built.
- The mode does not change any of this. `--jdk-only` and `--compatible` share the function. Only
  the timing of the collection differs, which is what the "Verify" runs (4 runs of `--compatible`
  JIT, and the `=0` negative control with `CRATONVM_GC_STATS=1 --verbose:gc`) must show.

If `--compatible` JIT still fails with the hold on, the next suspect is an export that does not
leave a candidate at the exception's address, so `hold_event_object` returns at
`candidate_for_addr` and holds nothing. Check with a temporary `eprintln!` there.
