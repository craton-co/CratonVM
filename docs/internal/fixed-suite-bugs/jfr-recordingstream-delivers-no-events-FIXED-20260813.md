# `RecordingStream` delivers no events, and `Event.isEnabled()` stays false while one is running

**Status:** ✅ FIXED 2026-08-13, on branch
`fix/netty-jfr-known-issues-20260813` (from `origin/dev` `bd80019c8`), verified
on the Azure Linux host (`20.80.105.49`).

`io.netty.buffer.JfrEventsTest` — the consumer this page was filed against —
goes from **0/10 to 10/10**, matching stock HotSpot JDK 25. The 60-line
outside-netty probe now answers identically on both VMs:

| | HotSpot JDK 25 | CratonVM before | CratonVM after |
| --- | --- | --- | --- |
| `isEnabled()` during the stream | `true` | **`false`** | `true` |
| `shouldCommit()` during the stream | `true` | **`false`** | `true` |
| event delivered to `onEvent` | yes | **no (timeout)** | yes |
| `RecordedEvent.getInt("capacity")` | 128 | — | 128 |
| `RecordedEvent.getString("label")` | `streamed` | — | `streamed` |
| `isEnabled()` after `close()` | `false` | `false` | `false` |

**Four separate defects had to be fixed, and none alone was enough.** The first
two are the ones this page predicted; the last two were only reachable once the
first two were in place, and each independently reduced delivery to nothing:

1. **The recording state transition was missing.**
   `RecordingStream.startAsync()` was a whole-method no-op, so
   `jfr_begin_java_recording()` never ran and `Event.isEnabled()`/`shouldCommit()`
   — both of which answer `jfr_java_recording_active()` — stayed `false`.
   `startAsync()` and `start()` now perform the transition and register the
   stream, while still skipping `directoryStream.startAsync` for the original
   reason (it polls a chunk repository that does not exist and spins a core).
   `close()` ends the recording symmetrically, and only when the last started
   stream closes — two concurrent streams share one recording.

2. **There was no delivery path from `commit()` to a Java consumer.**
   `onEvent(String, Consumer)` and `onEvent(Consumer)` now capture their
   `Consumer` as a **global root** (never a bare `ObjectRef` — that is the
   hazard this page flagged, and it is real: the consumer is parked from
   `onEvent` until `close()`, arbitrarily many GCs later), and `Event.commit()`
   dispatches to every matching subscription of every started stream.

3. **`Event.commit()` captured no field values at all.** It passed only the
   event's class name to the recorder, so even a delivered event would have had
   no `capacity` to read. It now walks the event object's class chain up to
   `jdk.jfr.Event` — superclass fields first, statics skipped, shadowed names
   resolved at their most-derived declaration — and reads each field whose
   descriptor has a JFR type. netty's events depend on that chain walk:
   `AllocateChunkEvent` declares two of its fields and inherits four more from
   `AbstractChunkEvent`/`AbstractAllocatorEvent`. `begin()`/`end()`, previously
   no-ops, now carry the event's start time and duration (per thread, because
   CratonVM does not weave a `startTime` field into event classes).

4. **A second `RecordingStream` in the same process got a dead recording.**
   `jfr_begin_java_recording` returned early on `active.is_some()` without
   checking whether that recording was still running, and `Recording::start()`
   only transitions out of `New` — so after the first stream closed, every later
   `startAsync()` left the running flag `false`. This is what the netty numbers
   showed: with fixes 1–3 in place, **exactly one** of the ten tests passed —
   whichever ran first. It now discards the finished recording and begins a new
   one.

Two related defects were found by the same probes and fixed with them:

* **`JVM.getTypeId` handed out ids inside the JDK's reserved range.** The
  counter started at 1; `JVM.RESERVED_CLASS_ID_LIMIT` is 500 and
  `jdk/jfr/internal/types/metadata.bin` bakes fixed ids below it into the ~200
  built-in types, with `TypeLibrary.types` keyed by id. So
  `FlightRecorder.register(ProbeEvent.class)` reported success and
  `EventType.getEventType(ProbeEvent.class)` then answered
  `jdk.MetaspaceAllocationFailure` — somebody else's event type. Ids now start
  at 500, and that call answers `ProbeEvent` with the right field list.
* **Events committed before `stop()` never reached the dump.**
  `record_event` parks on a per-thread ring, the drain is destructive and fans
  out only to *running* recordings, and `stop_recording` did not drain — so
  `start(); commit(); stop(); dump(path)` wrote a file with the recording's
  metadata and no events. `stop_recording` now drains first.

**Why not the repository route.** With the sibling chunk-format defect fixed
(see the retired `jfr-dumped-chunk-is-not-readable-by-the-jdk-parser`
write-up), "write chunks to disk and let the JDK's own `EventDirectoryStream`
poll them" became *possible*. It was still not taken: that path needs live
append-while-reading chunk writing (a mutating header, flush checkpoints,
`UPDATING_CHUNK_HEADER` handshakes) plus a working `PlatformRecorder`/`Repository`
underneath, and it would replace a ~200-line boundary with a protocol. It
remains the route to take if JFR streaming ever needs HotSpot's exact
asynchronous semantics.

**Residual divergences, documented at the code rather than left implied:**

* **Delivery is synchronous, on the committing thread.** HotSpot delivers from
  the stream's own thread; there is no door on this native boundary to start a
  Java dispatcher thread. So a consumer runs inside the emitting call and must
  not block on it, and events arrive in commit order with no flush batching. A
  consumer that commits its own event has its inner delivery suppressed
  (recorded, not delivered) rather than recursing.
* **`start()` does not block.** On HotSpot the blocking sibling processes events
  until the stream closes; here it performs the transition and returns. That is
  still a strict improvement on what it did before, which was to reach
  `directoryStream.start` and spin a core forever.
* ~~**`Recording.enable(...)`/`disable(...)` settings are still not consumed by
  the Rust recorder.** A Java-owned recording therefore captures every event
  CratonVM emits, not the enabled subset.~~ **Closed later the same day** — see
  the settings section below.

## Settings, closed 2026-08-13

`enable(...)`/`disable(...)`/`withThreshold(...)` were accepted and stored by the
JDK and read by nobody: `disable` did nothing, a threshold did nothing, and a
Java-owned recording swept up whatever built-in events the VM emitted while it
was open. All of that is now honoured, and every row of a nine-case probe agrees
with HotSpot JDK 25.

**The rule was measured, and the first draft of the fix had it backwards.** It is
tempting to read "no `enable(...)` call" as "record nothing"; HotSpot records
BOTH of a probe's custom events for a bare `new Recording()`, and none of its own
`jdk.*` types. `jdk.jfr.Enabled` defaults to `true` for a user event class, while
the JDK ships most built-in types with `enabled=false`. So:

* a Java event class is recorded **unless** a recording explicitly disables it;
* a CratonVM built-in is recorded **only if** a recording explicitly enables it;
* "disabled" needs *every* open recording to have said so — one that does not
  mention the name applies the type's default, and HotSpot's per-type answer is
  the union across running recordings.

A draft that defaulted Java events to *off* disagreed with HotSpot on three of
the nine rows. The oracle run is what caught it; the write-up's own framing of
the gap ("an empty `Recording` dumps 1 event where HotSpot's dumps 0") had
pointed at the right symptom for the wrong reason — the extra event was a
CratonVM built-in, not a defaulting rule.

Two mechanics worth knowing when reading `native-builtins/src/jfr.rs`:

* `Recording.enable(Class)` does **not** store the class name. It stores
  `String.valueOf(Type.getTypeId(eventClass))`, so a settings key reads
  `545#enabled` and the only way back to `io.netty.AllocateChunk` is a table
  `JVM.getTypeId(Class)` fills as it hands the ids out — the `Class` mirror is
  the one thing that knows the `@Name`.
* The settings are read at `start()`/`startAsync()`. A change *after* start is
  legal but its only funnel is a private `Recording.setSetting`, so the first
  refusal of an event name re-reads the settings before answering no, and that
  answer is then memoized per VM (otherwise a disabled event would re-read the
  map on every commit).

Still not consumed, and skipped at a visible `continue` rather than silently:
`stackTrace`, `period`, `cutoff`, `throttle`, `level` and the per-event control
classes, none of which the Rust recorder models. Delivery to `onEvent` is
unaffected by any of this — it matches on the event name.

## Original report (2026-08-12)

Filed as the single remaining blocker on netty investigate-batch-02 — it failed
all 10 tests of `io.netty.buffer.JfrEventsTest`, which pass on stock HotSpot
JDK 25. Found on the Azure Linux host (`20.80.105.49`), binary built from
`origin/dev` `6d1bfd531`.

At the time this was a **known, deliberate gap**, not a regression: the stub was
documented in place in `native-builtins/src/jfr.rs`. This page existed to attach
a concrete failing consumer, an outside-netty repro, and the measured boundary
of what did and did not work.

## Symptom

```
io.netty.buffer.JfrEventsTest   HotSpot 10/10 ok in 10.5 s
                                CratonVM 0/10, all 10 fail
```

Every one fails identically:

```
java.util.concurrent.TimeoutException: pooledJfrBufferAllocation() timed out after 10 seconds
```

`JfrEventsTest` is `@Timeout(10)`, and every test has the same shape: open a
`RecordingStream`, `enable` a netty event class, register `onEvent`,
`startAsync()`, allocate a buffer, then **block** on
`CompletableFuture.get()` or `CountDownLatch.await()` waiting for the event.
The event never arrives, so each test sits until the cap. Deterministic — run
solo on an idle box it is still 0/10 in 100.7 s (10 × the 10 s cap), so this is
not the `io.netty.buffer` throughput gap.

## Reproduced outside netty

60 lines, one custom `jdk.jfr.Event`, no netty at all:

```java
try (RecordingStream stream = new RecordingStream()) {
    CompletableFuture<RecordedEvent> got = new CompletableFuture<>();
    stream.enable(ProbeEvent.class);
    stream.onEvent(ProbeEvent.NAME, got::complete);
    stream.startAsync();

    ProbeEvent e = new ProbeEvent();
    System.out.println("isEnabled=" + e.isEnabled());   // <-- the tell
    e.capacity = 128; e.begin(); e.end(); e.commit();
    got.get(8, TimeUnit.SECONDS);
}
```

| | HotSpot JDK 25 | CratonVM |
| --- | --- | --- |
| `FlightRecorder.isAvailable()` | `true` | `true` |
| `isEnabled()` **before** any recording | `false` | `false` |
| `RecordingStream.startAsync()` | ok | ok |
| `isEnabled()` **during** the stream | `true` | **`false`** |
| `commit()` | ok | ok (no-op) |
| event delivered to `onEvent` | **yes** | **no** (timeout) |

## Mechanism

Two links of the chain are missing, and the first explains the second.

**1. `startAsync()` never transitions the recording state.**
`native-builtins/src/jfr.rs` registers
`jdk/jfr/consumer/RecordingStream.startAsync()V` as a whole-method no-op. The
in-tree comment gives the reason and is worth keeping: the real body's last
statement is `directoryStream.startAsync(startNanos)`, and
`EventDirectoryStream` polls an on-disk chunk repository that nothing in
CratonVM produces — its loop has no blocking source and spins a core forever.
The first two statements (`PlatformRecording.start()`, the actual state
transition) went with it.

`Event.isEnabled()` and `shouldCommit()` are natives that both answer
`ctx.jfr_java_recording_active()`. Since `startAsync` never begins a java
recording, both answer `false`, and `commit()` returns early. Nothing is ever
recorded, let alone delivered.

Note the contrast — the **non-streaming** API does make the transition, because
`jdk/jfr/Recording.start()V` is wired to `jfr_begin_java_recording()`:

```
                          Recording (non-stream)   RecordingStream
isEnabled() while active   true                     false
shouldCommit()             true                     false
```

**2. There is no delivery path from `commit()` to a Java consumer.**
`commit()` calls `ctx.jfr_emit_java_event(...)`, which appends to CratonVM's own
in-memory repository. The `jfr` crate already has the consumer half of this
(`jfr/src/stream.rs` — a cursor over `EventRepository` with type filters and
callbacks), but nothing connects it to the `Consumer<RecordedEvent>` objects a
Java `RecordingStream.onEvent(...)` registered, and CratonVM never materialises
a `jdk.jfr.consumer.RecordedEvent` for them to receive.

## What a fix needs

Both halves; neither alone makes `JfrEventsTest` pass.

1. Have `RecordingStream.startAsync()` / `start()` perform the recording state
   transition (so `isEnabled`/`shouldCommit`/`commit` go live) while still
   skipping `directoryStream.startAsync` — keep the reason the stub exists.
   `close()` must end it symmetrically.
2. Capture the `(eventName, Consumer)` pairs from
   `RecordingStream.onEvent(String, Consumer)` and dispatch to them from
   `jfr_emit_java_event`, handing each a `RecordedEvent` whose
   `getInt`/`getBoolean`/`getLong`/`getString`/`getEventType` answer the
   committed field values.

**The hazard to plan for in (2)** is holding those `Consumer` objects: a Java
object parked in a Rust side table across arbitrary GC needs a real root
provider, not an address-keyed map. That is the failure mode that has bitten
several overlay side tables in this VM already — do not park a bare
`ObjectRef`.

Scoped as a feature (JFR event streaming), not a contract bug, which is why
this batch-02 pass filed it rather than building it.

## Related

The retired `jfr-dumped-chunk-is-not-readable-by-the-jdk-parser` write-up —
found by the same probe run, independent defect: the `.jfr` file CratonVM *does*
write for a non-streaming `Recording` cannot be parsed by the JDK's own
`RecordingFile`. Fixed in the same pass.

## Repro

```bash
CP=.   # no netty needed
javac -d . JfrProbe.java     # the 60-line probe above
java     -cp . JfrProbe      # isEnabled=true,  received=yes
cratonvm --java-home <jdk25> -cp . JfrProbe   # isEnabled=false, received=NO

# the real consumer
cd apps/netty-suite-runner
printf 'io.netty.buffer.JfrEventsTest\n' > /tmp/one.txt
bash run-netty-suite.sh --list /tmp/one.txt --shards 1 --timeout 900 \
  --bin <cratonvm> --out /tmp/repro
```
