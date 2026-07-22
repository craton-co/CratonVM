# `ThymeleafServletAutoConfigurationTests.createLayoutFromConfigClass` hang building a Groovy `MetaClass` during real template rendering — FIXED

**Status: FIXED — 2026-07-21. Three independent bugs closed across three
sessions (2026-07-19, 2026-07-20, 2026-07-21); the full
`ThymeleafServletAutoConfigurationTests` class now passes 27/27, and its
Reactive sibling passes 21/21.**

## Symptom

`createLayoutFromConfigClass` (the one test in
`ThymeleafServletAutoConfigurationTests` that actually renders a Thymeleaf
template through the real `nz.net.ultraq` layout-dialect
`FragmentProcessor`) never completed — the suite runner reported `HANG` at
whatever timeout was configured, in both JIT and `--nojit` mode. It was also
the first test JUnit5 selected to run in this class (no `@TestMethodOrder`
declared), so it blocked every other test in the class from ever running in
the same process.

## Root cause #1 — GC `ReferenceQueue` field collision (FIXED 2026-07-19)

A GC-driven `ReferenceQueue` auto-enqueue used a bare field-count heuristic
(`num_fields > 2 ? 2 : 0`) to find a `Reference`'s `next` (queue-linkage)
slot. `java.util.WeakHashMap$Entry` (a `WeakReference` subclass) declares
its OWN field also named `next` (its hash-bucket chain pointer, an
unrelated linked list) — the heuristic couldn't tell the two apart. A
GC-driven auto-enqueue of a stale `WeakHashMap$Entry` spliced the queue
link over the bucket-chain link, corrupting the bucket; a later
`WeakHashMap.get()` walking it looped forever. This permanently wedged
`com.sun.beans.TypeResolver`'s internal `WeakCache` (used by
`java.beans.Introspector.getBeanInfo()`, itself 100% real JDK bytecode).

Confirmed via `--stack-dump-on-timeout` + `--nojit`: 80 successive dumps
showed the interpreter permanently parked at `WeakHashMap.matchesKey`'s
first bytecode instruction — genuinely frozen, not merely slow.

**Fix**: resolve the `next` slot BY NAME against `java/lang/ref/Reference`'s
own declaring class (`resolve_field_index_in_hierarchy`/
`NativeContext::resolve_field_index`), instead of guessing an index, in
both the GC's own enqueue path (`vm/src/runtime/interpreter.rs`'s
`gc_reference_next_slot`) and the user-code enqueue/poll path
(`native-builtins/src/reference.rs`'s `ref_next_slot`).

## Root cause #2 — non-identity-stable `TypeVariable` for `Class.getTypeParameters()` (FIXED 2026-07-20)

With root cause #1 fixed, the same test still hung, one level deeper: a run
with no watchdog and a 300s hard kill logged **574,867** calls to
`Arrays.hashCode` with no sign of terminating — a genuine unbounded loop,
not merely slow. Tracing (`CRATONVM_TRACE_ARRAYS_HASHCODE=1`, added that
session, left in place permanently) showed the same object, every single
time: CratonVM's synthetic `TypeVariable` stand-in for `java.util.List`'s
own declared type parameter `E`.

`com.sun.beans.TypeResolver.resolve(TypeVariable, Map)` (real JDK bytecode)
detects a type variable that maps to itself via `map.get(tv) == tv` —
**reference** equality. `native_class_get_type_parameters`
(`native-builtins/src/lang_class.rs`, backing `Class.getTypeParameters()`)
built a brand-new synthetic `TypeVariable` object on every call instead of
reusing the cached instance HotSpot's `Class.getGenericInfo()`
soft-reference cache guarantees across repeated calls — so two calls for
"the same" conceptual type variable (same declaring class + name) never
compared `==`, and `TypeResolver`'s self-mapping termination check could
never fire.

This was independently found and fixed (2026-07-20, commit `e426eadde`)
while investigating a *different* doc
(`embedded-tomcat-loopback-self-connect-silent-hang-FIXED.md` — Hibernate
Validator's `TypeHelper.resolveTypes` hit the identical non-identity-`Class.
getTypeParameters()` gap chasing a substitution map across a generic
constraint-validator hierarchy). Two narrower, sibling caching gaps in the
same family (method/constructor-scoped type variables, and the
unresolvable-name fallback arm) were separately found+fixed 2026-07-19 in
`native-builtins/src/generics.rs`'s `cached_building_type_parameter` — see
that file's doc comments for the full identity-caching history. All three
gaps are now closed: `native_class_get_type_parameters` reuses
`cached_building_type_parameter`'s entry when one already exists for
`(declaring class, name)`, only building+caching fresh on first request.

**Verified 2026-07-21**: `createLayoutFromConfigClass` now passes standalone
in 47s (JIT) / 32s (`--nojit`) — confirmed by rerunning it fresh against
current `dev` tip, which already contained `e426eadde`.

## Root cause #3 — a previously-fixed dead-dispatch bug silently regressed (FIXED 2026-07-21)

With both hangs closed, running the **full** `ThymeleafServletAutoConfigurationTests`
class for the first time ever (previously always blocked by root causes #1/#2)
surfaced one failure: `templateLocationEmpty(CapturedOutput, Path)` — the
exact symptom already documented and FIXED on 2026-07-19 in
[`path-tostring-indy-stringconcat-dead-dispatch-FIXED.md`](path-tostring-indy-stringconcat-dead-dispatch-FIXED.md):
`"spring.thymeleaf.prefix:file:" + directory` (`directory` a `Path`)
stringified as `file:java.nio.file.Path@<hash>` instead of the real path,
because `javap -v` confirmed the exact same bytecode shape that doc
describes — `invokestatic java.lang.String.valueOf(Ljava/lang/Object;)`
ahead of the `StringConcatFactory` indy call.

That doc's fix (`vm/src/vm/vm_exec.rs`'s `invoke_on_class_shared_inner`: a
standalone, receiver-aware `toString()` check, independent of the resolved
`class_name`, that dispatches straight to the registered
`java/nio/file/Path.toString()` native when the actual receiver
`is_subclass_of` `Path`) was **entirely missing from `dev` tip** — `git log
-S "let is_path = {"` shows it was added exactly once (`a6ce01fe2`,
2026-07-19) and never explicitly removed by any ordinary commit, yet the
string had zero occurrences in the current file. It was evidently dropped
silently during one of the many merges into `dev` between then and now (not
pinned to an exact commit — pickaxe with `-m` across the full first-parent
history didn't surface a removing diff, suggesting a merge-conflict
resolution that never carried the hunk forward). This is exactly why that
doc's own "Verification" section could only re-run
`ThymeleafReactiveAutoConfigurationTests`, not
`ThymeleafServletAutoConfigurationTests`'s copy of the same test — the
latter was still blocked by root causes #1/#2 at the time, so the regression
had no test coverage to catch it.

Two sibling fixes from the SAME original commit were independently
confirmed still present and unaffected: the `interpreter.rs`
`force_native_over_real_jdk_bytecode`-style gate keyed on
`class_name == "java/nio/file/Path"` (handles direct-interface-typed call
sites), and `invokedynamic.rs`'s `value_to_string` receiver-aware check
(handles a `Path` passed directly into the indy bootstrap with no
`String.valueOf` pre-conversion). Only the `vm_exec.rs` hunk — covering the
`String.valueOf(Object)`-internal-dispatch shape specifically — was gone.

**Fix**: re-added the identical hunk to `invoke_on_class_shared_inner`, in
the same location (immediately after `class_name` is resolved). Confirmed
safe against the unrelated `invoke_on_class_shared` caller-side dispatch gate
that a *later* fix (`886a3627f`, 2026-07-20) deliberately narrowed to avoid
a `BackgroundPreinitializingApplicationListener`/Hibernate-Validator
deadlock — that commit only touches the `NativeContextImpl` call site
(`vm_exec.rs` ~L8749), a different function from `invoke_on_class_shared_inner`
itself, and the re-added check adds no new locking.

## Verification

Worktree `fix/thymeleaf-metaclass-hang-20260721` (`dev` @ `fb92275dd`),
binary `cratonvm-thymeleaf-metaclass-hang-20260721.exe`.

- `createLayoutFromConfigClass` standalone: PASS in 47.2s (JIT), PASS in
  32.4s (`--nojit`) — both previously HANG at every timeout tried.
- Full `ThymeleafServletAutoConfigurationTests` class: **27/27 PASS**
  (152s) — previously never ran to completion at all.
- `ThymeleafReactiveAutoConfigurationTests`: **21/21 PASS** — unchanged
  from the 2026-07-19 verification, confirms no regression.
- `RemappedErrorViewIntegrationTests` (the regression test for `886a3627f`,
  the caller-side dispatch-gate narrowing): **2/2 PASS** in 22.0s —
  confirms the re-added `vm_exec.rs` hunk does not reintroduce that
  deadlock.
- `cargo test -p cratonvm-native-builtins --lib`: 3050 passed, 1 failed
  (`zip_real::tests::direct_buffer_deflate_paths_throw_instead_of_zero_progress`),
  6 ignored — the 1 failure is pre-existing/unrelated (confirmed identical
  on unmodified `dev` tip; this session's only change is in
  `vm/src/vm/vm_exec.rs`, nowhere near `zip_real.rs`).
- `cargo test -p cratonvm-vm --lib` (debug profile — release-profile
  `lock_order`/`jni` tests give false failures since they assert
  debug-only behavior, see below): 2237 passed, 7 failed, 111 ignored —
  all 7 failures are in `jit::skip_list::tests` and confirmed
  byte-identical on unmodified `dev` tip (pre-existing, unrelated to this
  fix; flagged separately, not investigated further here).

## Lesson: `--release` gives false negatives for some `cratonvm-vm` unit tests

`runtime::lock_order::tests::*` and `native::jni::tests::jni_function_table_extended_to_234`/
`jni_nio_slots_not_stub` fail under `cargo test --release` (debug-assertion
gated behavior compiled out) but pass under the default debug profile. Use
the default profile for `cratonvm-vm`'s test suite; only use `--release`
when specifically testing release-mode behavior.

## Affected classes (now passing)

- `module/spring-boot-thymeleaf` | `ThymeleafServletAutoConfigurationTests` | all 27 methods, including `createLayoutFromConfigClass()` and `templateLocationEmpty(CapturedOutput, Path)`
