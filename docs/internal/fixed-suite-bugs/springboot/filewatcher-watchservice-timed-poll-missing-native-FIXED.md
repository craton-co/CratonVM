# `WatchService` delivered no events — five separate defects, of which the missing timed `poll` was only the first

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

14 of the class's 15 tests failed.

## Root causes — the filed diagnosis was correct but was one of five

The filed doc identified the missing timed `poll` registration and predicted that registering it
would fix the class. It did not: fixing each defect only exposed the next, and all five had to go.
Every one of them presents from Java as *nothing happens*, which is why they stacked.

1. **`WatchService.poll(long, TimeUnit)` was never registered.** `WatchService` is an interface, so
   an unregistered overload has no `Code` attribute and the call raises `AbstractMethodError` — it
   does not silently do nothing. `FileWatcher$WatcherThread.run` polls with exactly that overload,
   and its `catch` covers only `InterruptedException`/`ClosedWatchServiceException`, so the error
   went to the uncaught handler and killed the thread. **(as filed)**

2. **`FileSystem.newWatchService()` had THREE registrations.** Real-JDK mode registers native-io's
   natives first and `phases_late/nio_file.rs`'s second, so nio_file's two stubs won over
   native-io's `native_ws_new`. The winner returned a bare **0-field**
   `java/nio/file/WatchService` — no `regs`/`count`/`open` slots and no platform watcher — so every
   later `poll`/`register` read past the object (the GC guard dropped the access) and reported
   "WatchService is closed". This is the `real_field_count=Some(0)` warning the filed doc noted but
   left un-root-caused. Both stubs removed.

3. **`Path.register` decoded the event mask by reading slot 0 of each `WatchEvent.Kind` as an
   `Int`.** `StandardWatchEventKinds.ENTRY_*` are static **fields**, so a real `StdWatchEventKind`
   arrives whose slot 0 is its `name` String. The mask came out **0** and `detect_events`'
   `kind & mask != 0` filter dropped every event the platform watcher delivered. Ask the kind for
   its `name()` instead.

4. **`WatchKey.pollEvents()` built a broken `ArrayList` by writing raw slots.** `java.util.ArrayList`
   is a real class here, so its instances carry the real JDK layout (an inherited `modCount` among
   the slots) — index 0 and 1 are not `elementData`/`size`. The list iterated as EMPTY:
   `detect_events` produced the right events and `poll()` returned a valid `WatchKey`, but the
   caller saw nothing. Build it through `ArrayList.add`. (Also: the pending array's LENGTH is the
   event count, so initialising/resetting it to 64 made a `pollEvents()` before the next `poll()`
   report 64 null events.)

5. **`WatchKey.watchable()` was not registered at all, and then returned the wrong path.** Another
   interface method, so another `AbstractMethodError` in the watcher thread —
   `FileWatcher.accumulate` opens with `Path directory = (Path) key.watchable();`. Registering it
   was not enough: it must return **the `Path` the caller registered**, not the canonicalised one.
   Spring registers a letsencrypt-style path verbatim
   (`live/certname/../../archive/certname`) and matches events with
   `key.watchable().resolve(event.context())` against that same unnormalised string, so a canonical
   answer made every match fail (`shouldFollowRelativePathSymlinks`). The registered `Path` object
   is now kept in a `WatchKey` slot and handed back, as the JDK does.

`WatchEvent.kind()` now also returns the real `StandardWatchEventKinds` constant when it resolves,
so callers comparing kinds by identity work. Several bare Rust-local `ObjectRef`s spanning
allocation points in `native_ws_register`/`native_ws_poll_timed`/`native_wk_poll_events` were
pinned (native stale-local family).

## Diagnosis aid added

`CRATONVM_DBG_WATCH=1` traces the pipeline end to end: registration (path + decoded mask), every
raw platform event and how it is attributed, and each poll's verdict. Defects 2–4 are each a
silent drop at a different stage and are indistinguishable from Java; the trace separates them in
one run. It is what turned defect 3 from a guess into a fact.

## Verification

`docs/known-issues/repros/nio-symlink/WatchProbe.java` — a ~50-line probe that mirrors
`FileWatcher.accumulate` step for step: register a directory, touch a file, poll with a timeout,
look the key up in a map keyed by `WatchKey` identity, call `watchable()`, resolve each event's
context against it, and compare with the registered path. Its output is now **identical** to a real
JDK run:

```
keyLookup = reg
watchable = /tmp/watchprobeNNN
events = [ENTRY_CREATE:a.txt, ENTRY_MODIFY:a.txt]
resolved = [/tmp/watchprobeNNN/a.txt, /tmp/watchprobeNNN/a.txt]
resolvedMatches = true
RESULT=OK
```

`FileWatcherTests` on Linux (`victor@20.83.144.174`, JDK 21) through `SbRunner`: **15/15 under
CratonVM, matching HotSpot's 15/15.** Before: 14 failures.

## Why it landed on the symlink branch

The five symlink-dependent cases (`shouldFollowSymlink`, `shouldFollowSymlinkRecursively`,
`shouldFollowRelativePathSymlinks`, `shouldTriggerOnConfigMapUpdates`,
`shouldTriggerOnConfigMapAtomicMoveUpdates`) could not run past the dead watcher thread, so the
symlink work in `files-createsymboliclink-unsupported-FIXED.md` was not end-to-end verifiable
without this.
