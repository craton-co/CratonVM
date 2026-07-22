# WildFly — spurious out-of-bounds `get_field` on zero-field factory objects

## Status
**OPEN (benign)** — boot continues; reads are dropped by GC guard.

## Severity
**LOW** — noise; possible latent correctness if guard masks wrong slot reads elsewhere.

## App / suite
- **Context:** WildFly standalone boot
- **Classes:** `org.jboss.as.controller.access.constraint.*$Factory` (`num_slots=0`)

## Symptom

```
gen_heap::get_field: out-of-bounds field read dropped
  class=org.jboss.as.controller.access.constraint.*$Factory index=… num_slots=0
```

Many lines during module/controller initialization.

## HotSpot behavior

No such warnings; virtual/interface dispatch on factory objects resolves without bogus field slot reads.

## CratonVM behavior

Interpreter or native fast path computes a **field slot index** on receivers with **zero instance fields** → guard drops read → likely returns zero/null.

## Root cause (suspected)

Virtual/interface method access incorrectly lowered to **`get_field` slot N** on a synthetic object with no fields — e.g. wrong receiver layout for lambda/factory stubs or interface default method dispatch.

Related pattern: [docs/comparison-handoff/bug-dacapo-avrora-getfield-oob.md](../../comparison-handoff/bug-dacapo-avrora-getfield-oob.md).

## Impact

- Boot noise; may affect constraint registration if dropped read should have been a real field
- Not the primary WildFly crash path after PathEntry fix

## Reproduce

Boot WildFly standalone with stderr visible:

```bash
org.jboss.as.standalone  # see apps/wildfly/CRATONVM_BUGS.md repro
grep get_field wildfly-daemon.log
```

## What to fix

1. Symbolize guard (`CRATONVM_SYMBOLIZE=1`) to get Java caller + receiver class.
2. Fix dispatch so factory/interface calls do not use raw slot indices on zero-field receivers.
3. Confirm warnings gone and constraint subsystem behavior matches HotSpot.

## Related

- [bug-wildfly-jar-signer-authenticated-attributes.md](bug-wildfly-jar-signer-authenticated-attributes.md)
- DaCapo avrora get_field OOB handoff doc
