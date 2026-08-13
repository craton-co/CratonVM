# "Stale pointer detected in invokevirtual receiver" fired on a healthy `new Object()` — FIXED

**Status:** FIXED (2026-08-12). Found while clearing
`docs/known-issues/hibernate-reactive/investigate-batch-04.md` — not from a
failing test, but from a warning that appeared **exactly once per test class**,
in every run, on every collector, while all 12 classes passed.

## Symptom

```
WARN cratonvm_vm::runtime::interpreter::invoke: Stale pointer detected in
invokevirtual receiver (ptr=0x200100014c0, all-zero header) — falling back to
CP class java/lang/Object
```

Six lines of Java reproduce it, with no Hibernate, no GC pressure and no
threads:

```java
public class StaleN {
    public static void main(String[] a) {
        Object o = new Object();
        System.out.println(o.hashCode());   // ← warns here
    }
}
```

Fires on `<default>`, `-XX:+UseG1GC`, `-XX:+UseZGC` and
`-XX:+UseGenerationalGC` alike.

## Root cause: a bare `Object` header IS all-zero, legitimately

The detector reads the first 16 bytes of the receiver and treats
`[0u8; 16]` as proof of a stale pointer into zeroed GC memory. For a
freshly-allocated, no-field `java.lang.Object` every one of those bytes is
zero **by construction**:

| header field | value for `new Object()` | why |
|---|---|---|
| `class_id` | `0` | `ClassId(0)` *is* `java.lang.Object` |
| `shape` (num_slots) | `0` | no instance fields |
| `mark_word` | `0` | `MARK_NEUTRAL == 0`, `ObjectKind::Object == 0`, `ArrayElementType::Reference == 0` |

`ObjectHeader::new`'s own doc comment says the mark word starts with "no
identity hash installed", so nothing else is there to break the tie.

**This had already been fixed once, and the fix was silently erased.** The
comment on `init_object_header` still describes it:

> *"H1: `identity_hash_code` is now eagerly assigned at allocation time (caller
> passes `shared.mem.heap.next_identity_hash()`). … every fresh TLAB-allocated
> `new Object()` (cid=0, fields=0) produced an all-zero first 16 bytes of header
> that the stale-pointer detector in `execute_invoke` mis-flagged as stale
> memory, causing CGLIB's HashMap operations to emit spurious 'Stale pointer
> detected' warnings on every legitimate `Object` key."*

That is a description of the current bug, written in the past tense. The
2026-08-06/07 header shrink (32 → 24 → 16 bytes) folded `identity_hash_code`
into the mark word and left `ObjectHeader::new` with **no hash parameter at
all** — so `init_object_header` cannot assign one, takes none, and the comment
now documents a fix the code is no longer capable of performing. The sibling
compact-layout path two lines above (`shape.init_header(ptr, class_id, hash)`)
still passes a hash, which is why only the legacy-layout shape regressed.

## Why it mattered, given nothing failed

The cost is not the log line. This warning is the **tripwire for the
reclaimed-live-receiver family** — `CRATONVM_DBG_BUG03`,
`CRATONVM_DBG_SWEEP_ZERO` and `CRATONVM_DBG_STALE_RECV` all hang off this exact
branch, and the sweep-zero consumer beneath it exists to name the root-coverage
gap when a live object really is reclaimed. A tripwire that fires on
`new Object()` — a lock, a sentinel, a `HashMap` key — is one nobody reads.

Dispatch itself was never wrong: the fallback picks the CP-resolved class, and
for a real bare `Object` that class *is* `java/lang/Object`.

## Fix

Demote to `debug!` when the CP-resolved class is `java/lang/Object`, i.e. an
`Object`-declared call site (`hashCode`/`equals`/`toString`/…) where the
fallback the detector takes is the correct dispatch for a genuine bare `Object`
anyway. Kept as a `warn!` for every other receiver class, where an all-zero
header really is suspicious.

This is the call already made two lines below for `java/lang/ClassLoader`
("the CP fallback succeeds and a WARN was mostly noise"), applied to the one
other shape that provably cannot be told apart from zeroed memory.

**The trade, stated rather than buried:** a genuinely stale receiver at an
`Object`-declared call site now logs at `debug` instead of `warn`. That is worth
it against a 100% false-positive rate at that site. The alternative — restoring
eager hash minting so a live `Object` is never all-zero — is the better fix and
needs `ObjectHeader::new` to take a hash again, which is a header-layout change
and not one to make from a docs sweep.

## Verification

| check | before | after |
|---|---|---|
| `StaleN` 6-line repro | 1 warning | **0** |
| hibernate-reactive batch-04, 12 classes on G1 | 12/12 PASS, 12 warnings | **12/12 PASS, 0 warnings** |
| total WARN lines in that run | 316 | **304** (exactly the 12 removed) |
| `cargo test -p cratonvm-vm --lib` | — | PASS |

The instrument was itself controlled: `@@RESULT` (12) and `WARN` (304) were
counted in the same pass, so "0 stale warnings" is a real zero and not a grep
that matched no files. An earlier version of this census *did* silently match
nothing because the raw logs live at `<run>/on-real/shard-N/raw.log` and the
glob assumed `<run>/*/raw.log`.
