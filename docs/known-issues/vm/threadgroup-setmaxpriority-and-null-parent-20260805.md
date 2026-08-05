# `ThreadGroup.setMaxPriority` does not clamp against the parent, and `new ThreadGroup(null, name)` does not throw

**Status:** OPEN, filed 2026-08-05. Both found by
`probes/ThreadGroupLayoutProbe.java` while verifying the transposed-slot fix;
**neither is caused by it** — both reproduce identically on the pre-fix binary.
Recorded here rather than fixed alongside, because each is a semantics change on
a shared path and wants its own A/B.

## 1. `setMaxPriority` ignores the parent's ceiling

`java.lang.ThreadGroup.setMaxPriority(int)` in the JDK clamps twice: into
`[MIN_PRIORITY, MAX_PRIORITY]`, **and then down to the parent group's own
`maxPriority`**. CratonVM's native only does the first.

Measured, JDK 25 image, both modes, identical on the pre- and post-fix binary:

```java
ThreadGroup parent = new ThreadGroup("pri-parent");
parent.setMaxPriority(4);
parent.setMaxPriority(Thread.MAX_PRIORITY + 5);   // 15
parent.setMaxPriority(Thread.MIN_PRIORITY - 5);   // -4
```

| | HotSpot 25 | CratonVM |
|---|---:|---:|
| after `setMaxPriority(15)` | **4** | 10 |
| after `setMaxPriority(-4)` | **4** | 1 |

HotSpot holds at 4 because `parent` is a child of `main`, whose own ceiling the
group may not exceed — and once lowered to 4 it may not be raised again through
this API either. CratonVM re-raises it to the global `MAX_PRIORITY`.

**Why it matters beyond the number:** `Thread.setPriority` clamps against the
owning group's `maxPriority`, so a group that will not stay lowered cannot cap
its threads. Anything that lowers a pool's group to de-prioritise its workers
(several JDK and container thread factories do) silently does not.

The fix is in `ThreadGroup.setMaxPriority` in
`native-builtins/src/phases_late/concurrent.rs`: read the parent's
`maxPriority` through `tg_get_field(.., "maxPriority", TG_SLOT_MAX_PRIORITY)`
and take the minimum, matching the JDK's own two-step clamp.

## 2. `new ThreadGroup(null, name)` returns normally

The JDK's `ThreadGroup(ThreadGroup parent, String name)` dereferences the
parent (`parent.checkAccess()` historically, `parent.maxPriority` today), so a
null parent is an `NullPointerException`. CratonVM's `<init>` native stores the
null and returns.

| | HotSpot 25 | CratonVM |
|---|---|---|
| `new ThreadGroup(null, "x")` | **NPE** | returns normally |
| `new ThreadGroup(null)` (name only) | returns normally | returns normally |

The second row is the control: a null *name* legitimately does not throw on
either, so this is specifically the parent argument, not general null-tolerance.

A group with a null parent is also a second root, which
`InnocuousThread.createThreadGroup()`'s walk to the root and
`ThreadGroup.enumerate(recurse)` both take at face value — the same class of
hierarchy confusion the `system <- main` parenting fix in
`vm/src/vm/vm_exec.rs` was written to avoid.

## 3. Adjacent, not a `ThreadGroup` bug: `setAccessible` ignores module access

Noted here only so a reader diffing the probe against HotSpot is not surprised
by it. `Field.setAccessible(true)` on a private `java.lang.ThreadGroup` field
succeeds under CratonVM and throws `InaccessibleObjectException` under HotSpot
25 without `--add-opens java.base/java.lang=ALL-UNNAMED`. That is a module-access
enforcement gap, unrelated to layout, and it affects every `java.base` class.

## How to reproduce

```sh
javac -d /tmp/tgp probes/ThreadGroupLayoutProbe.java
cd /tmp/tgp && java -cp . ThreadGroupLayoutProbe > hotspot.txt
cratonvm --real-jdk --java-home "$JAVA_HOME" -cp . ThreadGroupLayoutProbe > craton.txt
diff hotspot.txt craton.txt
```

The `pri clampHigh` / `pri clampLow` and `err nullParent` lines are these two.
Everything else in that probe matches HotSpot on the post-fix binary except the
`ref *` lines (item 3) and `root *` (a CratonVM-only native the JDK has no
member for).
