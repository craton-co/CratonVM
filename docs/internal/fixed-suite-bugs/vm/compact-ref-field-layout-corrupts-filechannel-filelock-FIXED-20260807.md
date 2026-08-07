# The compact reference-field layout corrupts `FileChannelImpl.fileLockTable`, so every file lock fails

## Status
**✅ FIXED 2026-08-07.** One line in `vm/src/threading/monitor.rs`:
`try_thin_unlock`'s last release stored the bare `MARK_NEUTRAL` constant into
the mark word, erasing the `kind` / `element_type` / `gc_age` / `gc_flags`
quartet that moved into bits 48..61 when the header shrank. `GC_FLAG_COMPACT`
lives in that quartet, so **the first `synchronized` block on a compact object
converted it to "legacy" on exit**, and every field read after that decoded a
compact-packed body as 16-byte cells.

The title is therefore half right: the compact layout is not corrupt, and the
writers were not wrong. The reader was reading a header the monitor had
rewritten. Regression test:
`monitor::tests::a_thin_lock_round_trip_preserves_the_mark_word_quartet`; the
witness this page shipped, `probes/CompactLayoutFileLockProbe.java`, goes
`PROBE-FAILURES=1` -> `PROBE-OK`.

### The leading hypothesis below was wrong — three measurements killed it

This page proposed *"allocated legacy, written compact"*: an object that took
the legacy fallback in `plan_object_alloc` and was then written by the several
writers that decide compact-ness per CLASS from the global flag. Not so.

| measurement | result |
|---|---|
| `CRATONVM_DBG=compact-legacy` | never names `FileChannelImpl` — it does not take the legacy fallback |
| allocation-site probe | `id=413 num_fields=17 compact_body=Some(96)` — a compact layout IS registered and chosen |
| birth-header probe | `gc_flags=0x4` when `ctx.new_object` returns, `0x0` at the failing read of the SAME object |

The object is **born compact and loses the flag**. That reframes the search from
"which writer used the wrong offset" to "what rewrote the header", and the
window is one `synchronized` block. The per-CLASS writers the page pointed at
(`interpreter.rs`, `jit_bridge.rs`, `jit/helpers.rs`) are JIT-codegen metadata
and are not on this path at all — `--nojit` reproduces byte-identically, which
the page itself records.

`CRATONVM_COMPACT_REF_FIELDS=0` is a complete workaround for the honest reason
that with the layout off nothing is born compact, so losing the flag is a no-op.

**Everything below is the original OPEN write-up, kept for its bisect and its
ruled-out list, both of which stand.**

---

## Status (original)
**OPEN, regression, deterministic (2026-08-07).** Bisected to
`6ba350cdd` — *"Merge perf/header-16-and-field-packing-20260806: HEADER_SIZE 24
-> 16"*. Its first parent `9ddbc9c61` is clean; `6ba350cdd` fails, and so does
every dev tip since, up to and including `51d68e1b7`.

**`CRATONVM_COMPACT_REF_FIELDS=0` is a complete workaround** — and the fact that
it is complete is the finding: the defect is in the compact reference-field
layout, not in the file-lock natives.

**Not the same bug as `5853e9069`** (*"the inline allocator must write the mark
word UNCONDITIONALLY"*), which landed for a Spring Boot `read_slot` corruption
from the same header change. That one is in the JIT's inline allocator; this one
reproduces with **`--nojit`**, byte-identically, and is still present on the tip
that contains it.

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

## The leading hypothesis: allocated legacy, written compact

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

1. Decide the contract: either every writer consults `is_compact_object(header)`
   like `compact_field_slot` does, or `plan_object_alloc` stops silently falling
   back so a registered layout is binding for every instance. The current split
   — per-allocation decision, per-class consumption — cannot be right either way.
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
