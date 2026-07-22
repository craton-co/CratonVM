# HIB-CV-31 — `AbstractMethodError: ... has no Code attribute` (interface method dispatch resolves to the abstract method)

> **✅ FIXED on dev (`c9258e17`, branch `fix/gc-young-sweep-corruptor`, 2026-06-23).**
> Re-classified (NOT an itable/dispatch defect): a mis-attributed cascade over the
> HIB-CV-32 BLOB-bind corruption. Two fixes landed:
> 1. **Masking defect** (`vm/src/runtime/interpreter.rs`): `try_lambda_dispatch`'s
>    retry-on-interface guard fired for *any* `NoSuchMethodError`, including one
>    raised deep inside a running `onFlush` body (`Object.read()I` on a corrupt
>    `InputStream`), re-dispatching the SAM onto the abstract interface → bogus
>    `AbstractMethodError`. Now the retry fires only when the NSME names the SAM's
>    own `(receiver_class, member_name)`; genuine in-body errors propagate.
> 2. **Root corruption** (== HIB-CV-32): the GC young-sweep corruptor — fixed in
>    `gen_heap.rs` (see HIB-CV-22/32/33).
> Verified: lambda dispatch smoke test (method-ref `BiConsumer`, `forEach`) passes
> under JIT and `--nojit`; GC fix via `NatPressure` + bt16/bt18. *(Full Hibernate
> e2e blocked on current dev by separate pre-existing JPMS empty-package + bootstrap
> stack-overflow bugs.)*

**Run:** full Hibernate ORM suite, 2026-06-22/23
**Binary:** `cvhibtest.exe` (dev `c863b23e`)
**Severity:** High — wrong virtual/interface dispatch (correctness); **deterministic, `--nojit`**, HotSpot PASS
**Status:** Confirmed. **Root-caused 2026-06-23 — the "interface/itable dispatch
defect" hypothesis below is WRONG.** See
[../../internal/h2-suite-bugs/run-20260622/HIB-CV-31-abstractmethoderror-onflush-root-cause.md](../../internal/h2-suite-bugs/run-20260622/HIB-CV-31-abstractmethoderror-onflush-root-cause.md).

> **CORRECTION (2026-06-23).** Interface dispatch is **correct** here:
> `accept(listener, event)` resolves `onFlush` to the concrete
> `DefaultFlushEventListener` (verified, `recv_cid=5148`). The reported AME is a
> **mis-attributed cascade** over two real but different defects:
> 1. **Root (== [HIB-CV-32](HIB-CV-32-sigsegv-blob-bytearray-bind.md)):** binding the
>    BLOB parameter corrupts a live reference — the BLOB `InputStream` reads back as
>    `java/lang/Object`/`ClassId(0)`, so `in.read()` raises
>    `NoSuchMethodError: java/lang/Object.read()I` (H2 `IOUtils.readFully`).
> 2. **Masking:** that NSME, raised *inside* the already-running `onFlush`, trips
>    `try_lambda_dispatch`'s retry-on-interface fallback
>    (`vm/src/runtime/interpreter.rs`), which re-dispatches `onFlush` on the
>    **abstract** `FlushEventListener` → `AbstractMethodError ... has no Code`.
>
> GC is not involved (collection count 0). The `TestTask.execute()V` sighting is
> likely the same masking pattern. Fixes: (A) narrow the lambda retry guard to fire
> only when the SAM itself fails to resolve on the receiver; (B) fix the BLOB-bind
> corruption (tracked under HIB-CV-32). Full analysis + evidence in the internal doc.

---

## Symptom

`org.hibernate.orm.test.lob.JpaLargeBlobTest`:

```
java.lang.AbstractMethodError: method org/hibernate/event/spi/FlushEventListener.onFlush(Lorg/hibernate/event/spi/FlushEvent;)V has no Code attribute
```

The same `... has no Code attribute` `AbstractMethodError` was also observed on an
unrelated interface during the run:

```
... HierarchicalTestExecutorService$TestTask.execute()V has no Code attribute
```

HotSpot: PASS.

## What it means

`onFlush` is an **interface** method (`FlushEventListener`) with concrete
implementations. `"has no Code attribute"` is a **CratonVM-internal** diagnostic:
CratonVM dispatched the call to the **abstract interface method itself** (which has
no bytecode body) instead of resolving to the concrete implementing method →
`AbstractMethodError`.

This is an **interface/virtual dispatch (itable) resolution bug**: under some
condition the receiver's concrete override is not selected. That it appears on two
completely unrelated interfaces (`FlushEventListener`, JUnit's `TestTask`) shows it
is a **general dispatch defect**, not a single mapping quirk — and it can corrupt
arbitrary programs.

## Why it's a real CratonVM bug

- Deterministic, reproduces standalone under `--nojit` (not the JIT family).
- HotSpot PASS.

## Reproduce

```
cvhibtest.exe --java-home <jdk25> --nojit @common.args -Dcraton.trace=1 \
  CratonRunner <list-with-org.hibernate.orm.test.lob.JpaLargeBlobTest> 0
# -> AbstractMethodError: ...FlushEventListener.onFlush... has no Code attribute
```

## Suggested next step for a fixer

Grep CratonVM for `"has no Code attribute"` — that is the throw site. Then examine
interface-method resolution / itable construction for the case that selects the
declaring interface's abstract method instead of the implementation. Likely
triggered by a specific class-hierarchy shape (default methods, re-abstracted
methods, multiple interfaces, or a proxy/enhanced subclass — `JpaLargeBlobTest`
involves Hibernate event listeners which are often composed/wrapped).

## Triage

Real, deterministic, independent of the JIT, and **general** (affects any
interface dispatch hitting the bad path). High-value **hand-off** for whoever owns
method resolution / vtable-itable.
