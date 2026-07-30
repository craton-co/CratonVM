# Framework throughput program

Framework/compiler throughput is a first-class performance lane, separate from
correctness firefighting. Its purpose is to prevent class definition,
redefinition, parsing, reflection, and generated-code workloads from regressing
behind one-off compatibility fixes.

## Named workloads

- Mockito redefine: pre/post-mock steady-state virtual-call latency and
  `StringBuilder.length()` throughput.
- Spring AOT/CGLIB: generated-class definition, loader identity, startup time,
  and steady-state proxy dispatch.
- Jython/compiler-parser startup: import/compile wall time and allocation.
- The published CPU deficits: HashMap, Binary Trees, and String/Regex.

## Gate policy

Every run records CratonVM and HotSpot JDK 25 on the same host with alternating
fresh processes. Correctness is mandatory before timing. A change fails review
when a workload exceeds its committed budget without an updated measurement,
root-cause note, and explicit approval.

Redefinition caches are keyed by class generation; unrelated redefinition must
not disable process-wide JIT, quickening, native-shadow caches, lambda caches,
or inherited vtable dispatch. Generated-class throughput is measured together
with exact loader/superclass identity so a fast wrong answer never passes.

Raw measurements belong with the benchmark baselines. Durable regressions get a
document under `docs/known-issues`; fixed analyses move under `docs/internal`.
