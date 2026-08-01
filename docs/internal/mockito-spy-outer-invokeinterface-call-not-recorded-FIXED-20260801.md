# A JVMTI redefinition left every descendant's vtable descriptors pointing at the pre-redefinition body

**Status: FIXED — 2026-08-01** (was
`docs/known-issues/springboot/mockito-spy-outer-invokeinterface-call-not-recorded-20260731.md`,
found 2026-07-31)

## Symptom

`ConversionServiceParameterValueMapperTests.mapParameterShouldDelegateToConversionService`
(`module/spring-boot-actuator`) failed a Mockito verification, intermittently —
9 PASS / 11 FAIL over 20 single-class runs on 2026-08-01 dev, and 18/20 PASS in
an earlier, less loaded sample of the same binary:

```
=> Argument(s) are different! Wanted:
defaultFormattingConversionService.convert("123", class java.lang.Integer);
Actual invocations have different arguments:
defaultFormattingConversionService.convert("123", java.lang.String, java.lang.Integer);
defaultFormattingConversionService.getConverter(java.lang.String, java.lang.Integer);
```

The spied service's *outer* `convert(Object, Class)` call was missing from
Mockito's invocation history, while the two internal self-calls that the same
method makes were recorded. The real method still executed correctly (`mapped`
equalled `123`), so the call happened — it was simply never intercepted.

## Root cause

**Not** the `invokeinterface`-vs-`invokevirtual` distinction the original
report proposed. The discriminator is whether the *call site* had already been
executed once before the retransformation:

| call site | first executed | outer call recorded |
|---|---|---|
| `ConversionServiceParameterValueMapper.mapParameterValue` | **before** `spy()` | **no** |
| a fresh `invokeinterface` site in the probe | after `spy()` | yes |
| a fresh `invokevirtual` site in the probe | after `spy()` | yes |
| the same warmed site, called again | — | still no (permanent) |

A warmed call site reaches `execute_invokevirtual_vtable_fast`
(`vm/src/runtime/interpreter/invoke.rs`), which dispatches the
`Arc<CachedBytecodeMethod>` snapshot stored in the receiver class's vtable
slot. Instrumented at frame-push, that snapshot was `code_len=22` — the
original `GenericConversionService.convert(Object,Class)` body — where every
other path ran the Mockito-woven `code_len=164` one.

`ClassManager::build_vtable_descriptors_with_overrides` seeds a subclass's
descriptor vec by `extend_from_slice`-ing its superclass's, so **each
descendant owns a copy of the ancestor's slot, bytecode snapshot included.**
`redefine_class` step 6 rebuilt only the redefined class's own vec. The VM-side
`VtableManager::install_vtable` does sweep its installed tables for inherited
entries declared by the redefined class, which is why the bug was not
universal — but nothing refreshed the *class-loader-side* copies that the next
`build_vtable_descriptors` reads from.

That is what made it intermittent. Mockito's
`InlineBytecodeGenerator.triggerRetransformation` collects the mocked type plus
its whole superclass chain into a `HashSet` and hands
`Instrumentation.retransformClasses` the set's `toArray`, so the per-class
redefines arrive in an order that varies run to run (`Class` identity hashes).
With `DefaultFormattingConversionService` → `FormattingConversionService` →
`GenericConversionService`:

* `GenericConversionService` redefined **last** → its `install_vtable` sweep
  fixes every descendant table → test passes.
* `GenericConversionService` redefined **first**, a descendant later → the
  descendant's own step-6 rebuild reads the stale parent copy and
  **re-installs the original body over the table the sweep had just fixed** →
  the woven advice never runs for that call site again.

The same hole also silently mis-served any subclass *linked* after a
redefinition, which would have inherited the pre-redefinition snapshot.

## Fix

`ClassManager::refresh_inherited_vtable_descriptors`
(`classloading/src/class_manager.rs`), called from `redefine_class` step 6
right after the redefined class's own vec is rebuilt: re-point every other
cached descriptor vec's slots that are attributed to this class at the freshly
built slot, keyed on `(method_name, descriptor)`. A slot with no counterpart
in the rebuilt vec is set to `None` (fail closed — JEP 109 forbids adding or
removing methods, so this is unreachable in practice, and a slow-path
resolution beats a body we know is stale). Mirrors
`VtableManager::install_vtable`'s existing sweep, and is likewise
O(total cached slots) on the cold JVMTI path only.

Regression test:
`class_manager::tests::redefine_refreshes_inherited_vtable_descriptors_in_descendants`.

## Reproducer

The one-shot JUnit class only fails ~50 % of the time. Warming the call site
explicitly makes it deterministic — one non-spied call through
`ConversionServiceParameterValueMapper.mapParameterValue` before the first
`spy()`:

```java
new ConversionServiceParameterValueMapper(new DefaultConversionService())
    .mapParameterValue(new TestOperationParameter(Integer.class), "123");   // warm

DefaultFormattingConversionService spy = Mockito.spy(new DefaultFormattingConversionService());
new ConversionServiceParameterValueMapper(spy)
    .mapParameterValue(new TestOperationParameter(Integer.class), "123");
// Mockito.mockingDetails(spy).getInteractions() has no convert(Object,Class)
```

Before: 20/20 iterations missing the outer call, with `--nojit` as well as with
the JIT on (the defect is interpreter-side; the earlier "12/12 clean under
`--nojit`" observation was test-ordering luck, not a JIT dependency).
After: 0/50 with the JIT on, 0/50 with `--nojit`, and 20/20 PASS on the JUnit
class.

## Regression evidence

| arm | measurement |
|---|---|
| `ConversionServiceParameterValueMapperTests`, 20 single-class runs, pre-fix binary | 9 PASS / 11 FAIL |
| same, post-fix binary | **20 PASS / 0 FAIL** |
| 16 suite classes that call `Mockito.spy(...)`, x2 runs each, pre-fix | 30/32 PASS (2 = this bug) |
| same, post-fix | **32/32 PASS** |
| `module/spring-boot-actuator`, 82 classes, pre-fix vs post-fix | identical: 81 PASS / 1 FAIL (`GitInfoContributorTests`, a separate open issue) |
| `cargo test -p cratonvm-classloading` | 720 + 118 pass |

## Adjacent gap — investigated separately, NOT a bug

`ClassManager::upgrade_synthetic_class` (synthetic JDK stub → real `.class`
bytecode) fires the JIT and resolution invalidation hooks but never rebuilds
any vtable descriptors at all — not even the upgraded class's own. That looked
like the same shape and was chased down on 2026-08-01: it is sound, because a
compatibility stub never has a vtable in the first place and its methods carry
no Code, so there is nothing stale to serve. See
[`synthetic-stub-upgrade-vtable-NOT-A-BUG-20260801.md`](synthetic-stub-upgrade-vtable-NOT-A-BUG-20260801.md);
the invariant is now enforced by
`class_manager::tests::synthetic_stub_has_no_vtable_and_no_dispatchable_body`.

## Diagnostic that settled it

A frame-push trace of the executed bytecode length. A retransformed body is
materially longer than the original (22 → 164 bytes here), so the length alone
separates "the agent's woven bytecode ran" from "a pre-redefinition snapshot
ran" — a question no Java-level observation of the mock could answer, because
both bodies compute the same result and only the woven one calls Mockito.
