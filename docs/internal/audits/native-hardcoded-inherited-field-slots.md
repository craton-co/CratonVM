# Handoff: native impls that hardcode an inherited field's slot

**Status:** Open audit. Two instances fixed (DataOutputStream, StringWriter); the
rest of the IO/stream surface still needs a sweep.
**Owner wanted:** native-io / native-builtins maintainer.

---

## The bug class (one-paragraph version)

A Rust native that models a real-JDK class often reaches an instance field by a
**hardcoded slot index** (`ctx.get_field(obj, 1)`). That index is a *physical*
address — a function of the **entire** superclass chain's instance fields. If the
native author counted only the class's own fields (or an older/shorter superclass
layout), the constant points at the wrong field. The natives stay **self-consistent**
(native writer + native reader use the same wrong slot, so the class's own methods
look correct), so it passes every native→native test. It only breaks at the
**native ↔ real-bytecode seam**: when real bytecode (a *subclass* `getfield`, or the
class's own un-overridden bytecode) resolves the field *by name* to the correct slot
and finds it stale, or finds a foreign value the native parked there.

Two consequences, both real:
1. **Stale/wrong reads at the seam.** The canonical victim: jboss-classfilewriter's
   `ByteArrayDataOutputStream.writeSize()` reads `this.written` (inherited from
   `DataOutputStream`) to record a back-patch position; the native maintained
   `written` at the wrong slot, so the subclass read 0, back-patched offset 0, and
   corrupted the generated class-file magic → Weld `WELD-001524` on every client
   proxy (HIB-CV-25b). **A real, shipped-library failure.**
2. **A reference parked in a primitive-typed slot.** When the native writes a
   `byte[]`/`String` into a slot whose *declared* type is `boolean`/`int` (because the
   real field there is e.g. `closed`/`initialSize`), a moving GC that scans slots by
   declared type may not treat it as a root → **use-after-free / dangling buffer under
   memory pressure.** Unproven to bite yet, but it's the dangerous tail.

**Reflection is a false comfort.** `getDeclaredFields()` / field layout / annotation
reflection all read the *class-file metadata*, which CratonVM parses faithfully — so
they match HotSpot exactly even when a native is poking the wrong slot. Verifying
"reflection matches HotSpot" tells you the *map* is right; it says nothing about
whether a given native follows the map. Do **not** use it to clear this bug class.

## How to audit (repeatable, no build needed)

Two probes against the current binary:

1. **Layout dump** — print the flat slot order of each suspect class (base-class
   fields first), then compare to the native's `*_FIELD_*` constants:
   ```java
   int slot=0;
   for (Class<?> k : hierarchyBaseToDerived(c))
     for (Field f : k.getDeclaredFields())
       if (!isStatic(f)) System.out.println("slot "+(slot++)+" = "+k.getSimpleName()+"."+f.getName());
   ```
2. **Subclass-seam test** — subclass the JDK class and read an inherited field via
   `getfield`; compare to HotSpot. A native that squats the slot shows a wrong type:
   ```java
   class Spy extends StringWriter { Object peekLock(){ return this.lock; } }
   // HotSpot: lock == the StringBuffer;  CratonVM(buggy): lock == [I
   ```
   A pure functional round-trip (write→read) is **blind** to this — it only exercises
   native→native.

## Findings (this audit, JDK 25, `--nojit`)

| Class | Native const | Real slot | Status |
|---|---|---|---|
| `DataOutputStream.written` | slot 1 | **3** (after `out,closed,closeLock`) | ✅ **FIXED** → by-name (`HIB-CV-25b`) |
| `BufferedOutputStream.buf/count` | 1/2 | 3/4 | ✅ already mitigated — `bos_slots()` resolves by name |
| `BufferedInputStream.*` | 1/2/3 | 2/4/3 | ✅ already mitigated — synthetic overrides dropped, real bytecode runs |
| `DataInputStream.in` | 0 | 0 | ✅ correct (only field used) |
| `StringReader` | content=0 | (lock=0…) | ✅ functional + seam OK (lock reads `this`, as JDK) |
| `LineNumberReader` | in/lineNum/pos/content=0..3 | lock=0, skipBuffer=1, in=2,… | ⚠️ functional OK; **seam not deep-tested** — constants clearly don't match real layout, audit further |
| `CharArrayWriter` | buf/count=0/1 | lock=0,… | ⚠️ functional OK; seam not deep-tested |
| `StringWriter` | buf/count=0/1 | lock=0, buf=1 | ✅ **FIXED** → synthetic natives + base `Writer.write(I)V` gated behind `synthetic-jdk`; real StringBuffer-based bytecode runs by default. Subclass `this.lock` now reads the `StringBuffer` (== `buf`), == HotSpot. |

`StringWriter` was fixed by *dropping* the synthetic natives (gating behind
`synthetic-jdk`) rather than remapping slots — its synthetic `char[]`+`count` model
can't be reconciled with the real `StringBuffer`-based representation, and the real
bytecode is self-contained. This also retired the base-class `Writer.write(I)V` →
`native_sw_write_int` registration, which had applied the StringWriter layout to
*every* Writer subclass (same hazard the `Reader.read` migration fixed). Verified:
StringWriter/PrintWriter/CharArrayWriter/OutputStreamWriter all == HotSpot;
regression suite 8/8 (+ a `LockSpy extends StringWriter` seam test in `RSerial`);
native-io 318 tests.

## Fix recipe

Mirror the `DataOutputStream`/`BufferedOutputStream` fixes — resolve the slot from the
same source of truth the bytecode uses:

- Hot path / per-element: resolve once with
  `ctx.resolve_field_index("java/io/StringWriter", "buf").unwrap_or(LEGACY)` and reuse
  (see `bos_slots`).
- Cold path: `ctx.get_field_by_name(this, "buf")` / `set_field_by_name(this, "buf", …)`
  (see the `DataOutputStream` fix).
- Keep a `LEGACY` fallback so synthetically-allocated instances (no real metadata)
  still work.

`out`/`in` at slot 0 are safe to leave hardcoded (the first inherited field of
`Filter*Stream` lands at slot 0 of any subclass).

## Remaining sweep (candidate surface)

Every `*_FIELD_*` constant in `native-io/src/lib.rs`, `native-builtins/src/*.rs`, and
`native-collections/src/lib.rs` that (a) is used with raw `get_field`/`set_field` on a
class **loaded as real JDK bytecode** (not `alloc_synthetic`) **and** (b) whose class
has any superclass with instance fields. Priority order (StringWriter now done):
`LineNumberReader`, `CharArrayWriter`, `CharArrayReader` (Reader/Writer family, slot-0
collides with `lock` — pass functional + shallow seam checks today, but their
constants don't match the real layout, so deep-audit the seam) → then a grep-driven
pass over the rest. Synthetic-only classes
(allocated by CratonVM with a controlled layout, never subclassed by real bytecode) are
out of scope — their constants *are* their layout.

## Tests to add per fix

A subclass-seam regression like `RSerial`'s `WrittenSpy extends DataOutputStream`
(reads the inherited field via `getfield`, asserts it matches the native counter /
the expected JDK value). Functional round-trips are necessary but not sufficient.
