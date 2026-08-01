# `WatchService.poll(long, TimeUnit)` had no native override — `AbstractMethodError` killed the watcher thread

**Status: FIXED — 2026-08-01** (branch `fix/nio-symlink-create-20260801`)

Supersedes
`docs/known-issues/springboot/filewatcher-watchservice-timed-poll-missing-native-20260731.md`.

## Symptom (as filed 2026-07-31)

`org.springframework.boot.autoconfigure.ssl.FileWatcherTests`: the background watcher thread died
immediately, once per test method:

```
[ssl-bundle-watcher] ERROR o.s.b.a.ssl.FileWatcher -- Uncaught exception in file watcher thread
java.lang.AbstractMethodError: method java/nio/file/WatchService.poll(JLjava/util/concurrent/TimeUnit;)Ljava/nio/file/WatchKey; has no Code attribute
	at org.springframework.boot.autoconfigure.ssl.FileWatcher$WatcherThread.run(FileWatcher.java:210)
```

and every later registration then failed with
`UncheckedIOException: Failed to register paths for watching: [...]` /
`IOException: WatchService.register: service is closed or unknown`.

## Root cause (confirmed — the filed diagnosis was correct)

`register_watch_service` (`native-io/src/lib.rs`) registered `poll()` and `take()` but never the
timed `poll(long, TimeUnit)` overload. `WatchService` is an **interface**, so an unregistered
overload has no `Code` attribute at all: calling it raises `AbstractMethodError` rather than doing
nothing observable. Spring Boot's `FileWatcher$WatcherThread.run` polls with exactly that overload.

## Repair

Registered `java/nio/file/WatchService.poll(JLjava/util/concurrent/TimeUnit;)Ljava/nio/file/WatchKey;`
→ `native_ws_poll_timed`: the same drain-and-check loop as `take()`, bounded by the caller's
deadline and returning `null` rather than throwing when it expires. The timeout is obtained by
asking the `TimeUnit` itself (`toNanos`) rather than decoding an ordinal, so any constant works.
A closed service still throws, which is what ends Spring Boot's watcher loop on `stop()`.

The receiver is pinned across the wait and re-read each iteration: the loop calls `native_ws_poll`,
which allocates (`WatchEvent`/`Path`/array), so a bare Rust-local `ObjectRef` would be stale after
a moving young GC — the same native stale-local hazard `take()` still carries.

## Why it landed on the symlink branch

The five symlink-dependent `FileWatcherTests` cases (`shouldFollowSymlink`,
`shouldFollowSymlinkRecursively`, `shouldFollowRelativePathSymlinks`,
`shouldTriggerOnConfigMapUpdates`, `shouldTriggerOnConfigMapAtomicMoveUpdates`) could not run past
the dead watcher thread, so the symlink work in
`files-createsymboliclink-unsupported-FIXED.md` was not end-to-end verifiable without this.

## Verification

`FileWatcherTests` on Linux (`victor@20.83.144.174`, JDK 21) through `SbRunner`:
15/15 under CratonVM, matching HotSpot's 15/15. Before: 14 failures.

## Not addressed

The filed doc also noted a `gen_heap::set_field: out-of-bounds field write dropped ...
class_name=java/nio/file/WatchService real_field_count=Some(0)` warning from `native_ws_close`,
i.e. a `WatchService`-typed receiver that never went through `native_ws_new`. It did not reproduce
in the verification runs above and was never independently root-caused; if it resurfaces it needs
its own investigation.
