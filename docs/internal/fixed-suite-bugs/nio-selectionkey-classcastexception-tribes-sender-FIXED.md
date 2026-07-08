````# Tribes `ParallelNioSender`: `ClassCastException: Object cannot be cast to SelectionKey` under GC stress

Status: FIXED / retired 2026-07-08

Date observed: 2026-07-07 (Azure Linux, dev @ `8b35a995` — i.e. **with** the
`nio_selector.rs` `build_set`/`populate_selected_keys_field` cross-call
GC-safety fix already landed; real-JDK jdk25, `--nojit`,
`CRATONVM_GC_STRESS=4194304`)

Date fixed: 2026-07-08 (`codex/fix-nio-selectionkey-tribes-20260708-001`)

## Resolution

The selector side table was not the remaining culprit. The selected-key set
construction and mutation path crossed into `native-collections`:

- `Selector.selectedKeys()` builds a `java.util.HashSet` and populates it through
  `HashSet.add`, which directly enters `HashMap.put`.
- `HashMap.put` computed `key.hashCode()` before pinning the inner map receiver,
  key, or value. Under `CRATONVM_GC_STRESS`, even `Object.hashCode()` can enter a
  safepoint with a pending moving GC, leaving later bucket/node writes using
  stale locals.
- `HashSet.iterator()` allocated the snapshot array before pinning the `HashSet`
  receiver used as the iterator's `remove()` backing. A GC during snapshot
  allocation could install a stale backing reference in the iterator.
- `Iterator.remove()` on the selected-key set routes through `HashMap.remove`,
  which had the same unpinned receiver/key window before `hashCode()`.

The fix pins the `HashMap.put` receiver/key/value across the whole hash/insert
window, pins the `HashMap.remove` receiver/key across the hash/remove window, and
pins the `HashSet.iterator()` receiver/backing/snapshot array before any
allocation that can move them. The focused fixture now uses two ready
`SelectionKey`s per selector cycle and asserts that `Iterator.remove()` drains the
selected-key set.

Validation:

```powershell
$env:CARGO_TARGET_DIR='C:\craton\cargo-targets\nio-selectionkey-tribes-20260708-001'
javac -d 'C:\craton\cargo-targets\nio-selectionkey-tribes-20260708-001-javac' vm\tests\resources\cratonvm\NioSelectorBuildSetGc.java
$env:CRATONVM_BIN='C:\craton\cargo-targets\nio-selectionkey-tribes-20260708-001\debug\cratonvm-nio-selectionkey-tribes-20260708-001-debug.exe'
$env:CRATONVM_TEST_CLASSES_DIR='C:\craton\cargo-targets\nio-selectionkey-tribes-20260708-001-javac'
cargo test -p cratonvm-vm --test nio_selector_build_set_gc -- --nocapture
cargo test -p cratonvm-native-collections --test mock_hashmap -- --nocapture
```

Both passed on Windows in the fix worktree.

## Correction to the original report

A prior investigation (background task spun off from the
`nio-native-side-table-stale-objectref` audit) hypothesized that the crash
below was caused by an unpinned Rust-local `ObjectRef` in
`native-builtins/src/serialization.rs`'s `ObjectInputStream`/
`ObjectOutputStream` native implementations (`ois_read_object`,
`ois_read_descriptor_fields`, the `writeObject` native), held across
GC-capable `invoke_virtual`/`invoke_special` dispatches — the same bug shape
as the (fixed) `nio_selector.rs::build_set`.

**That hypothesis does not hold.** `native-builtins::serialization` is
compiled only under the `experimental-serialization` or `synthetic-jdk`
Cargo features (`native-builtins/src/lib.rs:1162`:
`#[cfg(any(feature = "experimental-serialization", feature =
"synthetic-jdk"))] pub mod serialization;`), and neither is part of the
default feature set (`native-builtins/Cargo.toml`: `default = []`) or
enabled by `vm-cli`'s default build. The Tribes repro below uses a plain
`cargo build --release -p cratonvm-cli` binary — this module is not even
compiled into it. In real-JDK mode, `java.io.ObjectInputStream`/
`ObjectOutputStream` run almost entirely as **real JDK bytecode**; CratonVM
only overrides two narrow natives (`ObjectInputStream$1.checkArray` in
`shared_secrets_bridge.rs`, `ObjectInputStream.resolveProxyClass` in
`lib.rs`), neither of which matches the reported crash shape. (Three real
instances of the *same bug shape* were nonetheless found and fixed in
`serialization.rs` while auditing it — see
`fix/serialization-unpinned-local-gc-20260707` — but that fix is for the
dormant experimental-serialization path and does **not** address the crash
documented here.)

## What actually reproduces

```bash
cd /data/data/apps/tomcat
CRATONVM_REAL_NET_SOCKETS=1 CRATONVM_REAL_AQS=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 \
  CRATONVM_GC_STRESS=4194304 <cratonvm> --java-home <jdk25> --nojit -Xmx2g \
  -cp "$(cat .suite/cp.txt)" org.junit.runner.JUnitCore \
  org.apache.catalina.tribes.test.channel.TestDataIntegrity
```

One run: **5/5 test failures**. Some are plausibly ordinary GC-pause-induced
timing flakiness in Tribes' own UDP-multicast membership protocol (frequent
`McastService`/`SocketTimeoutException` retries are expected background
noise at this GC-stress level — do not over-index on those). Two signals
are **not** timing noise:

1. One occurrence of a genuine memory-safety symptom, early in the run,
   in the `Tribes-MembershipReceiver` thread:
   ```
   Stale pointer detected in invokevirtual receiver (ptr=0x20086bcb318, all-zero header)
     — falling back to CP class java/lang/Throwable
   ```
2. A reproducible `ClassCastException` inside Tribes' own
   `ParallelNioSender.doLoop` (its private per-sender `Selector`, not
   Tomcat's connector selector):
   ```
   Caused by: java.lang.ClassCastException: java.lang.Object cannot be cast to java.nio.channels.SelectionKey
       at org.apache.catalina.tribes.transport.nio.ParallelNioSender.doLoop(ParallelNioSender.java:174)
   ```
   Confirmed via `javap -c` on the compiled class that line 174 is exactly
   the standard idiom:
   ```java
   Iterator it = selector.selectedKeys().iterator();
   while (it.hasNext()) {
       SelectionKey sk = (SelectionKey) it.next();   // <-- javac-inserted checkcast fails here
       ...
       it.remove();
   }
   ```
   (bytecode: `Selector.selectedKeys()` → `Set.iterator()` → `hasNext()` →
   `next()` → `checkcast SelectionKey` — the checkcast is what throws.)

## Why the already-landed `build_set` fix doesn't (fully) explain this

`Selector.selectedKeys()` (`nio_selector.rs::selector_selected_keys`) collects
a `Vec<ObjectRef>` snapshot of live `key_obj`s under `selectors().read()` +
per-selector lock (no GC-capable call in that critical section, so the
snapshot itself should be consistent), then hands it to `build_set`, which
already pins the constructed `Set` and each key element across every
`HashSet::<init>`/`Set.add` dispatch (fixed 2026-07-07, verified via a
400-cycle GC-stress regression test with zero failures). Once inserted, the
`SelectionKey` objects live inside a **real** `java.util.HashSet`'s internal
`HashMap$Node[]` array — an ordinary Java object graph the interpreter's
own root-scanning should keep consistent through further GC cycles,
independent of `build_set`. That the checkcast *still* fails even with this
fix in place, on a fresh `dev`-based build, means either:

- `build_set`'s protection has a gap for this specific call pattern
  (`selectedKeys()` + `Iterator.remove()`, which mutates the HashSet's
  backing structure — worth checking whether `Iterator.remove()`'s real
  bytecode path has its own GC-safety issue distinct from construction), or
- a **different**, not-yet-identified native in the same
  `nio_selector.rs`/`sk_table` family (e.g. `sk_cancel_public`,
  `channel_register_native`, or the `attachment()` accessor) races with or
  invalidates a `key_obj` already sitting inside an in-flight
  `selectedKeys()` `Set` that a caller (here, Tribes' sender) is still
  iterating, or
- a genuine Tribes-side concurrency assumption that HotSpot's GC pause
  characteristics don't expose but CratonVM's more aggressive stress-GC
  timing does (i.e., not a CratonVM correctness bug at all, but a real
  behavioral difference under this GC-stress regime) — cannot be ruled out
  without further isolation.

## Closure notes

The original next-step list is now covered by the updated fixture:

1. The minimal repro is a two-`SelectionKey`, single-selector Java fixture
   (`NioSelectorBuildSetGc`) driven under `CRATONVM_GC_STRESS`.
2. `Iterator.remove()` is exercised against the selected-key `HashSet` and
   checked with `ready.isEmpty()` after the loop.
3. `sk_cancel_public` and `channel_register_native` were not the failing layer
   for this specific crash; the corruption window was in native collection
   map/set helpers used by the selected-key set.
