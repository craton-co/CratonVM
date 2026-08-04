# `upgrade_synthetic_class` never rebuilds a vtable — investigated, not a bug

**Status: NEGATIVE RESULT — 2026-08-01.** No behaviour change shipped; the
invariant that makes it safe is now enforced by a test.

## The suspicion

Raised while fixing
[`mockito-spy-outer-invokeinterface-call-not-recorded-FIXED-20260801.md`](mockito-spy-outer-invokeinterface-call-not-recorded-FIXED-20260801.md).
`ClassManager::upgrade_synthetic_class` replaces a synthetic JDK stub with real
`.class` bytecode and fires `fire_jit_invalidate_hook` /
`fire_resolution_invalidate_hook`, but — unlike `redefine_class` step 6 — never
calls `build_vtable_descriptors` or `fire_vtable_install_hook`. If a stub class
had a vtable and any call site was warm enough to reach
`execute_invokevirtual_vtable_fast`, that site would keep dispatching the
**stub's** `Arc<CachedBytecodeMethod>` after the upgrade — the exact shape of
the JEP 109 bug, on a path where the method set can change outright, so the new
`refresh_inherited_vtable_descriptors` would not even have been sufficient.

## Why it cannot happen

Two structural properties, both now pinned by
`class_manager::tests::synthetic_stub_has_no_vtable_and_no_dispatchable_body`:

1. **A compatibility stub never has a vtable.** Both mint paths —
   `create_synthetic_stub` and `fabricate_class` (behind
   `ensure_synthetic_class` / `try_ensure_synthetic_class`) — push straight into
   `class_store` with no `build_vtable_descriptors` call and no install hook.
   `execute_invokevirtual_vtable_fast` therefore takes its
   `guard.get_vtable(receiver_class_id) => None` exit and answers `CacheMiss`
   for any stub receiver. There is no stub-derived vtable entry to go stale.
2. **A stub has no dispatchable body.** `synthetic_stub_ctor_methods` builds
   every method `ACC_NATIVE` with `attributes: vec![]`, so `method.code()` is
   `None` and no `Arc<CachedBytecodeMethod>` can be constructed from a stub
   method at all. Nothing stale can therefore be sitting in a thread's
   `InvokeCache` or in `promoted_invokes` either — which matters, because this
   path deliberately does **not** bump `redefine_generations`, so `RedefineGate`
   would never evict such an entry if one existed.

The mutation check: re-running the test with a `build_vtable_descriptors` +
`vtable_descriptors.insert` inserted on the freshly-minted stub makes assertion
1 fail with "a synthetic stub must never carry vtable descriptors". The test is
not vacuous.

## Empirical census

A `CRATONVM_DBG_SYNUPGRADE=1` build (diagnostic not retained) over
`ConversionServiceParameterValueMapperTests` under the spring-boot suite
runner, with prints on all three upgrade call sites *and* on the upgrade itself:

| | count |
|---|---:|
| upgrade guard evaluated | 6277 |
| …with a genuine stub (`is_synthetic=true`) | 3205 |
| …of those, short-circuited by the known-absent memo | 3190 |
| …of those, scanned the classpath and found nothing | 15 |
| **`upgrade_synthetic_class` actually executed** | **0** |

The probe is demonstrably live (6277 firings); the upgrade simply never
happens. Every stub in the process is a VM-internal shape whose real bytes
exist nowhere: `cratonvm/internal/Unmodifiable*`, `cratonvm/stream/LazyOp`,
`java/util/HashMap$KeyItr`, `java/util/LinkedHashMap$Node`,
`java/lang/annotation/AnnotationProxy`, `java/lang/reflect/Proxy$Instance`,
`java/util/function/Predicate$$Lambda$*`, `java/util/Comparator$Native`.
`--dump-class-origins` on a hello-world agrees: 14 stubs, all of that family.

(As of `6ca262993`, `java/util/LinkedHashMap$Node` is no longer among them:
LinkedHashMap nodes bind to the real `java/util/LinkedHashMap$Entry`, so the
list is one shorter than measured here. The counts above are left as measured
on 08-01.)

Two positive controls also failed to trigger it, which is itself informative:

* Planting a real `cratonvm/internal/UnmodifiableList.class` on the app
  classpath — the scan runs (`known_absent=false`) and still finds nothing.
  Stubs are Bootstrap-owned, so `find_class_bytes_delegated` does not reach an
  application-classpath directory for them.
* `Class.forName("org.jboss.Foo")` with the bytes absent throws
  `ClassNotFoundException` rather than fabricating — the reflective-probe gate
  refuses to mint a stub, so the "stub first, real bytes later" sequence the
  enterprise-prefix fallback was written for is not reachable from reflection.

## What the gap actually costs

Dispatch **coverage**, never correctness. An upgraded class still has no vtable
afterwards, and a subclass linked while its parent was a stub seeded its own
descriptor vec from the parent's absent one. Both outcomes are a `lookup_slot`
miss ⇒ `CacheMiss` ⇒ the correct slow path. Restoring the fast path for that
population would newly route classes through
`execute_invokevirtual_vtable_fast`'s ~15 special-case guards for the first
time — a broad dispatch change whose measured upside here is zero calls. Not
shipped.

If a future change gives stubs a vtable, or gives stub methods a Code
attribute, the test above fails and points here.
