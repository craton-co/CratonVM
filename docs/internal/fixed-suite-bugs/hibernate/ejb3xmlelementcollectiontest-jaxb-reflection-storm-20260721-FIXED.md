# Hibernate JAXB reflection-storm timeout cluster (fixed 2026-07-21)

## Resolution

`Ejb3XmlElementCollectionTest` was not stuck in `retainAll`.  Its JAXB model
construction repeatedly crossed native reflection bridges while the interpreter
stack was 140+ frames deep.  Every ordinary native return rebuilt the complete
GC root snapshot, even though the running thread cannot be collected until it
reaches a safepoint or a blocking transition.  That made each bridge return
O(stack depth) and exhausted the suite's 300-second class budget.

Native object returns and native-thrown exceptions now remain in
`native_pending_return` until the next collector-visible boundary.  Safepoints
publish the current frame roots before collection, and blocking natives retain
their existing `deposit_root_snapshot` protocol.  This removes the redundant
running-thread snapshot rebuild without weakening GC visibility.

The default background tier policy had a related startup starvation: the
interpreter began reporting hot calls at 500 invocations, but the background
worker did not admit C1 until 1500.  The C1 threshold is now 500 while the
conservative C2 threshold remains unchanged, so reflection-heavy bootstrap
code reaches the worker before the short-lived test process spends its budget
interpreting it.

Files changed:

- `vm/src/vm/vm_exec.rs`
- `vm/src/runtime/env_cache.rs`
- `jit/src/tiered.rs`

## Validation

Built `cratonvm.exe` in the isolated target directory
`C:\craton\cargo-target-ejb3xml-rootsnapshot-20260721-019f874b` and ran the
real Hibernate fixture through `apps/hib-suite-runner` with JDK 25.0.3.

The exact linked six-class cluster passes in both modes (68 tests in each run):

| Class | JIT | `--nojit` |
|---|---:|---:|
| `Ejb3XmlElementCollectionTest` | 28/28, 172.8 s | 28/28, 179.6 s |
| `Ejb3XmlManyToOneTest` | 9/9, 37.3 s | 9/9, 40.8 s |
| `Ejb3XmlOneToOneTest` | 11/11, 53.0 s | 11/11, 61.5 s |
| `XmlAccessTest` | 9/9, 43.9 s | 9/9, 44.9 s |
| `XmlProcessingSmokeTests` | 5/5, 40.7 s | 5/5, 38.1 s |
| `HbmTransformationJaxbTests` | 6/6, 55.7 s | 6/6, 55.0 s |

The tiered-policy unit suite also passes serially: 57/57.  A parallel attempt
had one pre-existing process-global OSR deny-list race; the serial run removes
that cross-test state interference.
