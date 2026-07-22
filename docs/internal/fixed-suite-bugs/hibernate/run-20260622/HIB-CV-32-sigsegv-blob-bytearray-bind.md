# HIB-CV-32 — SIGSEGV binding a `byte[]` as a BLOB parameter (JDBC insert)

> **✅ FIXED on dev (`c9258e17`, branch `fix/gc-young-sweep-corruptor`, 2026-06-23).**
> Confirmed = the HIB-CV-33 GC corruptor (victim: a boxed `java.lang.Byte` whose
> reclaimed-then-reused storage made `getfield Byte.value` read a malformed `Value`
> `{heap-ptr, 6}`, then a wild jump-table SIGSEGV in `CompactValue::from_value`).
> Two fixes landed:
> 1. **Root** (`gen_heap.rs`): `promotion_oom_risk` no longer diverts `--nojit`
>    young collections into the corrupting non-moving sweep when no conservative JIT
>    roots are present — the precise moving collector runs instead (== `FORCE_MOVING`;
>    opt-out `CRATONVM_PROMOTION_OOM_GUARD_BROAD=1`). Same fix as HIB-CV-22/33.
> 2. **Defense-in-depth** (`types/src/value.rs` + `gen_heap.rs::read_slot`): new
>    `read_value_checked` validates the `Value` discriminant from raw bits before
>    constructing the enum, so any future corrupt cell degrades to a benign null +
>    diagnostic instead of a wild SIGSEGV (the report's recommended guard). Layout
>    pinned by a passing test.
> Verified via `scratch/h22repro/NatPressure` (fix == `FORCE_MOVING` clean / old
> fails), bt16/bt18 == HotSpot, 737 GC tests, `Value` layout/round-trip tests.
> *(The `ByteArrayMappingTests` e2e stays blocked on current dev by separate
> pre-existing bugs — JPMS empty-package + a bootstrap native stack overflow — so it
> cannot reach the BLOB-bind path; validated via the GC mechanism probe instead.)*
>
> **Note for the maintainer:** this report's pre-fix note that HIB-CV-33 is
> "Distinct from HIB-CV-32 (verified)" is superseded — they are the **same** GC
> corruptor (different victims), resolved by one fix.

**Run:** full Hibernate ORM suite, 2026-06-22/23
**Binary:** `cvhibtest.exe` (dev `c863b23e`)
**Severity:** High — hard VM crash (SIGSEGV) in a common data path; **deterministic, reproduces under `--nojit`**, HotSpot PASS
**Status:** **ROOT-CAUSED — re-attributed.** Deep analysis: [`docs/internal/h2-suite-bugs/run-20260622/HIB-CV-32-sigsegv-blob-bytearray-bind.md`](../../h2-suite-bugs2/HIB-CV-32-sigsegv-blob-bytearray-bind.md).

> **Correction to this triage (2026-06-23).** The "audit the `byte[]`→BLOB bind /
> native blob-write path for an OOB read / stale array pointer" hypothesis below is
> **wrong**. The bind succeeds; there is **no native blob-write defect**. The crash
> is in `ValueStack::push`/`CompactValue::from_value` (jump-table on a corrupt
> `Value` discriminant) reached from **`getfield java/lang/Byte.value`** while
> Hibernate *logs* the bound value (`extractLoggableRepresentation` → `Byte.toString()`
> over the boxed `byte[]` elements). The `Byte.value` **heap cell is corrupt**
> (holds a heap pointer where a primitive `byte`/`Value::Int` belongs), during
> active GC forwarding — i.e. **GC corruption of a live boxed `Byte`**, likely the
> **same corruptor family as HIB-CV-22 / HIB-CV-33** (generational non-moving
> young-gen sweep), *contra* HIB-CV-33's "distinct from HIB-CV-32" note (which
> predates this analysis). Caveat to reconcile: this crash is **deterministic**,
> whereas HIB-CV-33 is a load-sensitive heisenbug. Full disassembly / cdb evidence
> in the deep write-up.

---

## Symptom

`org.hibernate.orm.test.mapping.basic.ByteArrayMappingTests` **crashes the VM**
(SIGSEGV, rc=139). HotSpot: PASS (2 tests).

The fatal error fires immediately after Hibernate binds a `byte[]` value as a
BLOB parameter in a batched INSERT:

```
TRACE [org.hibernate.orm.jdbc.batch] Created JDBC batch (15) - [... tableExpression=EntityOfByteArrays, kind=INSERT ...]
TRACE [org.hibernate.orm.jdbc.bind]  binding parameter (3:BLOB) <- [[97, 98, 99]]
# A fatal error has been detected by the CratonVM Runtime Environment:
#  EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005) at pc=0x...
#  Faulting access: read at address 0x...
```

So the crash trigger is **binding a small `byte[]` (`{97,98,99}`) into a BLOB
column** (`EntityOfByteArrays`, which maps `byte[]` / `Byte[]` / `@Lob` columns).

## Why it's a real CratonVM bug

- Deterministic SIGSEGV, reproduces standalone under `--nojit` (so this is **not**
  the JIT family — it crashes in the interpreter/native path).
- HotSpot PASS.

## Root cause area

The crash is on the JDBC BLOB bind path for a `byte[]`. Candidates:
- a native/intrinsic handling `byte[]` → `Blob`/`setBytes`/`setBinaryStream`
  reading out of bounds or dereferencing a bad pointer, or
- the H2 BLOB write path through CratonVM's JDBC/stream handling.

(The fault is a *read* access violation shortly after the bind trace, pointing at
the value-marshalling step for the byte array.)

## Reproduce

```
cvhibtest.exe --java-home <jdk25> --nojit @common.args \
  CratonRunner <list-with-org.hibernate.orm.test.mapping.basic.ByteArrayMappingTests> 0
# -> EXCEPTION_ACCESS_VIOLATION (SIGSEGV), rc=139, right after "binding parameter (3:BLOB) <- [[97, 98, 99]]"
```

The crash dump prints raw native frames and a `CRATONVM_SYMBOLIZE=` hint to
symbolize offline with the same binary.

## Suggested next step for a fixer

Symbolize the dump (`CRATONVM_SYMBOLIZE=<RVAs> cvhibtest …`) — top RVAs
`exe+0x97C344` / `exe+0x9B73A4` / `exe+0x97E101`. Then audit the `byte[]`→BLOB
bind / native blob-write path for an out-of-bounds read or a stale/raw array
pointer.

## Triage

Real, deterministic hard crash on a common mapping (`byte[]`/BLOB), independent of
the JIT. High-value **hand-off** — crash dump + greppable bind trace make it
tractable.
