# Proposal: a flat-store define is told apart from a built-in loader's own define, so a loader constraint can refuse the latter

**Status: proposal — filed 2026-10-10 by interpreter round i1 wave 46, lane
L5. Not implemented. It takes over the "built-in loader defines admitted
(counted)" item of
`docs/internal/fixed-bugs/interpreter-L5-loader-constraints-are-not-imposed-at-member-resolution-FIXED-20261010.md`.**

## The problem

JVMS §5.3.4: a define that would break a loader constraint fails with
`LinkageError` (HotSpot's `SystemDictionary::check_constraints`).
CratonVM checks it at define under `--jdk-only`
(`ClassManager::loader_constraint_define_refusal`, wave 37), but refuses only
a USER-DEFINED defining loader. A built-in loader's define (bootstrap,
platform, application) that breaks a constraint is counted and traced
(`[ACCESS-DBG] LOADER-CONSTRAINT (counted, admitted) built-in define …`) and
admitted, because the same define path also carries CratonVM's own
flat-store defines: the loader-blind fallbacks that define a class-path name
into the application (or bootstrap) namespace where HotSpot defines
nothing (a global route's answer after a user loader refused, a stub
upgrade, an eager load the VM makes for itself). Refusing one of those would
be a `LinkageError` HotSpot never throws; admitting all of them also admits
the real violation (an application-loader class defined after a user loader
pinned the name to its own).

## The direction

1. Carry the define's ORIGIN to the define path: a define the loader
   itself asked for (the application loader's `findClass` / `defineClass`,
   the boot class path's own load for a resolution the bootstrap loader
   initiated) versus a VM fallback define (the flat route's
   `load_class_concurrent` for a name another loader refused, a stub
   upgrade). `classloading/src/class_manager.rs` `DefineClassOptions`
   (which already carries `hidden`, `skip_verification` and the code
   source per define) is where one more field would go; the class-path
   loads that do not build options need the default to be "fallback".
2. `loader_constraint_define_refusal` refuses the first kind for every
   loader, as HotSpot does, and keeps counting the second.
3. Then measure: every `(counted, admitted)` line of the suite, the jdk-only
   corpus, Spring Boot and Tomcat under `CRATONVM_DBG=access` is either a
   fallback define (it stays admitted) or a real violation (a latent
   confusion to read before enforcing). The census rule of waves 31-42.

## Probe

A user loader `a` resolves a method of an application class whose
descriptor names `p.Shared`, with `a`'s own `p.Shared` loaded first (the
constraint is recorded, pinned to `a`'s class), then the application loader
loads its `p.Shared` for the first time: HotSpot throws `LinkageError`
("loader constraint violation: loader 'app' wants to load class p.Shared. A
different class with the same name was previously loaded by 'a' …");
CratonVM admits the define and counts it.
