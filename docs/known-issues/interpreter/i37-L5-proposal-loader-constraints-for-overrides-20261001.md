# Proposal: impose JVMS §5.3.4 loader constraints when an override crosses loaders

**Status: proposal, superclass half built in wave 39 and itable half in wave
40 (below) — filed 2026-10-01 by interpreter round i1 wave 37, lane
L5. The last §5.3.4 site the VM does not cover (member resolution: waves 31
and 37; define and initiating load: wave 37).**

## Progress (wave 40) — lane L5: the itable half is built

The i29-L5 defect fix built the itable half in the same function
(`check_override_constraints_at_link`): each superinterface method against
the method the class selects for it (its own or a superclass's
declaration, else the one concrete maximally-specific default), with
HotSpot's `klassItable` message. Probe `L5W40ItableConstraint`. What is left
of the proposal is only its census-before-refusal step (both halves refuse
at once; the orchestrator's `CRATONVM_DBG=access` run is the census), so the
proposal can be retired once that census reads zero.

## Progress (wave 39) — lane L5: the superclass half is built

The i29-L5 defect fix built the vtable half at LINK rather than in
`ClassManager`'s layout pass: `vm/src/runtime/resolve/loader_constraints.rs`
`check_override_constraints_at_link`, from `vm/src/vm/vm_util.rs`
`link_claimed_class`, `--jdk-only`. Working at link keeps it out of the
class-manager write guard, so the member-resolution machinery (views,
recording, loader labels read from the loader objects) is reused as it is,
and the error surfaces where HotSpot raises it (linking `Sub`). Probes
`L5W37LoaderConstraintOverride` and `L5W39OverrideConstraintPending`. Not
built: the itable half (a class's method implementing a superinterface
method of another loader, and a default method selected across loaders),
and the census-before-refusal step: the wave refuses at once, like wave 37's
define check; the orchestrator's `CRATONVM_DBG=access` run is the census.

## The gap

JVMS §5.3.4, second bullet: when a class C (loader L1) declares a method that
overrides (§5.4.5) a method declared in a supertype D (loader L2), every class
name N in the method's descriptor must satisfy N^L1 = N^L2. HotSpot imposes
it while building C's vtable (`klassVtable::check_loader_constraints`) and
itable, and refuses the link:

```
java.lang.LinkageError: loader constraint violation for class p.Sub: when selecting overriding method 'int p.Sub.take(p.Shared)' the class loader 'a' @H of the selected method's type p.Sub, and the class loader 'b' @H for its super type p.Base have different Class objects for the type p.Shared used in the signature (p.Sub is in unnamed module of loader 'a' @H, parent loader 'b' @H; p.Base is in unnamed module of loader 'b' @H, parent loader 'app')
```

(`tools/probes/interp/L5/L5W37LoaderConstraintOverride.java`, HotSpot 25,
`java` and `-Xint`.) CratonVM builds vtables in
`ClassManager::build_vtable_descriptors_with_overrides`
(`classloading/src/class_manager.rs`; the §5.4.5 predicate is
`classloading/src/method_override.rs` `can_override`) with no constraint, so `Sub` links, and a call
through `b`'s `Base.take` hands `b`'s `Shared` to a method verified against
`a`'s: the probe prints `link=ok`, `call=7` in every mode (from the code).
A call from a class of loader `a` through `Base` is caught since wave 31 at
the call site's resolution, with the "when resolving method" message instead.

## The design

* Where: the vtable pass, once per class at link, after the override
  decision (only a method that actually overrides — `can_override` — and only
  when the overridden
  method's declaring class has another loader).
* What: for each descriptor name outside the JDK-global namespaces
  (the wave-37 recorder's filter): both loaders' current views
  (`loaded_class_under_exact_key` / the initiating memo; the layout pass holds
  the class-manager write guard, so no Java and no heap reads) → a direct
  comparison when both see a class, else `impose` + `pin` in
  `ClassManager::loader_constraints` (a `get_mut`, no lock). A violation
  under `--jdk-only` fails the define/link with HotSpot's message; the loader
  labels come from `LoaderConstraints::label` (stored by the recorders; the
  define natives would need to store the defining loader's label before
  `define_class_full`, since this pass cannot read the loader object).
* Cost: one extra loop over the overriding methods of a class whose supertype
  has another loader — the layout pass already walks them.
* Measure first: count, do not refuse, over the suite, Spring Boot, Tomcat
  (webapp classes overriding container interfaces: `jakarta/` names are
  recorded) and the jdk-only corpus; a count on a green workload is either a
  latent confusion or a CratonVM view that is not the loader's answer.

`--compatible`: unchanged (census only, if at all).
