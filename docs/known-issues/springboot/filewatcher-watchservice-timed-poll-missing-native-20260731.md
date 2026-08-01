# `WatchService.poll(long, TimeUnit)` has no native override — `AbstractMethodError` kills the watcher thread

**Status: OPEN — found 2026-07-31**

## Symptom

`org.springframework.boot.autoconfigure.ssl.FileWatcherTests`: 9 of 14 failures (the other 5 are
the unrelated symlink-creation bug, see `files-createsymboliclink-unsupported-20260731.md`).

The background watcher thread dies immediately with:

```
16:29:00.685 [ssl-bundle-watcher] ERROR o.s.b.a.ssl.FileWatcher -- Uncaught exception in file watcher thread
java.lang.AbstractMethodError: method java/nio/file/WatchService.poll(JLjava/util/concurrent/TimeUnit;)Ljava/nio/file/WatchKey; has no Code attribute
	at org.springframework.boot.autoconfigure.ssl.FileWatcher$WatcherThread.run(FileWatcher.java:210)
```

repeated once per test (the class re-creates its `FileWatcher`/watcher thread per test method). Every
subsequent attempt to register a path for watching then fails:

```
java.io.UncheckedIOException: Failed to register paths for watching: [...]
	at org.springframework.boot.autoconfigure.ssl.FileWatcher.watch(FileWatcher.java:93)
Caused by: java.io.IOException: WatchService.register: service is closed or unknown
	at org.springframework.boot.autoconfigure.ssl.FileWatcher$WatcherThread.register(FileWatcher.java:199)
```

Affects `testRelativeDirectories`, `shouldWatchFile`, `shouldNotFailIfStoppedMultipleTimes`,
`testRelativeFiles`, `shouldTriggerOnFileModification`, `shouldTriggerOnFileDeletion`,
`shouldNotFailIfDirectoryIsRegisteredMultipleTimes`, `shouldIgnoreNotWatchedFiles`,
`shouldTriggerOnFileCreation`.

`native_ws_close` (invoked once the FileWatcher/JUnit `@AfterEach` cleanup runs) also logs a
GC guard warning for an unrelated-looking receiver:

```
gen_heap::set_field: out-of-bounds field write dropped ... obj=0x17c142739a8 index=2 num_slots=0
class_id=ClassId(1221) class_name=java/nio/file/WatchService real_field_count=Some(0)
```

i.e. `native_ws_close`'s `ctx.set_field(this, WS_FIELD_OPEN, ...)` (index 2) is being called against
a `WatchService` receiver whose class layout has **zero** fields — not the 3-field synthetic object
`native_ws_new` allocates via `alloc_synthetic(ctx, "java/nio/file/WatchService", WS_NUM_FIELDS)`.
This is very likely a second, related symptom of the same underlying dispatch problem below (a
`WatchService`-typed receiver that never went through `native_ws_new`), but was not independently
root-caused beyond that observation.

## Root cause

`native-io/src/lib.rs`'s `register_watch_service` only registers a native override for the
zero-argument `poll()` overload:

```rust
// WatchService.poll() → WatchKey (or null)
r.register(ws, "poll", "()Ljava/nio/file/WatchKey;", native_ws_poll);
```

(`native-io/src/lib.rs:16656`). Real `java.nio.file.WatchService` is an interface with **three**
poll-family methods: `poll()`, `poll(long, TimeUnit)`, and the blocking `take()`. CratonVM registers
`poll()` and `take()` (`native-io/src/lib.rs:16656`, `16659`) but never registers
`poll(long, TimeUnit)` — confirmed by grepping every `"poll"` registration in the tree; the timed
overload's descriptor `(JLjava/util/concurrent/TimeUnit;)Ljava/nio/file/WatchKey;` does not appear
anywhere. `FileWatcher$WatcherThread.run` (Spring Boot's own polling loop) calls exactly this timed
overload, so every invocation falls through to the interface method's own (abstract, no `Code`
attribute) body, throwing `AbstractMethodError` on the first poll — which is uncaught in the watcher
thread and kills it immediately, before any file-change test scenario can run.

Once the watcher thread has died, the `WatchService`/`FileWatcher` fixture is left in a state where
subsequent `Path.register(...)` calls report `WatchService.register: service is closed or unknown`
(`native_ws_register`, `native-io/src/lib.rs:16774` via the `WATCH_SERVICES` map lookup) — this
cascades from the same root cause rather than being independently diagnosed.

## Fix direction

Register `java/nio/file/WatchService.poll(long, TimeUnit)` (`"poll"`,
`"(JLjava/util/concurrent/TimeUnit;)Ljava/nio/file/WatchKey;"`) alongside the existing `poll()`/`take()`
registrations in `register_watch_service`, backed by the same `drain_into_queues`/`WATCH_SERVICES`
machinery as `native_ws_poll`/`native_ws_take` but honoring the timeout (blocking up to the given
duration rather than either returning immediately or blocking forever).

## Affected classes
- `core/spring-boot-autoconfigure` — `org.springframework.boot.autoconfigure.ssl.FileWatcherTests`
  (9/14 failures; the other 5/14 are the separate symlink-creation bug)
