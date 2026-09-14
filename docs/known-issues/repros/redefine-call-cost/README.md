# Redefinition call-cost reproducer

Measures what a JVMTI class redefinition costs *per subsequent call* into the
redefined class, and guards the fix that closed it.

Mockito is deliberately **not** used. Mockito's inline mock maker was only ever
a route to `Instrumentation.redefineClasses`; the cost being measured is the
VM's. Dropping it makes this runnable anywhere in about a second, with no jars.

Both probes redefine `Victim` and then call a hot method:

* `RedefineCostProbe` — redefines with **byte-identical** bytecode, so nothing
  about the class changes and any slowdown afterwards is purely the cost of the
  VM having marked the class redefined.
* `RedefineCorrectnessProbe` — redefines with a body whose arithmetic differs
  (steps by 100, not 1) and asserts the new body is observed **both**
  interpreted and after the method re-tiers into the JIT. This is what stops
  the perf fix from being traded for a stale-body bug: while redefined classes
  were barred from compiling, its "hot" assertion was vacuous.

`cratonvm/Instrument.java` is a declaration-only stand-in so
`Class.forName("cratonvm.Instrument")` resolves and binds to the VM's
registered native. Without it the probes silently degrade to a control run.

## Build

```bash
javac -d alt alt/RedefineCorrectnessProbe.java
javac -d .   cratonvm/Instrument.java RedefineCostProbe.java RedefineCorrectnessProbe.java
```

## Run

```bash
cratonvm -cp . RedefineCorrectnessProbe   # must print PASS
cratonvm -cp . RedefineCostProbe          # multiplier should be ~1x
```

HotSpot runs both as a control: with no agent installed the redefine is
skipped, which shows the measurement loop itself is stable.

## Reading the result

Wall-clock on a loaded machine is noisy — the BEFORE column has been observed
between 314 and 2,574 ns/call on the same binary. Prefer the counter, which is
not:

```bash
CRATONVM_DBG_HOTPATH_COUNTS=1 cratonvm -cp . RedefineCostProbe
```

`retarget_field` counts interpreter field-access dispatches. Before the fix it
climbed to 6,000,000 across the post-redefine phase (every call interpreted);
after it, it stays at ~9,000 — the method stays compiled through the
redefinition.

Full analysis: `mockito-redefine-makes-every-call-40us-20260726.md`.
