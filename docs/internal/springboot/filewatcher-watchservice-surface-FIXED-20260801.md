# `WatchService` surface: missing timed `poll`, and a placeholder that displaced the real implementation

**Status: FIXED 2026-08-01**

Retires `docs/known-issues/springboot/filewatcher-watchservice-timed-poll-missing-native-20260731.md`.

## Defect

`org.springframework.boot.autoconfigure.ssl.FileWatcherTests` failed 14 of 15. Nine of those
failures were the WatchService surface (the other five are the separate
`Files.createSymbolicLink` gap, still open — see below). Three independent faults stacked:

**1. `WatchService.poll(long, TimeUnit)` had no native.** `java.nio.file.WatchService` is an
interface, so the call fell through to its abstract declaration:

```
java.lang.AbstractMethodError: method java/nio/file/WatchService.poll(JLjava/util/concurrent/TimeUnit;)Ljava/nio/file/WatchKey; has no Code attribute
	at org.springframework.boot.autoconfigure.ssl.FileWatcher$WatcherThread.run(FileWatcher.java:210)
```

Only `poll()` and `take()` were registered. Those are the two overloads a watch loop cannot use:
`poll()` returns immediately and `take()` blocks forever, so the loop's quiet-period pattern
(`watchService.poll(quietPeriod, MILLISECONDS)`) is the only shape that works — and it was the one
missing.

**2. Two competing WatchService implementations; the placeholder won.**
`register_phase57_nio_file` registered `java/nio/file/FileSystem.newWatchService()` returning a
bare object with **zero** fields, and `cratonvm-native-io`'s `register_watch_service` registered
the real one — a `notify::RecommendedWatcher` behind a 3-field carrier. Registration is
last-write-wins and the real-JDK arm of `vm_init` calls `register_io_natives` *before*
`register_phase57_nio_file`, so the placeholder silently displaced the real implementation. Every
later native then operated on an object that had none of the slots it expected:

* `Path.register` looked the service up in `WATCH_SERVICES` and missed →
  `java.io.IOException: WatchService.register: service is closed or unknown`, which
  `FileWatcher.watch` rethrows as
  `UncheckedIOException: Failed to register paths for watching: [...]` (8 of the 9 failures);
* `WatchService.close` wrote slot 2 on a zero-slot receiver → the
  `gen_heap::set_field: out-of-bounds field write dropped ... real_field_count=Some(0)` guard
  warning the original report noted but did not root-cause. That warning was this, exactly.

A third, dormant copy of the same surface lived in `register_p66_watch_service` (a 2-field layout
answering `poll` by re-`stat`ing the directory and comparing mtimes). It lost on ordering in every
arm, but was one reordering away from taking over with a third incompatible layout.

**3. The surface could not have worked against real JDK objects even once reached.** Found while
fixing the above, each independently fatal:

* `Path.register` decoded its `WatchEvent.Kind[]` by reading slot 0 as an `Int`. On a real
  `StandardWatchEventKinds$StdWatchEventKind` slot 0 is the `name` String, so the mask came out 0
  and `detect_events` filtered out **every** event — a watch that registered successfully and
  reported nothing, forever.
* `WatchKey.watchable()` was not registered at all, though `Path dir = (Path) key.watchable()` is
  the first line of the canonical accumulate loop.
* `WatchEvent.context()` returned a synthetic 2-field `java/nio/file/Path`. The caller's very next
  move is `directory.resolve((Path) event.context())` on a real `sun.nio.fs.UnixPath`.
* `WatchKey.pollEvents()` returned a synthetic 2-slot `java/util/ArrayList`; the caller iterates it
  with a for-each, which runs real `ArrayList$Itr` bytecode against `elementData`/`size`/`modCount`.
* `WatchKey.reset()` installed a fresh **64-element** array as the pending set, so the next
  `pollEvents()` reported 64 null events.
* Registering the same directory twice minted a second key. `detect_events` drains one shared
  per-path queue, so whichever key was scanned first consumed everything and the other never fired.
* `poll`/`take` threw `IOException` on a closed service. The JDK throws
  `ClosedWatchServiceException`, and a watch loop catches exactly that to shut itself down.

## Fix

`native-io/src/lib.rs`:

* Register `WatchService.poll(long, TimeUnit)` (`native_ws_poll_timed`): honours the timeout via
  `TimeUnit.toMillis`, sleeps in short slices inside a *timed blocking region* so a
  stop-the-world GC is never held off for the whole wait, and returns `null` at the deadline.
  Also registers the 3-arg `Path.register(WatchService, Kind[], Modifier[])` primitive,
  `WatchKey.watchable()`, and `WatchEvent.count()`.
* `poll`/`take` now throw `java.nio.file.ClosedWatchServiceException`.
* `watch_event_kind_bit` decodes real `StandardWatchEventKinds` singletons (via `name()`), the
  synthetic Int-tagged kinds, and the legacy bare-name Strings. A kinds array that decodes to
  nothing is logged and treated as CREATE|DELETE|MODIFY rather than silently registering a watch
  that can never fire.
* `watch_event_kind_object` hands back the **real** `StandardWatchEventKinds` singleton, so
  `event.kind() == ENTRY_CREATE` (an identity comparison) holds.
* `WatchKey` gained a `watchable` slot holding the caller's own `Path` object;
  `WatchEvent.context()` is derived from it as `dir.resolve(name).getFileName()`, so it is the same
  `Path` implementation the caller already holds.
* `pollEvents()` builds a real `java.util.ArrayList` and *consumes* the pending set;
  `reset()` clears it instead of installing a 64-null array; `close()` cancels every key it created
  and skips the slot write entirely on a receiver that has no slots.
* Re-registering an already-registered directory replaces that key's event set and returns the
  same key (the JDK's documented behaviour).
* Every reference held across an allocation / `invoke_virtual` is pinned and re-read
  (`pin_native_root` / `read_native_pin`), including the previously-unpinned event materialization
  loop.

`native-builtins/src/phases_late/nio_file.rs`: both placeholder `FileSystem.newWatchService`
registrations removed, and `register_p66_watch_service`'s competing implementation removed down to
the `StandardWatchEventKinds` constants. `cratonvm-native-io` is now the single owner of the
surface.

`native-builtins/tests/registry_contracts.rs`:
`watch_service_surface_has_a_single_owner_in_native_io` replays the real-JDK arm's registration
order and asserts each triple has exactly one owner, in `native-io`. A plain presence check would
not have caught this — it passes just as happily when the *loser* is the one left standing.

## Validation

Azure host, `fix/watchservice-timed-poll-20260801`, real-JDK mode (JDK 25).

* `org.springframework.boot.autoconfigure.ssl.FileWatcherTests`: **14 failed → 5 failed**, stable
  across 4 consecutive runs. All 9 tests named in the original report pass. Zero
  `AbstractMethodError`, zero out-of-bounds field writes, zero WARN/ERROR in the run log.
* The 5 remaining failures are `UnsupportedOperationException` from
  `Files.createSymbolicLink` at test *setup*, i.e. the separate open bug
  `docs/known-issues/springboot/files-createsymboliclink-unsupported-20260731.md`. They never
  reach the watch loop.
* Rest of `org.springframework.boot.autoconfigure.ssl`: `BundleContentNotWatchableFailureAnalyzerTests`
  2/2, `BundleContentPropertyTests` 9/9, `PropertiesSslBundleTests` 6/6, `SslAutoConfigurationTests`
  4/4, `SslPropertiesBundleRegistrarTests` 7/7. `CertificateMatcherTests` fails 4 containers on a
  Mockito error — verified identical on a baseline binary built from the same commit without this
  change.
* `org.springframework.boot.loader.nio.file.NestedFileSystemTests` 22/22 and `NestedPathTests`
  31/31 — these assert that a third-party `FileSystem`/`Path` implementation's own
  `newWatchService`/`register` overrides still throw `UnsupportedOperationException`, i.e. the
  natives registered on the `java/nio/file/*` base types do not hijack them.
* `cargo test -p cratonvm-native-io` 371/371; `cargo test -p cratonvm-native-builtins` 3202 passed,
  2 failed (`panama::tests::test_85_4_upcall_handle_and_invoke`,
  `tls_deny::tests::every_plaintext_base_overload_is_accounted_for`) — both verified failing on the
  unmodified tree at the same commit.
* `cargo test -p cratonvm-vm --lib --features synthetic-jdk -- watch_service` 2/2.
* Direct probe (`WsProbe.java`): `newWatchService` → `register` → timed `poll` → `pollEvents` →
  `kind`/`context`/`watchable` → `reset` → `close` round-trips, with
  `ctx class=sun.nio.fs.UnixPath` and `kind=ENTRY_CREATE`.
