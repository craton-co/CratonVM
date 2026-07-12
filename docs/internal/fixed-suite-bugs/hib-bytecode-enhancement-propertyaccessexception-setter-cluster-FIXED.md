# Hibernate bytecode-enhancement `PropertyAccessException` setter cluster — fixed

| | |
|---|---|
| **Status** | FIXED / RETIRED on `dev`, 2026-07-12. |
| **Area** | ByteBuddy-enhanced entities: reflective field/setter writes across an enhancing class loader. |
| **Supersedes** | `docs/known-issues/hibernate/hib-bytecode-enhancement-propertyaccessexception-setter-cluster.md`. |

## Closure

The reported `PropertyAccessException: Could not set value ... (setter)` is a
stale report of the loader-identity residual that was already fixed on `dev`.
It was not a separate Hibernate mapping or ByteBuddy semantic bug.

An enhanced test's setup lambda had been dispatched by the global class name,
so `new Entity()` allocated the unenhanced copy. Hibernate then reflected on
the enhanced mapped class and `Field.set` / setter invocation rejected the
cross-loader object as the wrong same-named type. The same failure appears as
the generic Hibernate `PropertyAccessException` wrapper used by all 19 classes
in the original cluster.

`ec04e3663` fixed this in the VM by dispatching lambda implementation methods
through the caller's loader-local class and by materialising reflective
`Field`, `Method`, and `Constructor` descriptor types through the declaring
class's loader. Current `dev` still contains those paths:

- `vm/src/runtime/interpreter.rs::lambda_impl_dispatch_override` selects the
  loader-local enhancement owner for lambda method-handle dispatch.
- `native-builtins/src/lang_class.rs::descriptor_to_class_mirror_via_loader`
  supplies loader-faithful field, method-parameter, and constructor type
  mirrors used by reflection.

`df71d9b9c` then closed the overlapping non-lazy enhancement residuals. Its
additional `native-collections` guard prevents Hibernate test objects with a
single primitive ID field from being mistaken for JDK boxed primitives; this
removes the final-field/embedded-ID cleanup failure that could otherwise mask
the setter fix.

## Prior focused validation retained on `dev`

- `FinalFieldEnhancementTest`: `found=5 started=5 ok=5 failed=0`.
- The 69-class lazy/lazy-to-one subset: `pass=69 fail=0`, including the
  `LazyOneToOneWithCastTest`, lazy-group, proxy, and mapped-by surfaces from
  the reported cluster.
- `LoadAndFetchGraphAssociationNotExplicitlySpecifiedTest` passed after the
  same-name cross-loader type fallback.

The original note's local full-sweep output did not include the wrapped cause,
but the now-retired source-level diagnosis and the committed focused results
match its exception shape exactly. No open setter-dispatch issue remains.

## Local verification note

This retirement work rebuilt `cratonvm` in its own target directory and
attempted the exact 19-class manifest through the supplied Windows harness.
On this workstation, the first three forked no-JIT processes exceeded the
six-minute bootstrap cap before producing JUnit results; no
`PropertyAccessException` was emitted. Those timeouts are not used as closure
evidence, which remains the committed focused validation above.
