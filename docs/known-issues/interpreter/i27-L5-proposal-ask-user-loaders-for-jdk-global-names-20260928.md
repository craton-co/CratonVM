# Proposal: ask a user loader for the JDK-global names it has not recorded (retire `is_global_resolution_namespace` in stages)

**Status: proposal, stage 2 landed without the census — filed 2026-09-28 by
interpreter round i1 wave 27, lane L5; wave 28 note below.** Follows
`interpreter-L5-a-user-loaders-class-under-a-jdk-name-is-resolved-to-the-jdks-class-FIXED-20261005.md` (fixed-bugs),
whose last open row this is.

## Problem, with evidence

`vm/src/runtime/interpreter/constants.rs` `is_global_resolution_namespace`
sends every `java/`, `javax/`, `jdk/`, `sun/` and `com/sun/` reference of a
user-loader class past that loader: `drive_defining_loader_load` declines the
names, so the loader's `loadClass` is never asked. Wave 27 answers a loader's
OWN definition of such a name (`loader_own_record_of_global_name`) and asks the
loader after a global MISS (`drive_defining_loader_load_after_global_miss`),
which fixes every row of `tools/probes/interp/L5/L5W27JdkNamedOwnClass.java`
but one: `lazy jdk class` — a child-first loader that defines its own
`javax.security.auth.x500.X500Principal` only when asked. The global route
finds java.base's class, which is a success, so no miss rescue runs; HotSpot
asks the loader (`SystemDictionary::resolve_instance_class_or_null` →
`loadClass`) and gets the loader's class. Real shapes: shading/relocating
test harnesses, OSGi-style child-first bundles that carry their own
`javax.*` API jars (`javax.annotation`, `javax.xml.bind` on JDK 25, which no
longer has them — those are misses and already work — versus
`javax.xml.*` / `javax.security.*`, which it still has).

## Design

Under `--jdk-only` only (the `--compatible` route stays byte-for-byte):

1. In `lookup_loader_initiated` / `resolve_class_loader_aware`, treat a
   non-`java/` global name like any other name for a user-defined referencing
   loader: own definition, then the initiating memo, then
   `drive_defining_loader_load` (the loader's `loadClass`), then the global
   fallback. The memo makes it one upcall per (loader, name).
2. Keep `java/` global (the loader cannot define it, and every loader
   delegates it).
3. `dispatch_static`'s owner arm and `resolve_field_ref_loader_aware` follow
   automatically (they go through the two functions); `site_fill_admitted`
   already covers memo answers with the resolution epoch.

## Expected win and how to measure it

Correctness: the `lazy jdk class` rows of `L5W27JdkNamedOwnClass` match
HotSpot. Cost: one `loadClass` upcall per user loader per distinct
`javax/`-`jdk/`-`sun/`-`com/sun/` name it references. Measure before landing:

* a census first — count, per run, the (user loader, global name) pairs that
  would be driven (`CRATONVM_DBG=access` style counter at the point step 1
  would call the drive), on the Spring Boot fat-jar and Tomcat webapp boots
  and the jdk-only corpus;
* then startup wall time of the same boots, A/B, fat LTO (`LaunchedClassLoader`
  classes reference `jdk/internal/...` and `javax/...` names routinely).

## Cost / risk

A `loadClass` override runs arbitrary code; some loaders throw on
`javax.*` rather than delegating (then the global fallback must still
answer, as the miss rescue does today). Deadlock surface: the same as any
other VM-initiated `loadClass`
(`i27-L5-a-non-parallel-capable-loader-gets-a-per-name-lock-20260928.md`).

## Staged plan

1. Census counter only (both modes), no behaviour change.
2. `--jdk-only` behind the census reading, with the probe row as the gate.
3. Decide `--compatible` separately (AGENTS.md).

## Wave 28 note — lane L5

Stage 2 landed under `--jdk-only`, in a narrower form that makes the census
less urgent: only a loader whose delegation chain overrides `loadClass` is
asked (`classloader_real::loader_answers_jdk_names_as_the_jdk`, one verdict
per loader, kept in `ClassRealm::loader_global_name_transparency`); a loader
that overrides nothing is never asked, because its base parent-first
delegation answers a runtime-image name exactly as the global route does. The
per-resolution cost of the common case is one read-locked map probe (no Java,
no epoch bump). See the i26 page's wave-28 progress for the whole design and
its remaining doors.

The census (stage 1) is now a measurement of the landed change rather than a
gate: `CRATONVM_DBG_ISOLATED_CNF=1` prints one `[ISOLATED-CNF] global-name
drive name=... loader=... answer=...` line per ask; count them on the Spring
Boot fat-jar and Tomcat webapp boots under `--jdk-only`, and A/B the boot wall
time against the pre-wave-28 build (fat LTO, interleaved). Expected: some
hundreds of asks for a `LaunchedClassLoader` boot (one per distinct
`javax/`-`jdk/`-`sun/`-`com/sun/` name its classes reference), each one
`loadClass` upcall; if that shows above the host's noise floor, the next step
is a per-loader learned verdict (a loader whose first N asks all delegated)
— unsound for a filtering loader, so only behind a flag. Stage 3
(`--compatible`) is unchanged: the owner's decision.
