# `FileChannelImpl.fileLockTable` reads back corrupt: the final `monitorexit` erased the object's compact-layout flag

*(Filed under the compact reference-field layout, which is where the evidence
pointed before the flag transition was instrumented. The layout is the victim,
not the cause — see "Root cause" below. Filename kept so existing links resolve.)*

## Status
**FIXED 2026-08-07** on `claude/h2-mvstore-insert-perf-20260807`, by
`fix(vm): the last thin-lock release erased the mark word's quartet`. The
original triage below is kept because its measurements are all correct and
because its leading hypothesis was wrong in an instructive way.

Regression, deterministic, bisected to `6ba350cdd` — *"Merge
perf/header-16-and-field-packing-20260806: HEADER_SIZE 24 -> 16"*. Its first
parent `9ddbc9c61` is clean.

**Not the same bug as `5853e9069`** (*"the inline allocator must write the mark
word UNCONDITIONALLY"*), which landed for a Spring Boot `read_slot` corruption
from the same header change. That one is in the JIT's inline allocator; this
one reproduces with `--nojit`, byte-identically, and is still present on the
tip that contains it.

## Root cause: the final `monitorexit`, not the layout

`try_thin_unlock` (`vm/src/threading/monitor.rs`) returned to NEUTRAL by
storing the **literal** `MARK_NEUTRAL`, which is `0`. The same merge moved
`kind` / `element_type` / `gc_age` / `gc_flags` into mark-word bits 48..63, so
that store erased all four on **every final thin-lock release**. The object
keeps its compact body and loses `GC_FLAG_COMPACT`, so from that instant
`is_compact_object` says no and every reader — correctly honouring the
per-object flag — uses the legacy 16-byte stride over an 8-byte-packed body.

`try_thin_lock`, twenty lines above, had already been taught to mask the
quartet out of its compare for exactly this reason. The release is the other
half of that change and did not get it.

`CRATONVM_DBG_OOBFIELD=sun/nio/ch/FileChannelImpl` against an instrumented
build prints the transition directly — ten accesses compact, then every
subsequent one legacy, with the switch landing between the `putfield` inside
`synchronized (this)` and the `return fileLockTable` after it:

```
[CDIAG alloc] class=sun/nio/ch/FileChannelImpl id=411 num_fields=17 body=Some(96)
[CDIAG get] index=16 num_slots=17 gc_flags=0x4 compact_obj=true  slot=Some((80, Reference))
[CDIAG get] index=10 num_slots=17 gc_flags=0x4 compact_obj=true  slot=Some((56, Reference))
[CDIAG get] index=16 num_slots=17 gc_flags=0x0 compact_obj=false slot=None      <-- after monitorexit
```

`body=Some(96)` is the first line that settles it: the object was allocated
**compact**, not legacy. So the split is not "allocated legacy, written
compact" (the hypothesis below) but "allocated compact, and stopped being
compact halfway through its life".

A second defect of the same family was found beside it and fixed in the same
branch: `MARK_QUARTET_MASK` was `0x3FFF << 48`, two bits short of `gc_age`'s
top, so an object that had survived four young collections failed
`try_thin_lock`'s screen forever and **every `synchronized` on it inflated a
`Monitor`** — and `quartet_of` truncated `gc_age` on every transition.

`probes/CompactLayoutFileLockProbe.java` prints `PROBE-OK` on the fixed build,
with and without `--nojit`, and is kept as the regression test. Unit-level
guards were added at both sites (`thin_unlock_preserves_the_quartet`,
`recursive_thin_unlock_preserves_the_quartet`,
`quartet_mask_covers_every_quartet_subfield`,
`quartet_survives_a_lock_transition_at_every_age`) — the reason neither defect
was caught is that the one existing release test asserts
`mark_state(mark) == MARK_NEUTRAL`, i.e. the low two bits, which a bare
`MARK_NEUTRAL` store satisfies, and the one existing `gc_age` test used age 3,
which fits in the two bits the mask did cover.

## Severity
**HIGH.** `FileChannel.tryLock()` / `lock()` is how every file-backed database
opens. On dev tip no persistent H2 database opens at all — the whole
`org.h2.test.*` file-config suite, and the throughput work in
`h2-update-path-throughput-20260802.md`, are blocked on it. Anything else that
takes a file lock is equally dead.

## Reproducing — twelve lines, no H2

`probes/CompactLayoutFileLockProbe.java`:

```bash
javac -d probe probes/CompactLayoutFileLockProbe.java
<cratonvm> --java-home $JDK25 --nojit --Xmx 1g -c probe CompactLayoutFileLockProbe
```

```
ERROR cratonvm::gc::guard: gen_heap::read_slot: corrupt Value cell (out-of-range
  discriminant) — returning null instead of a UB-on-match Value.
  slot=0x20042441108 raw0="0x0000020042441138" raw1="0x0000000000000000"
Exception in thread "main" java/lang/NullPointerException: Cannot invoke
  "sun.nio.ch.FileLockTable.add(java.nio.channels.FileLock)" because "flt" is null
	at sun/nio/ch/FileChannelImpl.tryLock(FileChannelImpl.java:1734)
```

Same command with `CRATONVM_COMPACT_REF_FIELDS=0` prints `PROBE-OK`. Stock
HotSpot on the same JDK prints `PROBE-OK`. The addresses above are identical on
every run and on every affected build — this is not a race.

## What actually fails

`FileChannelImpl.fileLockTable()` (JDK 25, `FileChannelImpl.java:1651`) is
textbook double-checked locking:

```java
private volatile FileLockTable fileLockTable;

private FileLockTable fileLockTable() throws IOException {
    if (fileLockTable == null) {
        synchronized (this) {
            if (fileLockTable == null) {
                ...
                fileLockTable = new FileLockTable(this, fd);
            }
        }
    }
    return fileLockTable;                 // <-- comes back corrupt, guard nulls it
}
```

The field is assigned and then read back through a cell the heap guard rejects.
`CRATONVM_DBG=cellcorrupt` names the holder and the slot:

```
[CELLCORRUPT] holder=0x200424410b8 (young_from=true old=false) class_id=411
  class=sun/nio/ch/FileChannelImpl kind=0x00 num_slots=17 array_len=0
  gc_flags=0x0 index=4 raw0=0x0000020042441138 raw1=0x0000000000000000
[CELLCORRUPT]   shift-test over 8 cells: valid@aligned=3 valid@+8=4 valid@-8=4
[CELLCORRUPT]   target-header: class_id=0 class=java/lang/Object num_slots=0 gc_flags=0x4
```

## The original leading hypothesis — REFUTED, and worth keeping

*Everything in this section was written before the flag transition was
instrumented. It is wrong at its first step, and the way it is wrong is the
lesson: `gc_flags=0x0` in a `CELLCORRUPT` dump was read as "this object was
allocated legacy", when the allocator had in fact stamped it compact and
something cleared the flag later. A header field is a **time series**, not a
constant; one sample cannot tell "never set" from "set and then cleared", and
the instrument that does is a per-access trace, not a single dump.*

*The negative control in "Ruled out" below could not have reproduced it
either: a user-class object with the same shape never gets a compact layout at
all (`compact_obj=false` from its very first field access), so losing
`GC_FLAG_COMPACT` costs it nothing. A negative control has to be shown to be
capable of failing.*

### (original) The leading hypothesis: allocated legacy, written compact

Stated as a hypothesis because it has not been instrumented to proof — but it
predicts every number above, and it names a concrete design seam rather than a
symbol.

**`gc_flags=0x0` on the holder means this `FileChannelImpl` is on the LEGACY
uniform layout**, not the compact one (`GC_FLAG_COMPACT` is `0x04`,
`types/src/heap_types.rs:632`). `plan_object_alloc`
(`gc/src/gen_heap.rs:15388`) decides that **per allocation**: compact only
"when the flag is on, a layout is registered for `class_id`, **and its field
count matches `num_fields`**" — otherwise it silently falls back to legacy.

The readers honour that per-object decision: `compact_field_slot`
(`gc/src/gen_heap.rs:15421`) opens with `if !is_compact_object(header) { return
None; }`, so `get_field` reads a legacy object as legacy.

**Several writers do not.** They decide per *class*, from the global flag, with
no reference to the object header:

* `vm/src/runtime/interpreter.rs:2562` — the single-pass codegen precomputes
  `compact_field_slot(field.declaring_class_id, field.field_index)` for every
  `getfield`/`putfield` in a method whenever `compact_ref_fields_enabled()`.
* `vm/src/runtime/interpreter/jit_bridge.rs:492`, `:6067` and
  `vm/src/jit/helpers.rs:3736` gate the same way.

So a class that *has* a registered layout, but whose individual object took the
legacy fallback, gets its fields **written at packed 8-byte compact offsets and
read at the 16-byte legacy stride**.

That is exactly the cell we see. A legacy slot is a 16-byte `Value`; slot 4
spans body bytes `[64, 80)`. Under compact packing those same bytes are *two*
8-byte fields. Read as one legacy cell they give `{pointer, 0}` — `raw0` a bare
reference where the discriminant belongs, `raw1` zero. Hence "out-of-range
discriminant". The `+8`/`-8` shift test scoring higher than aligned (4, 4 vs 3)
is the same statement.

`raw0` landing on `holder + 0x80` (this object's own legacy slot 7) is
incidental — it is whatever reference happened to be packed there, not a
self-pointer with meaning.

### Why `FileChannelImpl` in particular

Its fields are hand-written from Rust rather than by running `<init>`:
`native_fcimpl_open` (`native-io/src/file_channel.rs:422`) allocates via
`new_object_ref` and then does ~15 `ctx.set_field_by_name` calls, including
`ctx.set_field_by_name(channel, "fileLockTable", Value::Object(None))`. Those
writes and the legacy reads agree — a field census immediately after `open`
returns `fd` / `path` / `threads` / `parent` correctly on the broken build, and
`fileLockTable` correctly as `null`. It is the *later Java `putfield`* of a real
`FileLockTable` that produces the unreadable cell. That split — native writes
fine, bytecode write corrupt — is what points at the per-class writer gating
rather than at the natives.

Worth checking while fixing: that native also writes `closeLock`, a field JDK 25
`FileChannelImpl` **does not declare** (HotSpot reflection reports it absent).
Whether `set_field_by_name` on an absent field is a no-op or writes somewhere is
a separate question this bug did not need to answer, but it is adjacent.

## Ruled out

* **The JIT.** `--nojit` fails identically, same addresses. The inline-allocator
  mark-word fix (`5853e9069`) does not cover it.
* **The file-lock natives.** `FileKey.create(fd)` — which allocates a `long[2]`
  and has a CratonVM native fill it — returns correct `st_dev`/`st_ino` on the
  broken build, and 400 `long[2]` heap canaries around it are unsmashed.
* **A general double-checked-locking or volatile-reference bug.** A user class
  with the same shape (final refs + `volatile` ref assigned inside
  `synchronized (this)` + primitives) round-trips correctly on the broken build.
* **A general `FileChannelImpl` layout bug.** Every other field of the same
  object reads back correctly, before and after the failure.

## Next steps

1. **Done, differently.** The per-class writer gating this section proposed to
   change is not what broke `FileChannelImpl` — the readers and writers agreed
   throughout; the object's flag moved under both of them. The question it
   raises is still a real one (a per-allocation decision consumed per-class is
   an uncomfortable seam) but it is **not** load-bearing for this bug and
   should not be changed on its evidence.
2. Whatever lands, `probes/CompactLayoutFileLockProbe.java` is the regression
   test; it is deterministic and needs no H2.
3. Until then `CRATONVM_COMPACT_REF_FIELDS=0` restores correctness, at the cost
   of the packing the header change was for.

## Related

* `docs/known-issues/h2/h2-update-path-throughput-20260802.md` — blocked on this;
  no file-backed H2 database opens on dev tip.
* the retired `bug-h2-testtransaction-merge-using-lock-timeout-RESOLVED-20260807`
  write-up — its `MergeLockBudgetProbe` is a second, H2-shaped reproducer
  (`verify` mode fails to open the database on an affected build).
