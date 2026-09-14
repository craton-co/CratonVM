# F41-1 — what the first real measurement of wave F found

**Status:** FIXED-MEASURED (the four below were measured on CratonVM before and
after, on attributable binaries). **Provenance:** MEAS on both VMs.
HotSpot 25.0.3+9-LTS is the oracle throughout. Probes:
`scratchpad/orch/{CbProbe,CbRoute,PropNull,MapNull,HtNull,PropAxis}.java`.

Wave F landed 16 lanes, none of which could build or run the VM — every
"after" in F2-1 … F40-1 is **PREDICTED**. This record is the first pass that
ran them. It exists because three of the four defects below were *created or
left behind by that wave*, and none was visible to the lane that owned the
file.

---

## 0. The headline

| vector | before | after |
|---|---|---|
| `RJdkIntrinsics2` | aborted at check 32 of the `hex` family | **PASS, 1022 checks**, differential-clean |
| `RJdkReflBox` | (new) | **PASS, 107 checks** |
| `RSslNullSession` | — | **PASS** |
| `Properties` null axis | **0 of 15** rows matched | 11 of 15, then 15 of 15 |

`hex` reads **77** and `bounds` reads **121** — exactly F14-1's and F21-1's
predicted denominators. Those two predictions were right.

---

## 1. `CharBuffer.toString()` — right length, wrong origin, on ONE route

`HexFormat.parseHex(CharSequence)` threw
`NumberFormatException: not a hexadecimal digit: "x" = 120`.

Measured on `CharBuffer.wrap("x00ff0a80y", 1, 9)` (position 1, limit 9):

| route | HotSpot | CratonVM |
|---|---|---|
| bytecode `cb.toString()` | `00ff0a80` | `00ff0a80` |
| `Method.invoke` / `ctx.invoke_virtual` | `00ff0a80` | **`x00ff0a8`** |

Eight characters starting at index **0**: the *length* was applied and the
*position* dropped. `java/nio/CharBuffer.toString()` called
`cb_to_string_range(this, 0, lim - pos)` — a relative range — while the
`java/nio/StringCharBuffer` twin beside it did the absolute thing correctly.

**Why only one route was wrong.** `StringCharBuffer` does **not declare**
`toString()`; the JDK inherits it from `CharBuffer`. So
`Class.getMethod("toString")` resolves to the **declaring** class and every
reflective / native-invoke caller lands on the generic registration, while a
bytecode `invokevirtual` dispatches on the receiver and lands on the correct
one. Two implementations, one exercised per route, and the route nothing
tested was the broken one.

`parseHex(char[],int,int)` is specified as
`parseHex(CharBuffer.wrap(chars, fromIndex, toIndex - fromIndex))`, so the JDK's
own bytecode arrives at that native holding a `CharBuffer` — which is how a
`toString` defect surfaced as a hex-parsing exception.

**Fixed** by dispatching on the receiver's real class inside the generic
registration, so one implementation serves both routes. The twin stays as the
direct-dispatch entry. Patching the second copy would have left a third to
drift.

## 2. `array()` on the typed views — a phantom family and a silent null

`ByteBuffer.allocate(16).asIntBuffer().array()` returned **null**, so
`.length` raised NullPointerException where HotSpot throws
`UnsupportedOperationException`.

`--dump-native-registry` answered what reading could not:

```
java/nio/IntBuffer     ()[I  by=servlet.rs:6581  owns_slot=true  overwrote=null
java/nio/LongBuffer    ()[I  by=servlet.rs:6581
java/nio/ShortBuffer   ()[I  by=servlet.rs:6581
java/nio/FloatBuffer   ()[I  by=servlet.rs:6581
java/nio/DoubleBuffer  ()[I  by=servlet.rs:6581
```

Two defects in one row:

* the loop hardcodes `()[I` for five families with five different element
  types, so **four are phantom registrations** nothing can ever dispatch to;
* for `IntBuffer` — the one whose descriptor happens to be right —
  `owns_slot=true, overwrote=null` means it is the **sole** registrant. So
  `native-io`'s `native_tb_array` and every refusal F14-1 added to it **never
  ran**. F14-1 fixed the twin that loses.

The body was `Ok(Some(Value::Object(s2_bb_arr(...))))` — a `None` became a
silent Java `null`. **Fixed**: per-class descriptor, and the shared
`buffer_array_access` decision with `Absent → UnsupportedOperationException`
(no fourth arm: every typed view is array-less in the JDK, as F37-1 measured).

## 3. The `Properties` null axis — 0 of 15, and the rule is TWO rules

Every null-key entry point silently succeeded. The 15-row sweep also corrects
the obvious generalisation, which this lane made and had to unmake:

* **lookup path** — `get`, `getProperty`, `getProperty(k,d)`, `containsKey`,
  `getOrDefault`, 1-arg `remove` — reaches `Hashtable.get`, which dereferences
  the key for its hash:
  `NullPointerException: Cannot invoke "Object.hashCode()" because "key" is null`
* **write path** — `put`, `putIfAbsent`, `replace`, `remove(k,v)`,
  `containsValue`, `contains`, `forEach` — is overridden in `Properties.java`
  with `Objects.requireNonNull`, whose NPE carries **no message**
* `putAll(null)` is a third:
  `Cannot invoke "java.util.Map.size()" because "m" is null`

Applying one helper everywhere would have replaced fourteen wrong *answers*
with fourteen wrong *messages* — and the fixtures assert message text, so it
would have looked like progress while staying red.

**Two messages were FABRICATED**, i.e. the right exception kind with invented
text, which is invisible to any assertion that checks only the type:

* `putIfAbsent(null, v)` and `computeIfAbsent(null)` emitted the `hashCode`
  text where HotSpot's message is empty;
* `replace(k, null)` emitted `"Hashtable.replace: null value"` — a string that
  appears **nowhere in the JDK**.

**The family had to stay receiver-routed.** `HashMap.getOrDefault(null, d)` is
legal and `Hashtable`'s is not (measured), so the shared Map native must not
throw; the check went on the `Properties` slot and, for
`replace`/`remove(k,v)`, into the existing `is_hashtable_receiver` helper —
which was already correctly receiver-aware and merely had the wrong message
and no key check at all.

## 4. The differential caught a HARNESS bug, and blamed the VM for it

After every assertion in `RJdkIntrinsics2` passed, the run still failed with
`output differs from HotSpot`. The entire diff was one em-dash: HotSpot's
`System.out` mangled it through the Windows console encoding, CratonVM emitted
correct UTF-8. **A non-ASCII character in a printed check label makes the
oracle comparison encoding-dependent**, and CratonVM was failing for being
more correct. Three such labels existed across two fixtures; all are now
ASCII and the residual count is zero.

---

## 5. What this says about the wave

Three of the four defects were **reachable only by a route no fixture had ever
exercised**, and became visible the moment F14-1 and F21-1 added rows for
paths nobody had asserted. Two were in code a wave-F lane had just fixed — in
the copy that loses. The registry dump, not source reading, is what settled
both ownership questions; two competing hypotheses about which body ran were
resolved in one command.

**Still open, measured but not fixed:** `java/util/Hashtable` itself diverges
on **7 of 9** rows of the same axis (`get`, `getOrDefault`, `containsKey`,
`containsValue`, `put` both ways, `remove`). It is a separate slot from
`Properties`, served partly by generic Map natives shared with `HashMap`, so
it needs the same receiver-routing treatment rather than a copied guard.
`ConcurrentHashMap`'s helper also carries an invented message
(`"ConcurrentHashMap does not permit null keys"`) that has not been checked
against the oracle.

## 6. `InheritableThreadLocal` snapshots at START, not at CONSTRUCTION

MEASURED 2026-08-13 (`/tmp/T.java`), and this one is a **semantics**
divergence rather than a missing refusal:

```java
ITL.set("parent-init");
Thread t = new Thread(() -> seen[0] = ITL.get());
ITL.set("set-after-construction");
t.start();
```

| | HotSpot | CratonVM |
|---|---|---|
| child sees | `parent-init` | `set-after-construction` |

The JDK copies the parent's map in `Thread.<init>`
(`this.inheritableThreadLocals = ThreadLocal.createInheritedMap(parent...)`),
so a `set` between construction and `start()` is **not** visible to the child.
A second thread constructed *after* the second `set` sees the new value on
both VMs, which is why a single-thread probe reads clean.

**The cause is already written down at the site** —
`lang_system.rs::native_thread_start0` applies the snapshot at *start0-time*
deliberately, as a documented workaround: the real `Thread.<init>` copy
"silently doesn't take effect … specifically when BOTH the ThreadGroup and
name constructor arguments are explicitly non-null", which is exactly the
shape `Executors.defaultThreadFactory()` uses for every pooled worker. The
workaround chose losing the values in executors over losing the capture
timing; the timing is what this fixture measures.

**Not attempted here.** Moving the capture to construction touches every
executor path, and the site's own note says root-causing the constructor
misbehaviour needs interpreter-level bytecode tracing. It is recorded with
its repro rather than half-fixed at the tail of a session that cannot
validate the executor paths it would move.
