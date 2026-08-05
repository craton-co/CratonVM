# HIB-CV-25 (follow-up) — Weld client-proxy `WELD-001524` — `DataOutputStream.written` wrong field slot ✅ FIXED

**Status:** FIXED on dev (native-io; pure correctness fix, no flag)
**Severity:** High — broke ALL Weld client-proxy generation (`@ApplicationScoped`/normal-scoped beans); deterministic, HotSpot PASS
**Area:** `native-io` — `java.io.DataOutputStream` native field layout

---

## Symptom

After the [containsAll fix](HIB-CV-25-cdi-weld-qualifier-containsall-foreign-collection.md)
cleared `WELD-001301`, two `cdi.general.hibernatesearch.*` tests still failed —
now with a **different** error (and correct qualifiers `[@Any @Default]`):

```
org.jboss.weld.exceptions.WeldException: WELD-001524: Unable to load proxy class
for bean Managed Bean [class ...TheSharedApplicationScopedBean] with qualifiers [@Any @Default]
  Caused by: java.lang.IllegalArgumentException: Lookup.defineClass: not a valid class file (bad magic)
    at org.jboss.weld.bean.proxy.util.WeldDefaultProxyServices.defineWithMethodLookup(...:153)
```

Weld generates a client-proxy subclass and defines it via
`privateLookupIn(target, lookup()).defineClass(bytes)`. CratonVM's
`Lookup.defineClass` native rejected the bytes: **bad magic**.

## Root cause

CratonVM's `Lookup.defineClass` reads valid class bytes fine (verified). The
problem was upstream: the proxy **bytecode itself** was generated with a corrupt
magic. Weld builds it with `org.jboss.classfilewriter`, whose
`ByteArrayDataOutputStream extends java.io.DataOutputStream` uses `writeSize()`
to reserve a 4-byte length and later **back-patches** it at the position recorded
as `this.written` (the inherited `DataOutputStream.written` byte counter).

CratonVM's native `DataOutputStream` methods hardcoded the `written` field at
**slot 1**:

```rust
const DOS_FIELD_WRITTEN: usize = 1;   // WRONG
```

But the real JDK layout is
`FilterOutputStream{out, closed, closeLock}` → `DataOutputStream{written, …}`,
so `written` is at **slot 3**; slot 1 is `closed`. The natives were
*self-consistent* (native writer + native `size()` both used slot 1, so
`size()` returned the right count) — but a **subclass** reading the real
`written` via `getfield` saw a perpetual **0**. So
`ByteArrayDataOutputStream.writeSize()` recorded back-patch position 0, and
`getBytes()` overwrote **offset 0 (the class-file magic)** → `0xCAFEBABE`
became `0xFFFFFFFC`.

Minimal repro (no Weld):

```java
ByteArrayDataOutputStream s = new ByteArrayDataOutputStream();
s.writeInt(0xCAFEBABE);          // magic
LazySize ls = s.writeSize();     // records position = this.written
s.writeByte(0xAA); s.writeByte(0xBB);
ls.markEnd();
byte[] b = s.getBytes();
// HotSpot:  CA FE BA BE | 00 00 00 02 | AA BB
// CratonVM(before): FF FF FF FC | 00 00 00 00 | AA BB   <-- magic clobbered
```

## Fix

`native-io/src/lib.rs`: access `written` **by name** (`get_field_by_name` /
`set_field_by_name(this, "written", …)`) instead of the hardcoded slot 1, so the
native byte counter and the real bytecode's `getfield written` resolve to the
SAME slot. Touches the 3 sites that own `written`: `native_dos_init`,
`dos_write_one`, `native_dos_size`. (`out` at slot 0 was already correct.)

## Verification

- `BadosProbe`/`CfwProbe`: generated class-file magic is now `CA FE BA BE`
  (was `FF FF FF FC`); back-patched sizes correct.
- `DosProbe`: `DataOutputStream.size()` after writeInt/short/byte/array/long/UTF
  still correct (no regression).
- `ProxyTrace`: `@ApplicationScoped` client proxy now created
  (`…$Proxy$_$$_WeldClientProxy`), no `WELD-001524`.
- **All 12 sampled `cdi.*` tests PASS == HotSpot** (the 2 HibernateSearch
  classes that this fixes, plus the 10 already green).
- Regression suite 8/8 (incl. `RSerial`); **native-io 318 unit tests pass**.

## 2026-08-04 correction — "12 PASS" no longer reproduces (unrelated, earlier-in-bootstrap blocker)

A fresh 2026-08-04 residual run shows all 14 classes in this cluster FAILing
with `java.util.concurrent.RejectedExecutionException` out of
`WeldStartup.startInitialization` → `ConcurrentBeanDeployer.addClasses` →
`ForkJoinPool.invokeAll`. This happens during Weld's early `BeanDeployment
.createClasses` step, well before the client-proxy generation
(`WeldDefaultProxyServices.defineWithMethodLookup`,
`DataOutputStream.written`) this doc fixes is ever reached — so it is not a
regression of the `written`-slot fix itself (still `get_field_by_name`/
`set_field_by_name`, not the hardcoded slot). The blocker is a separate,
later-introduced bug: `CRATONVM_REAL_FORKJOINPOOL` became default-on
(`16ec5d7ad`, 2026-07-30), and the real-FJP bridge's method allow-list never
covered `ForkJoinPool.invokeAll(Collection)`, the overload Weld's
`ConcurrentBeanDeployer` calls. Full analysis and a confirmed workaround:
[`docs/internal/fixed-suite-bugs/hibernate/cdi-cluster-forkjoinpool-invokeall-rejectedexecution-FIXED-20260804.md`](cdi-cluster-forkjoinpool-invokeall-rejectedexecution-FIXED-20260804.md).
Re-verify this doc's "12 PASS" claim once the `invokeAll` gap is fixed.

## 2026-08-04 re-verification — done, and the claim is now stronger

The `invokeAll` gap is FIXED (see the doc linked above). All 14 `cdi.*` /
`jpa.cdi.*` classes were re-run on the fixed binary in the **default**
configuration (no `CRATONVM_SYNTHETIC_FORKJOINPOOL`), against a HotSpot
control on the same classpath and the same `@common.args`:

* HotSpot: 14/14 PASS
* CratonVM: **14/14 PASS**, per-class `found`/`ok` identical to HotSpot.

Weld's client-proxy generation (`WeldDefaultProxyServices
.defineWithMethodLookup` → `DataOutputStream.written`) is reached again now
that `ConcurrentBeanDeployer.addClasses` succeeds, so this doc's `written`-slot
fix is positively verified rather than merely unreachable. That supersedes the
original "12 sampled" claim — the sample is now the whole cluster.
