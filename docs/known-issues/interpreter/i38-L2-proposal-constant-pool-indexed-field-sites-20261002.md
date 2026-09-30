# Proposal: constant-pool-indexed field sites, so a compile door can defer one field site

**Status: proposal — filed 2026-10-02 by interpreter round i1 wave 38, lane
L2. Not implemented.**

## Problem

A compiled `new`, `anewarray`, `checkcast`, `instanceof` or `ldc <Class>`
site whose class the compile door cannot (or may not) know yet compiles to a
helper that resolves `(holder, cp index)` at the program point, through the
holder's loader, with the interpreter's checks (`jit_new_object_cp`,
`jit_anewarray_object_cp`, the holder-keyed type-check sites, `jit_ldc_class_cp`).
A field site has no such form: every door resolves it at compile time with
the threadless `field_access::resolve_field_ref`, and a site with no
`field_info` / `static_field_info` row makes the single-pass backend refuse
the whole method (`unresolved_field_site`; the optimizing tier likewise).

Two defects follow, both recorded:

* `interpreter-L5-a-user-loaders-class-under-a-jdk-name-is-resolved-to-the-jdks-class-FIXED-20261005.md` (fixed-bugs)
  ("Progress (wave 38)"): a cold `getstatic` / `getfield` of a JDK-global
  owner that a child-first loader defines itself on request compiles against
  java.base's field. Deferring the site as `new` sites are deferred would
  refuse every such method until the site runs.
* `docs/internal/fixed-bugs/interpreter-L2-compile-door-review-items-left-open-RETIRED-20261010.md`, item 3: the
  eager first-call door refuses (and seals) a method whose static field's
  owner is not loaded yet at the method's first call.

## Proposal

Four helpers, `jit_getstatic_cp` / `jit_putstatic_cp` / `jit_getfield_cp` /
`jit_putfield_cp`, taking `cratonvm_jit::cp_holder_word(holder)` and the cp
index (the stamp translates the index across a redefinition, as the other
cp-indexed helpers do), resolving with `resolve_field_ref_loader_aware` on
the calling thread (JVMS §5.4.3: record the resolution per site; a failure
is recorded and rethrown), running the §6.5 static/instance and final-put
checks and class initialization at the program point, then performing the
access through the value-tagged slot. A compile door emits one of them for a
site it cannot bind instead of refusing the method; the resolved-row fast
path is unchanged.

Cost: a helper call at a deferred site only (a site the interpreter never
ran at compile time), memoised per site after its first resolution.

## Progress (wave 41) — lane L5: carries the i26-L5 field-owner remainder

The page `i26-L5-a-user-loaders-class-under-a-jdk-name-is-resolved-to-the-jdks-class`
was closed by wave 41 (lane L5) into
`docs/internal/fixed-bugs/interpreter-L5-a-user-loaders-class-under-a-jdk-name-is-resolved-to-the-jdks-class-FIXED-20261005.md`:
its resolver half is done, and its last JIT item is this proposal's. The
evidence, as that page's "Progress (wave 38)" traced it (not run as a probe):
the compile doors resolve a field site with the threadless
`field_access::resolve_field_ref`, which for a JDK-global owner (`javax/`,
`jdk/`, `sun/`, `com/sun/`) answers the loader's own record, else the global
class; so a COLD `getstatic javax/…/X.f` in a class of a child-first loader
that defines its own `X` on request compiles against java.base's (or the
application's) field, where HotSpot, and CratonVM's interpreter, ask the
loader at the program point. Deferring the site is the fix; refusing the
method is not (it would keep every hot method with one such cold access
interpreted). A probe for the implementer: `L2W37JdkNameColdSite`'s shape
with a `getstatic` of the loader's own `javax/` class in a cold branch of a
method made hot first.
