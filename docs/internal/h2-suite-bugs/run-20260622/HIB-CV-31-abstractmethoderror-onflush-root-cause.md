# HIB-CV-31 — `AbstractMethodError: FlushEventListener.onFlush ... has no Code attribute` — ROOT CAUSE

**Run:** Hibernate ORM suite, retest 2026-06-23
**Repro binary:** release `cratonvm.exe` built from `dev` (`f8cdd52b` + working tree)
**Test:** `org.hibernate.orm.test.lob.JpaLargeBlobTest#jpaBlobStream`
**Severity:** High — wrong result reported; HotSpot PASS; deterministic under `--nojit`
**Status:** Root-caused. **NOT an interface/itable dispatch defect.** Two distinct
defects, one of which (the real fault) is shared with **HIB-CV-32**.

---

## TL;DR (correction of the original triage)

The original HIB-CV-31 write-up
(`docs/known-issues/run-20260622/HIB-CV-31-abstractmethoderror-interface-dispatch-no-code.md`)
hypothesised a **general interface/itable dispatch bug** — "interface invoke binds
to the abstract method". **That hypothesis is wrong.** Interface dispatch works
correctly here. The reported `AbstractMethodError` is a **mis-attributed cascade**:

1. **Root fault (shared with HIB-CV-32):** while binding the BLOB parameter, a live
   object reference (the BLOB's `InputStream`) is read back as **`java/lang/Object`
   / `ClassId(0)`** — i.e. it is corrupted. Calling `in.read()` on it raises
   `NoSuchMethodError: java/lang/Object.read()I`.
2. **Masking defect:** that `NoSuchMethodError` bubbles **out of the already-running
   `onFlush` body** back to the lambda-dispatch call site, which mis-reads it as
   "the lambda's SAM did not resolve on the receiver" and **retries the call on the
   abstract `FlushEventListener` interface** → no `Code` attribute →
   `AbstractMethodError`, which is what the suite reports.

So the AME is noise over a heap/value-corruption bug. GC is **not** involved
(collection count was 0 in every instrumented run).

---

## Symptom (reproduced)

```
@@FAIL org.hibernate.orm.test.lob.JpaLargeBlobTest :: java.lang.AbstractMethodError:
  method org/hibernate/event/spi/FlushEventListener.onFlush(Lorg/hibernate/event/spi/FlushEvent;)V has no Code attribute
```

Java stack at the AME:

```
at org.hibernate.event.service.internal.EventListenerGroupImpl.fireEventOnEachListener(EventListenerGroupImpl.java:133)
at org.hibernate.internal.SessionImpl.fireFlush(SessionImpl.java:1449)
at org.hibernate.internal.SessionImpl.beforeTransactionCompletion(SessionImpl.java:2014)
... TransactionImpl.commit(TransactionImpl.java:89)
at org.hibernate.orm.test.lob.JpaLargeBlobTest.lambda$jpaBlobStream$0(JpaLargeBlobTest.java:61)   // tx.commit()
```

## How `fireEventOnEachListener` actually dispatches

`EventListenerGroupImpl.fireEventOnEachListener(U event, BiConsumer<T,U> action)`
(bytecode @133) calls `action.accept(listener, event)`, where `action` is the
method reference `FlushEventListener::onFlush` (an **unbound** instance-method-ref
`BiConsumer`, 0 captures). CratonVM routes `accept` through
`try_lambda_dispatch` (`vm/src/runtime/interpreter.rs`).

## Evidence — dispatch is correct, the receiver is corrupt

Instrumented run (`CRATONVM_DBG_NOCODE=1`, `--nojit`) showed, in order:

1. The `onFlush` lambda dispatch **resolves the receiver correctly**:
   ```
   [DBG_LAMBDARECV] impl=org/hibernate/event/spi/FlushEventListener.onFlush(...)V
       num_captures=0 call_args=2 recv_cid=5148
       recv_class=Some("org/hibernate/event/internal/DefaultFlushEventListener")
   ```
   → `accept(listener, event)` correctly targets `DefaultFlushEventListener.onFlush`.
   **There is no itable/dispatch bug.** `onFlush` then runs.

2. Inside the running `onFlush`, Hibernate emits the JDBC INSERT and binds the BLOB:
   ```
   TRACE [org.hibernate.orm.jdbc.bind]  binding parameter (1:BLOB) <- [{blob}]
   WARN  NoSuchMethodError method="java/lang/Object.read()I"
         caller="org/h2/util/IOUtils.readFully(Ljava/io/InputStream;[BI)I @pc=23"
   ```
   H2's `IOUtils.readFully` calls `in.read()`, but the receiver `in` (the BLOB's
   `InputStream`) resolves to **`java/lang/Object`** → `read()I` is not found.

3. Immediately after, the reported AME, with a **corrupt receiver**:
   ```
   [DBG_NOCODE] ...FlushEventListener.onFlush(...)V has no Code attribute
       | recv_cid=0 recv_class=java/lang/Object
   ```
   `ClassId(0)` is `java/lang/Object`; `get_class(0)` returns it (not the
   `<cid 0>` "unknown" fallback). So a reference that should point at a concrete
   object is reading back as a bare `Object`.

4. `[DBG_GC]` collection count = **0** across all instrumented runs → this is **not**
   a GC stale-root / moved-object bug.

## The masking defect (why the AME, not the NSME, is reported)

`try_lambda_dispatch`'s `InvokeVirtual`/`InvokeInterface` arm
(`vm/src/runtime/interpreter.rs`, the `let result = match &result { … }` fallback
after the first `invoke_or_native`) does:

```rust
let result = invoke_or_native(shared, thread, &receiver_class /* DefaultFlushEventListener */,
                              member /* onFlush */, desc, &full_args);
let result = match &result {
    Err(NoSuchMethodError { .. }) if receiver_class != impl_handle.class_name => {
        // retry on the lambda's DECLARED class — here the abstract interface
        invoke_or_native(shared, thread, &impl_handle.class_name /* FlushEventListener */,
                         member, desc, &full_args)
    }
    _ => result,
};
```

Intent: "if the receiver's class doesn't *have* the SAM, fall back to the lambda's
declared class." Bug: the guard fires for **any** `NoSuchMethodError`, including the
`java/lang/Object.read()I` raised **deep inside the successfully-dispatched
`onFlush` body**. Because `receiver_class` (`DefaultFlushEventListener`) ≠
`impl_handle.class_name` (`FlushEventListener`), it **retries `onFlush` on the
abstract interface `FlushEventListener`**, which has no `Code` → `AbstractMethodError`.

This both **hides the real error** (the `Object.read()I` NSME / corruption) and
**fabricates a misleading dispatch-looking symptom**. The `recv_cid=0` on the AME
also shows the flush-listener reference is itself among the corrupted references at
bind time (consistent with widespread reference corruption — see HIB-CV-32).

## Root fault — BLOB-bind reference/value corruption (== HIB-CV-32)

The actual defect is that **binding a BLOB parameter corrupts live references**:
the BLOB `InputStream` (and the flush-listener reference) read back as
`java/lang/Object` / `ClassId(0)`. This is the **same path and family as
[HIB-CV-32](../../../known-issues/run-20260622/HIB-CV-32-sigsegv-blob-bytearray-bind.md)**
(SIGSEGV binding a `byte[]` as a BLOB — `binding parameter (3:BLOB) <- [[97,98,99]]`).
HIB-CV-31 binds a stream (`LobInputStream`, an inner class created with a valid
`ClassId`), HIB-CV-32 binds a `byte[]`; both corrupt on the bind step and both are
size-independent (HIB-CV-32's array is 3 bytes, HIB-CV-31's declared length is
200 MiB but `read()` fails on the *first* call, before any bulk transfer).

A pre-existing, now-gated diagnostic confirms value-slot corruption on this build:
`[HIB32] CORRUPT getfield value …` (interpreter `getfield`, gated behind
`CRATONVM_DIAG_HIB32`). Note the raw-discriminant heuristic over-reports on
NaN-boxed primitive slots (e.g. it flags `Integer.value=1`, `boolean` fields), so
treat individual `[HIB32]` lines as leads, not proof.

## Two defects, two fixes

**A. Masking defect — narrow the lambda retry-on-interface guard.**
Only retry on the declared class when *the SAM itself* failed to resolve on the
receiver — i.e. the `NoSuchMethodError` is for **`(receiver_class, member_name)`** —
not for some unrelated method raised inside the body:

```rust
Err(NoSuchMethodError { class_name: nsme_cls, method_name: nsme_m, .. })
    if receiver_class != impl_handle.class_name
       && *nsme_cls == receiver_class
       && *nsme_m   == impl_handle.member_name => { /* retry on impl class */ }
```

This is correctness-positive on its own (it stops `try_lambda_dispatch` from
double-invoking and from masking in-body linkage errors), independent of the BLOB
fix. The original report's second sighting — `HierarchicalTestExecutorService$TestTask.execute()V
has no Code attribute` — is almost certainly the same masking pattern (an in-body
`NoSuchMethodError` re-dispatched onto an abstract SAM).

**B. Root fault — fix the BLOB-bind reference/value corruption (HIB-CV-32 family).**
This is the high-value fix and is **already under active investigation** (the
enriched `[HIB32]` `getfield` diagnostic with receiver-header dump lives on `dev`).
Audit the `Blob`/`setBinaryStream`/`setBytes` bind path for whatever overwrites or
mis-decodes live reference slots so that they read back as `ClassId(0)`.

## Reproduce

```sh
D=C:/craton/CratonVM/apps/hibernate-orm/.cratonvm-suite
printf 'org.hibernate.orm.test.lob.JpaLargeBlobTest\n' > "$D/hib31_list.txt"
CRATONVM_DBG_NOCODE=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 \
  cratonvm.exe --nojit --java-home <jdk25> --Xmx 1500m \
  @"$D/common.args" -Dcraton.batch=1 CratonRunner "$D/hib31_list.txt" 0
# -> binding parameter (1:BLOB); NoSuchMethodError java/lang/Object.read()I;
#    then AbstractMethodError FlushEventListener.onFlush ... has no Code attribute (recv_cid=0)
```

Diagnostics used (debug `eprintln`s) were temporary and have been reverted; the
`CRATONVM_DBG_NOCODE` `[DBG_NOCODE]` line and the `CRATONVM_DIAG_HIB32` `[HIB32]`
getfield diagnostic remain on `dev`.

## Triage

Real, deterministic, `--nojit`, HotSpot PASS. **Re-classify** from
"interface/itable dispatch defect" to: (A) a lambda-dispatch error-misattribution
bug (`try_lambda_dispatch` retry-on-interface), masking (B) BLOB-bind
reference/value corruption (== HIB-CV-32). Fix A is small and self-contained; B is
the substantive root cause and is being worked under HIB-CV-32.
