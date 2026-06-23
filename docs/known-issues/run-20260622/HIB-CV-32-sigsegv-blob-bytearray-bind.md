# HIB-CV-32 — SIGSEGV binding a `byte[]` as a BLOB parameter (JDBC insert)

**Run:** full Hibernate ORM suite, 2026-06-22/23
**Binary:** `cvhibtest.exe` (dev `c863b23e`)
**Severity:** High — hard VM crash (SIGSEGV) in a common data path; **deterministic, reproduces under `--nojit`**, HotSpot PASS
**Status:** **ROOT-CAUSED — re-attributed.** Deep analysis: [`docs/internal/h2-suite-bugs/run-20260622/HIB-CV-32-sigsegv-blob-bytearray-bind.md`](../../../internal/h2-suite-bugs/run-20260622/HIB-CV-32-sigsegv-blob-bytearray-bind.md).

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
