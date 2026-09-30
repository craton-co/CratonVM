# Proposal: link only javac's shapes natively, and every other `invokedynamic` through the JDK's own bootstrap

**Status: proposal, filed 2026-10-02 by interpreter round i1 wave 38, lane
L4.** Not implemented.

## Why

CratonVM links four JDK bootstraps natively (`execute_invokedynamic_at` in
`vm/src/runtime/invokedynamic.rs`): `LambdaMetafactory`, `StringConcatFactory`,
`SwitchBootstraps` and `ObjectMethods`. Each native linkage assumed javac's
shape, and waves 32-38 have been re-implementing, one message at a time, the
checks each JDK bootstrap makes on every other shape:

* the lambda checks (`lambda_site_refusal`, waves 36-38: 30 rows of
  `L4W37LambdaSiteValidation`, 19 of `L4W38LambdaSiteRecords`, and three
  rows still undecided on
  `i37-L4-lambda-site-validation-leaves-undecidable-shapes-linked-20261001.md`);
* the concat checks (`concat_needs_the_jdk_factory`, wave 35), and in wave 38
  the compiled concat bridge's copy of the same assumption
  (`jit_concat_site_is_pool_decided`);
* the switch and record checks (`switch_site_refusal`,
  `object_methods_site_refusal`, wave 38), with the record getters still
  ignored (`i38-L4-objectmethods-native-linkage-ignores-its-getters-20261002.md`).

Every one of these is a hand-assembled shape. Each re-implementation is a
copy of JDK logic that can drift from the JDK it copies, and each message
had to be measured.

## Direction

1. **One recogniser per bootstrap for the shapes javac emits**, as exact as
   the checks above are loose: `metafactory` with three static arguments of
   the right kinds whose types all resolve; `makeConcatWithConstants` whose
   constants are strings and numbers; `typeSwitch`/`enumSwitch` of
   `(T,int)int` with `Class`/`String`/`Integer`/`EnumDesc` labels; `ObjectMethods`
   with one `REF_getField` per record component, in component order, from the
   record itself. A recognised site links natively, exactly as today.
2. **Every other site links through `bootstrap_generic`**: the JDK's own
   bootstrap, which raises the JDK's own error for a refused shape and
   builds the JDK's own call site for an accepted one. The refusal code of
   waves 35-38 then becomes the recogniser's `false` branch and can go.
3. **Prerequisite, to measure first**: the JDK bootstraps must LINK on
   CratonVM for accepted non-javac shapes. Wave 35 measured that
   `StringConcatFactory`'s handle chain fails with `AbstractMethodError` for a
   `Class` constant; `SwitchBootstraps.generateTypeSwitch` spins a hidden class
   with the ClassFile API; `ObjectMethods` composes `guardWithTest` /
   `filterArguments` / `permuteArguments`. Run each probe row of the four
   wave-37/38 validation probes that HotSpot LINKS (the `*-ok` rows, plus
   variants that differ from javac's shape but are legal) through
   `bootstrap_generic` with a debug flag first.

## Expected win and cost

No new behaviour for javac output (every site keeps its native linkage, and
the recogniser's cost is paid once per site at link time). What goes away is
the class of divergence this lane has filed in waves 29, 32, 35, 36, 37 and
38. Measure with a census (`CRATONVM_DBG_INDY_ALL=1`, one line per linkage)
on the Spring Boot corpus and the regression suite that every site there is
recognised, so the generic route is never taken by real code.

## Risks

A recogniser that is too narrow sends javac output down the slower generic
route (the census is the guard); one that is too wide keeps today's
divergences. The generic route's cost for a hand-assembled site is
irrelevant.

## Progress (wave 39) — lane L4

The `ObjectMethods` half of step 1 was built for a defect fix
(`i38-L4-objectmethods-native-linkage-ignores-its-getters-20261002.md`):
`Class::object_methods_args_are_canonical` recognises javac's getter list
(the record itself, one `REF_getField` per component in component order,
naming instance field `i`). A recognised site links natively as before; a
site whose getters are all `REF_getField`s of the record links natively
GETTER-DRIVEN (every mode); any other getter takes `bootstrap_generic` under
`--jdk-only` (step 2, for `ObjectMethods` only). Step 3's measurement for
`ObjectMethods` is the `accessor-getter` row of
`tools/probes/interp/L4/L4W39ObjectMethodsGetters.java`. The other three
bootstraps are untouched.
