# `RecordingStream` delivers no events, and `Event.isEnabled()` stays false while one is running

**Status:** OPEN (2026-08-12). The single remaining blocker on
[netty investigate-batch-02](investigate-batch-02.md) — it fails all 10 tests
of `io.netty.buffer.JfrEventsTest`, which pass on stock HotSpot JDK 25.
Found on the Azure Linux host (`20.80.105.49`), binary built from `origin/dev`
`6d1bfd531`.

This is a **known, deliberate gap**, not a regression: the stub is documented
in place in `native-builtins/src/jfr.rs`. This page exists to attach a concrete
failing consumer, an outside-netty repro, and the measured boundary of what
does and does not work.

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

`jfr-dumped-chunk-is-not-readable-by-the-jdk-parser-20260812.md` — found by the
same probe run, independent defect: the `.jfr` file CratonVM *does* write for a
non-streaming `Recording` cannot be parsed by the JDK's own `RecordingFile`.

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
