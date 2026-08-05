# `ThreadGroup.setMaxPriority` does not clamp against the parent, and `new ThreadGroup(null, name)` does not throw

**Status:** FIXED 2026-08-05. What was filed as two divergences measured as
**ten** once the probe asked paired questions, and the fix this page originally
prescribed was **wrong** — see "Corrections" below. Originally found by
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

~~The fix is in `ThreadGroup.setMaxPriority` … take the minimum, matching the
JDK's own two-step clamp.~~ **That prescription was wrong.** See below.

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

---

## Corrections, and what was actually done — 2026-08-05

`probes/ThreadGroupPriorityProbe.java` asks 27 paired questions instead of the
three the original probe asked. Ten of them diverged from Temurin 25.0.3, and
two of the divergences falsify this page's own analysis.

### The prescribed fix would not have worked

"Clamp into `[MIN, MAX]`, then take the minimum with the parent" gives **10**
and **1** for the two rows in the table above, not 4. Out-of-range is a
**no-op** in JDK 25 — `setMaxPriority` returns before touching anything:

| after | HotSpot | clamp-then-min | old CratonVM |
|---|---:|---:|---:|
| `setMaxPriority(15)` on a group at 4 | **4** | 10 | 10 |
| `setMaxPriority(-4)` on a group at 4 | **4** | 1 | 1 |

The page's reading that a lowered group "may not be raised again through this
API either" is also wrong: an in-range `setMaxPriority(7)` afterwards gives
**7**. The constraint is the parent's ceiling, not a ratchet.

### Three more divergences it did not mention

* **No propagation at all.** Lowering a parent to 2 left an existing subgroup
  at 10; the JDK gives 2, recursively, to every descendant.
* **Propagation ASSIGNS, it does not only lower.** A subgroup at 1 whose parent
  is set to 5 comes **up** to 5 — the JDK's recursion is
  `for (g : groups) g.setMaxPriority(maxPriority)`. The natural
  `if (child > new) lower(child)` guard is wrong, and no in-range test would
  catch it.
* **`toString` hard-coded `maxpri=10`.** A second reader of the same state,
  found only because the probe reads it through a second API.

### And one that is not a `ThreadGroup` bug

`Thread`'s initial priority did not inherit the group ceiling:
`populate_real_thread_holder` (`native-builtins/src/lib.rs`) passed a literal
`NORM_PRIORITY` to the `Thread$FieldHolder` constructor. The JDK takes the
CREATING thread's priority and caps it at `g.getMaxPriority()`. `setPriority`
already clamped correctly; only construction skipped the step — so a group
lowered to 3 still handed out threads at 5, which defeats the point of lowering
it. Fixed at that call site.

### Result

Ten divergences to **one**, then to zero. The remaining `ref *` lines are the
module-access gap in §3, which is unrelated and still open.

**Method note.** This page's prescription was derived by reading the JDK's
behaviour rather than measuring it, and it was wrong in both halves. A fix
written from it would have produced 10 and 1, changed the transcript, and
looked like progress while reproducing the original defect.
