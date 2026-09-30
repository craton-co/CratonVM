# §5.4.5 overriding by run-time package: what the wave-6 fix leaves open

Status: OPEN
Area: class linking / virtual selection (`classloading/src/method_override.rs`,
`classloading/src/class_manager.rs` vtable build,
`vm/src/runtime/interpreter/dispatch_virtual.rs` vtable fast path)
Severity: LOW (policy residuals; no known application affected)
Found by: round 12 wave 6 lane override

The wave-6 fix of `r12w4-mega3-override-by-package-name-ignores-the-loader-FIXED-20260926.md`
makes a strict (`--jdk-only`) VM compare the defining loader when deciding
whether a method overrides a package-private one. Three things are left, each
deliberately.

## 1. `--compatible` keeps the name-only rule (owner's decision)

`ClassManager::set_compatibility_mode` turns the loader-aware rule on only for
`CompatibilityMode::JdkOnly`. AGENTS.md requires `--compatible` to stay
byte-for-byte unchanged, so `R12Hunt3PkgPrivate` still prints `bad 96000` there
(and with `CRATONVM_OVERRIDE_PACKAGE_BY_LOADER=0`). Every embedding entry point
(`VmConfig::default`, `libcratonvm`, `cratonvm-embed`) defaults to
`Compatible`, so an embedder sees the old answer too.

To change it: in `set_compatibility_mode`, compute `by_loader` without the
`mode.is_jdk_only()` term. Nothing else is policy-dependent (the vtable build,
selection and the fast-path guard all read the store flag).

## 2. The three built-in loaders are not told apart, even in strict mode

`same_loader_for_override` answers "same loader" for any two of `Bootstrap`,
`Extension`, `Application`. Reason: the native boundary does not preserve them.
`native-builtins/src/lookup_define.rs::inherit_lookup_loader` maps a lookup
class's loader id `< 3` (bootstrap, platform, app) to `0`, which
`ClassLoaderId::from_native_id_or_default` decodes as `Application`; so a class
defined through `Lookup.defineClass` / `defineHiddenClass` against a
`java.lang.invoke` lookup (a `BoundMethodHandle$Species_*`, which overrides the
package-private `BoundMethodHandle.copyWith` / `speciesData`) is recorded as
`Application` while its superclass is `Bootstrap`. Comparing them would make
those overrides stop overriding (an `AbstractMethodError` or the wrong body).

What stays wrong: a class-path class in the same package NAME as a platform or
bootstrap class does not form a separate runtime package for overriding. JPMS
forbids that split for named modules, so only unnamed-module split packages
between the platform and application loaders are affected.

To close: make `inherit_lookup_loader` return the lookup class's real built-in
id (`ClassLoaderId::to_native_id` of the lookup class's `loader_id`, not the
`< 3 → 0` collapse), audit the other `define_class_full` callers for the same
collapse (`classloader.rs` hidden define, `unsafe_natives.rs`, JNI
`DefineClass` after lane jni's wave-6 change), then drop the built-in arm of
`same_loader_for_override`. Confirm with a probe that spins a
`BoundMethodHandle` species at run time (`MethodHandles.insertArguments` binding
a mix of argument kinds the image does not pregenerate a species for) under
`--jdk-only`.

## 3. The interpreter fast-path guard is per class, not per signature

`ClassManager::vtable_shadows_a_signature(receiver)` is true when the
receiver's vtable holds ANY signature twice; every virtual call on such a
receiver then pays the §5.4.6 validation arm of `dispatch_virtual.rs` Step 4
(a class-manager read and a memoized `select`), not only calls of the
duplicated signature. Such receivers are rare (a child loader re-defining a
class of its parent's package that re-declares a package-private method), so
the cost was not worth a new `Vtable` accessor in `vm/src/runtime/vtable.rs`
(lane mega5's file this wave).

To narrow it: add `Vtable::signature_is_shadowed(name, desc) -> bool` (the
`fast_lookup` bucket holds two verified slots) and use it in Step 4 in place
of the per-class bit, or record the duplicated `(name, desc)` pairs instead
of the class id.

## How to confirm

`R12Hunt3PkgPrivate` and `R12OverrideSelfCall` (`C:\craton\jitr12-probes\src`):
`bad 0` in the default (jdk-only) arm, `bad` non-zero under `--compatible`
(item 1). Unit tests: `classloading/src/class_manager.rs`
`strict_override_policy_keeps_the_overrides_it_cannot_disprove` pins item 2
(`r/Species` under a `Bootstrap` `r/Base` still overrides).

## Round 12 wave 7 (lane compat)

Re-read with the orchestrator's `28d6d768f` in place: `set_compatibility_mode`
now installs the loader-aware rule in both modes (unless
`CRATONVM_OVERRIDE_PACKAGE_BY_LOADER=0`), and `vm_init.rs:2203` calls it for
every VM, embedders' `Compatible` default included.

**Item 1 is closed** by that commit: `R12Hunt3PkgPrivate` and
`R12OverrideSelfCall` should print `bad 0` under `--compatible` too.

**Is there a real override that now stops overriding under `--compatible`?**
The rule can only turn an override off when the two classes carry DIFFERENT
recorded loaders and at least one is `UserDefined`. I read every define path
that picks the recorded loader for a class that could override a
package-private method of a same-package class:

- `lookup_define.rs` (`Lookup.defineClass` / `defineHiddenClass`, the winning
  registration in both modes): inherits the lookup class's loader
  (`inherit_lookup_loader`); a built-in lookup collapses to `Application`,
  which item 2 already treats as the same loader.
- `classloader.rs` `lk_define_class` / `lk_define_hidden_class` (define with
  loader `0`): never dispatched; `register_lookup_define_class` overwrites
  both triples (the comment at ~12102 says so, and it is right).
- `cglib_enhancer.rs` (the `--compatible` CGLIB `@Configuration` enhancer):
  defines into the superclass's loader, or keeps it there whenever the
  superclass relies on package visibility (`resolve_define_loader` (3a));
  `enhanceFactoryBeanReference` uses the concrete class's loader.
  `spring_startup_bootstrap.rs` uses `super_loader_id`.
- `unsafe_natives.rs` `defineAnonymousClass` (loader `0`): unreachable on JDK
  25 (the method is gone from both `Unsafe`s since JDK 17).
- JNI `DefineClass`: through the caller's loader since the same commit.

None of these records a subclass under a different loader from a same-package
superclass it could override. No HIGH finding.

**New item 4 (MEDIUM, not demonstrated).** `resolve_class_loader_aware`
(`vm/src/runtime/interpreter/constants.rs`, the `user_loader` arm) records
that for forked loaders (`@CompileWithForkedClassLoader`, Groovy) the class
manager's `loader_id` for a class can disagree with the
`defining_loader_for` side table and read `Application`. If one class of a
package is recorded `UserDefined(n)` and a same-package class the same fork
defined is recorded `Application`, the loader-aware rule now calls them
different runtime packages in `--compatible` as well as `--jdk-only`, and a
package-private override between them stops overriding (the subclass's
method gets a fresh vtable slot; calls through the superclass run the
superclass's body). The classloading crate cannot consult the side table
(it lives in `native-builtins`). To close: make every define site that
consults `defining_loader_for` also stamp the class's `loader_id`
(`ClassStore::get_mut(..).loader_id`), or have `same_loader_for_override`
answer "same" for an `Application`/`UserDefined` pair when the store records
the user loader as a fork of the application loader. Confirm with a Spring
`@CompileWithForkedClassLoader` test whose forked `@Configuration` class has
package-private `@Bean` methods (CGLIB overrides them), under both modes, and
`CRATONVM_OVERRIDE_PACKAGE_BY_LOADER=0` as the control.

Items 2 and 3 are unchanged. Status stays OPEN (items 2-4).

## Round 13 wave 1 (lane mhffm)

Items 2 and 3 are in files this lane did not own (`lookup_define.rs`, `vm/src/runtime/vtable.rs`,
`dispatch_virtual.rs`); unchanged. Item 4 is still undemonstrated: the reading path it names
(`resolve_class_loader_aware`'s `user_loader` arm) records the loader for RESOLUTION, and no define
site read this wave stamps a class `Application` while `defining_loader_for` names a fork for a
same-package class of the same fork; changing `same_loader_for_override` without a reproducer would
risk turning real overrides off. Related, landed this wave: the reflective door
(`invoke_virtual_declared`, i.e. every `findVirtual` handle) now selects with the same
`can_override` rule the vtable build and the interpreter use
(`r12w8-hunter5-mh-and-reflection-virtual-dispatch-ignore-the-override-rule-FIXED-20260928.md`), so the
three doors can no longer disagree about a user-loader override; `selection::tests` gained the
transitive user-loader shape. Status stays OPEN (items 2-4).

## Round 13 wave 3 (lane ffm2)

`classloading/src/method_override.rs` (this lane's file) re-read against JVMS 25 §5.4.5: the
non-private / non-static test, the public-or-protected arm, the run-time package arm
(`same_runtime_package_for_override`, loader-aware in both modes unless
`CRATONVM_OVERRIDE_PACKAGE_BY_LOADER=0`) and the transitive clause (each `mB` strictly between C
and A, both halves recursive, budget exhaustion answering "overrides") match the rule; no defect
found. Items 2 (`lookup_define.rs` `inherit_lookup_loader` collapse) and 3 (per-class guard in
`dispatch_virtual.rs` / `vm/src/runtime/vtable.rs`) are in files this lane does not own and are
unchanged; item 4 is still undemonstrated (no define site read records a same-fork class as
`Application`). The reflective doors now select by the same rule (hunter5 page, FIXED pending
verification), so no door disagrees with the vtable build any more.

Status stays OPEN (items 2-4).

## Round 13 wave 5 (lane proxy3)

**Item 3 landed.** `ClassManager::vtable_shadows_signature(class, name, descriptor)`
(`classloading/src/class_manager.rs`, next to `vtable_shadows_a_signature`) answers whether the
receiver's recorded vtable descriptors hold THIS signature in more than one slot; the interpreter's
vtable fast path (`dispatch_virtual.rs`, `receiver_vtable_shadows`) asks it instead of the
per-class bit, so only the duplicated signature pays the §5.4.6 validation. It still answers
`true` for a class in the shadow set whose descriptors are not recorded, and the per-class set is
still the first filter (one hash probe, empty under `CRATONVM_OVERRIDE_PACKAGE_BY_LOADER=0`). No
`Vtable` accessor in `vm/src/runtime/vtable.rs` was needed: the class manager's own descriptors
are what the VM vtable is built from. Switch `CRATONVM_OVERRIDE_SHADOW_PER_SIGNATURE` (default
on; `0` keeps the per-class answer), read once per `ClassManager`. The existing unit test
`vtable_package_private_root_split_by_loader_follows_the_policy` now also pins that `p/C`'s `m`
is shadowed and its `other` is not.

Items 2 and 4 are unchanged: item 2 needs `lookup_define.rs` (`inherit_lookup_loader`'s `< 3`
collapse) and an audit of every `define_class_full` caller before `same_loader_for_override` can
drop its built-in arm, and item 4 still has no reproducer. `method_override.rs` re-read: no
defect. Status stays OPEN (items 2 and 4).

## Round 13 wave 6 (lane proxy4)

No code change; both items re-read, with a narrower statement of each.

**Item 2 is smaller than written.** Since JDK 9 a package that belongs to a named module is
loaded only by that module's loader (`BuiltinClassLoader.loadClassOrNull` maps the package to its
module first), so a class-path class can never share a package name with a bootstrap or platform
MODULE class: the application loader would never load it. The only way two built-in loaders
define one package name is an unnamed-module split between `-Xbootclasspath/a:` (bootstrap,
unnamed) and the class path (application, unnamed) -- the application loader delegates the
unmodularised package to its parents first, so both copies load, as two runtime packages. That is
the whole population the built-in arm of `same_loader_for_override` gets wrong, and it is a
configuration no known application uses. Against it stands the arm's reason to exist
(`inherit_lookup_loader`'s `< 3 -> 0` collapse records a `BoundMethodHandle$Species_*` as
`Application` under a `Bootstrap` super; dropping the arm first would stop those overrides). The
closing plan above stays correct and stays in the owner's order: `lookup_define.rs` first, the
`define_class_full` caller audit, then `method_override.rs`. Recommendation: close item 2 as
not worth the risk unless an `-Xbootclasspath/a` split-package report arrives; a probe needs a
second class-path root, which the single-file battery cannot build.

**Item 4 still has no reproducer.** Read the three sites that record a defining loader for a class
the store already holds (`lang_system.rs` `DuplicateDefine::ServeExisting` for an adopted orphan and
for a namespace collision, `classloader.rs` `cl_define_class_basic` after `define_class_full`): each
registers the side-table loader for a class found in, or defined into, the SAME namespace id the
registering loader maps to, so the class manager's `loader_id` and the side table name the same
runtime package for every class one fork defines. The shape the item fears (one class of a fork
recorded `UserDefined(n)`, a same-package class of the same fork recorded `Application`) needs a
define path that stamps a namespace other than the loader's own; none found. Status stays OPEN
(items 2 and 4) for the orchestrator's decision on item 2.

## Round 13 wave 8 (lane proxy5)

No code change; none of the files involved (`lookup_define.rs`, `method_override.rs`,
`class_manager.rs`) is this lane's. Re-read at the current tree: `inherit_lookup_loader`
(`lookup_define.rs:204`) still collapses a built-in lookup class to `0` (Application) and
`same_loader_for_override` (`method_override.rs:98`) still answers "same" for any two built-ins, so
item 2 is exactly as wave 6 narrowed it (only an `-Xbootclasspath/a` / class-path split package is
affected). Item 4 still has no define path that stamps a fork's class `Application`. Recommendation
for the orchestrator, unchanged from wave 6: close item 2 as an accepted gap (move it to
`docs/known-issues/` with its closing plan) unless a split-package report arrives, and close item 4
after a Spring `@CompileWithForkedClassLoader` run with `CRATONVM_OVERRIDE_PACKAGE_BY_LOADER=0` as the
control shows no difference. Status stays OPEN (items 2 and 4).

## Round 13 wave 9 (lane proxy6)

All four files of this page are this lane's now; re-read with that in mind. No wrong-dispatch case
found, so no dispatch code changed (one stale comment in `dispatch_virtual.rs` Step 4, which still
said the shadow set is "never under `--compatible`", now names the both-modes policy and the
per-signature accessor).

**Item 2 cannot occur in CratonVM today, and closing it needs an API change, not a policy
change.** The one population it affects (a class-path class and a bootstrap class sharing an
unnamed-module package, i.e. `-Xbootclasspath/a:`) is not modelled: `vm-cli`'s
`normalize_java_launcher_argv` rewrites `-Xbootclasspath/a:<p>` (and `/p:`) to
`--Xbootclasspath <p>`, which REPLACES the boot class path (`VmConfig::with_boot_classpath`;
`vm_init.rs` ~2082 then skips image discovery), so no class-path/bootstrap split package can be
built. That launcher behaviour is its own defect, filed as
`r13w9-proxy6-xbootclasspath-append-replaces-the-boot-image-FIXED-20260928.md`. When it is fixed, item 2
becomes reachable again, and the fix is still not in `method_override.rs`: `inherit_lookup_loader`
(`lookup_define.rs:204`) cannot return Bootstrap, because `define_class_full` decodes loader `0` as
"default = Application" (`ClassLoaderId::from_native_id_or_default`), and Bootstrap's native id IS
`0`. Only Extension (`1`) is expressible, and platform-vs-application overriding is not observable
(no class-path class can share a package with a platform module). Recommendation unchanged: move
item 2 to `docs/known-issues/` as an accepted gap once the launcher page lands.

**Item 4: new HotSpot-valid probe, and the stale premise behind it is gone.** The comment at
`lookup_define.rs` ~217 says the legacy `loader_id_of_class` path collapses a user loader with
namespace id 1 or 2 onto Application; that is no longer true -- `to_native_id` keeps user ids `>= 3`
(`NATIVE_FIRST_USER_DEFINED`) and `raw < 3` is exactly the built-ins, so a `Lookup.defineClass` /
`defineHiddenClass` against a fork's class lands in the fork's `UserDefined(n)`, the same id the
fork's own `defineClass1` records. `C:\craton\jitr13-probes\src\R13Proxy6ForkOverride.java` pins the
three define doors of one fork (a child-first loader defining Base and Sub, `Lookup.defineClass` and
`Lookup.defineHiddenClass` subclasses of the fork's Base) plus the cross-loader control, with the
dispatch inside each Base copy's compiled `callM`. Expected HotSpot `bad 0`; if CratonVM prints
`bad 0` in the default, `--nojit` and `--compatible` arms, close item 4
(`CRATONVM_OVERRIDE_PACKAGE_BY_LOADER=0` must turn the control row wrong: it proves the arm is live).

Status stays OPEN for the orchestrator's two decisions (item 2 to `docs/known-issues/`, item 4 on the
probe result).

## Round 13 wave 11 (lane misc11)

No change to the override rule (`method_override.rs` and `lookup_define.rs` are not this lane's).

**Item 4: the probe settles it.** `R13Proxy6ForkOverride` is `OK` (= the HotSpot reference,
`bad 0`) in all eight wave-10 arms the orchestrator ran (`res-w10f-{def,compat,thr1,osr0,nle0,g1,c2never,c2always}.txt`
in `C:\craton\jitr13-probes`), `--compatible` included, so every define door of one fork records one
runtime package. The one run still owed is the control, `CRATONVM_OVERRIDE_PACKAGE_BY_LOADER=0`,
whose cross-loader row must go wrong (it proves the loader arm is what makes the other rows
right). With that, item 4 is closed.

**Item 2 is now reachable, through two doors.** Wave 9's "cannot occur" rested on
`-Xbootclasspath/a:` replacing the boot path. Since wave 10 it appends
(`vm_init.rs`, `CRATONVM_XBOOTCLASSPATH_APPEND`), and since this wave an agent's `Boot-Class-Path`
does too (`agent_loader.rs`, `CRATONVM_AGENT_BOOT_CLASS_PATH_BOOTSTRAP`): a class from either is
`ClassLoaderId::Bootstrap`, and a class-path class in the same (unnamed-module) package is
`Application`, which `same_loader_for_override` still calls the same loader. So a class-path
`p.Sub` now overrides a package-private `m` of an appended `p.Base` where HotSpot says it does
not (two runtime packages). Still no known application: it takes a split package across the
appended jar and the class path plus a package-private override across the split.

The narrowest closure found, for whoever owns `method_override.rs`: keep the built-in arm, except
answer "different" for a `(Bootstrap, Application|Extension)` pair whose Bootstrap side is in the
appended-jar census (`cratonvm_classloading::is_bootstrap_appended_class(vm, name)`) and whose
other side is not. It does not touch the `BoundMethodHandle` species (their bootstrap supers are
image classes, never appended). Its one wrong answer is a `Lookup.defineClass` against a lookup
class that is itself appended (recorded `Application` by `inherit_lookup_loader`'s collapse)
overriding a package-private method of an appended super -- HotSpot: same package. Two
obstacles: `ClassStore` does not know its VM's identity (the census key is the class manager's
`vm_id`), and `ClassOrigin` cannot stand in for the census (it is derived from `loader_id` alone,
so a `Lookup`-defined class is also `ApplicationClassPath`). A probe needs an appended jar, which
the single-file battery cannot build. Recommendation unchanged: accept item 2 as a gap unless a
split-package report arrives.

Status stays OPEN: item 2 (orchestrator decision), item 4 (the control run).

## Round 14 wave 2 (lane trace)

No code change. Re-read `classloading/src/method_override.rs` (`same_loader_for_override`,
`same_runtime_package_for_override`, `can_override`) against JVMS 25 §5.4.5: unchanged since wave
11 and correct for every recorded-loader pair except the item-2 population. The narrowest closure
(wave 11) still needs the appended-jar census reachable from `ClassStore`, whose key is the class
manager's `vm_id`: that means a per-store field set by `ClassManager` at construction (a
`classloading/src/class_manager.rs` change, a file that is also the interpreter round's
resolution path's neighbour) for a population no application is known to hit. Recommendation
unchanged: move item 2 to `docs/known-issues/` as an accepted gap; close item 4 after the one
control run (`R13Proxy6ForkOverride` with `CRATONVM_OVERRIDE_PACKAGE_BY_LOADER=0`, whose
cross-loader row must go wrong). Status stays OPEN for those two orchestrator actions.

## Round 14 wave 3 (lane compat3)

No code change (`classloading/src/method_override.rs`, `lookup_define.rs` and `class_manager.rs`
are not this lane's; `vm/src/runtime/interpreter/native_override.rs` -- the "override regions" this
lane was given -- is the native-over-bytecode dispatch policy, not JVMS §5.4.5 method overriding,
and holds nothing for this page). Re-checked the one run item 4 has been waiting for since wave 11: it was never
made. `C:\craton\jitr14-probes\out\*\R13Proxy6ForkOverride.norm` exists only for the ordinary
arms (base/w2a), all `OK`; no arm set `CRATONVM_OVERRIDE_PACKAGE_BY_LOADER=0`.

**Orchestrator, two actions (no lane can take either):**

1. Run `R13Proxy6ForkOverride` once with `CRATONVM_OVERRIDE_PACKAGE_BY_LOADER=0` (default mode).
   Its cross-loader control row must go wrong (`bad` > 0); that proves the loader arm is what makes
   the other rows right, and item 4 closes on the existing eight `OK` arms.
2. Decide item 2: move it to `docs/known-issues/` as an accepted gap (a split package across an
   `-Xbootclasspath/a:` / agent `Boot-Class-Path` jar and the class path, with a package-private
   override across the split; no known application), carrying wave 11's narrowest closure. The
   single-file probe battery cannot build the reproducer.

Status stays OPEN for those two actions.

## Round 14 wave 4 (lane compat4)

No code change (the §5.4.5 files -- `classloading/src/method_override.rs`, `lookup_define.rs`,
`class_manager.rs` -- are not this lane's; `native_override.rs` is the native-over-bytecode gate,
not method overriding). Re-checked the evidence: `C:\craton\jitr14-probes\out\*\R13Proxy6ForkOverride.norm`
now exists for 27 arms (base, w1a/w1b, w2a, w3a/w3b, the `oi-*` interpreter arms, `--compatible`
included), every one `bad 0`; still none with `CRATONVM_OVERRIDE_PACKAGE_BY_LOADER=0`. The two
orchestrator actions of wave 3 stand unchanged: (1) one control run of `R13Proxy6ForkOverride`
with `CRATONVM_OVERRIDE_PACKAGE_BY_LOADER=0` (its cross-loader row must go wrong), then close item
4; (2) decide item 2 (accepted gap, `docs/known-issues/`, carrying wave 11's narrowest closure).
Status stays OPEN for those two actions.

## Round 14 wave 6 (lane compat6)

No code change; read only. `classloading/src/method_override.rs` (`same_loader_for_override`,
`same_runtime_package_for_override`, `can_override`) and `native-builtins/src/lookup_define.rs`
(`inherit_lookup_loader`'s `< 3 -> 0` collapse) are unchanged since wave 11 and not this lane's files;
`vm/src/runtime/native_override.rs` override regions (this lane's) are the native-over-bytecode gate,
not JVMS §5.4.5 overriding, and hold nothing for this page. The two orchestrator actions recorded
since wave 3 still stand and still cannot be taken by a lane: (1) one control run of
`R13Proxy6ForkOverride` with `CRATONVM_OVERRIDE_PACKAGE_BY_LOADER=0` (its cross-loader row must go
wrong; then item 4 closes on the existing `bad 0` arms); (2) move item 2 to `docs/known-issues/` as an
accepted gap (split package across an `-Xbootclasspath/a:` / agent `Boot-Class-Path` jar and the class
path with a package-private override across it; wave 11's narrowest closure carried). Status stays
OPEN for those two actions.
