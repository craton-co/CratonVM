# JIT round 21 wave 18: adversarial review of waves 16-17 (lane review-w18)

Status: OPEN. There are 8 items. Item 1 is low-medium (it can resume a frame with the wrong class's body). Items
2-7 are low; item 6 is a performance item and item 7 needs a HotSpot run to confirm. Item 8 lists the areas that were
reviewed and found clean. Nothing was fixed in place, because this lane may not edit `.rs` files.

Scope: `git diff a7480774ca 30635cad5e -- '*.rs'` (waves 16-17), read at HEAD 30635cad5e. This was a review by
reading only: nothing was built or run. Line numbers are HEAD's. A wave-18 lane (finalize-reinit) was editing
`gc/src/reference.rs`, `exception_dispatch.rs`, `vm_init.rs` and `jit_realm.rs` in the working tree during this
review. Its uncommitted change already handles the GC side of `withdrawn_finalizables`: the rows are no longer
watched and are dropped at every collection. This page does not repeat that change, and agrees with it.

## Item status

| Item | Severity | Verdict |
|---|---|---|
| 1 a deopt-resume scope with no class-id row takes the name or self answer before the constant-pool evidence | Low-medium (wrong-class resume, multi-loader) | FIXED (wave 19, lane deopt-resume; pending orchestrator build): `vm/src/runtime/interpreter/deopt_resume.rs:865` `resolve_unhinted_scope_class` always runs the CP scan, refuses a disagreement; `:805` `supertype_named` refuses two matches; same switch `CRATONVM_DEOPT_RESUME_SCOPE_BY_LOADER` |
| 2 the `Lookup.in` walk and the `getEnclosingClass` bridge search past the first self row | Low (both modes for `in()`; widens access) | FIXED (wave 19, lane innerclasses; pending orchestrator build): `classloader.rs:17471` and `lang_reflect.rs:614` call `lang_class::member_row_outer_of` (`CRATONVM_DECLARING_CLASS_ANY_OUTER_ROW`); pin `r21w19_member_rule_has_one_copy`; probe `R21Fz6InnerRows1` (X$6) |
| 3 compatible-mode nesting answers for a row without `EnclosingMethod`, and for `EnclosingMethod` without a row | Low (`--compatible`, pre-existing, left by wave 16) | FIXED (wave 19, lane innerclasses; pending orchestrator build): `lang_class.rs:37076` `class_nesting_of` decides the predicates, `resolve_simple_name` and `canonical_class_name` (`CRATONVM_COMPAT_NESTING_FOLLOWS_ENCLOSING_METHOD`, default ON); probe `R21Fz6InnerRows1` (X$2, X$4, X$5, X$6) |
| 4 the redefinition forget races with a reflection computation that read the old tables | Low (needs concurrent reflection during RedefineClasses) | FIXED (wave 19, lane innerclasses; pending orchestrator build): `lang_class.rs:38526` `InnerTablesFill` (`CRATONVM_REFLECT_INNER_TABLES_GENERATION`, default ON), bumped first in `forget_redefined_inner_class_answers` (:38648) |
| 5 the `<init>`-return hook and the unwinder take the receiver from local 0 | Low (hand-built bytecode or JNI) | FIXED (wave 19, lane finalize-reinit; pending orchestrator build): the hook refuses a constructor body that can write local 0, `vm/src/runtime/interpreter/exception_dispatch.rs:420` `ctor_return_names_its_initialized_receiver` (called from `:376`). No `Frame` field keeps the entry receiver (`frame.rs` is outside the lane). The unwinder variant (b) was already refused: `unconstructed_mark_of_ctor` rests on `ctor_prefix_scan`, which returns `None` for every `xstore 0` / `iinc 0` / `wide` form / `jsr` / `ret`, so no mark is taken. Tests `:6319` (tail: an `aload_1; astore_0` body neither re-registers nor marks), `:6443`; the scan-level pin is `a_write_of_local_0_or_a_subroutine_refuses` |
| 6 the `0xb1` hook costs a lock, a code compare and a scan on every finalizable `<init>` return, and its kill switch does not remove the cost | Low (perf) | FIXED (wave 19; pending build). The switch is now `VmConfig::finalizer_reregister_at_init_return` (`vm/src/config.rs:2958`). It is ANDed into `DG_FINALIZER_INIT_RETURN` (`interpreter.rs:10311`), so `=0` costs one bit test per return, and it is handed to the reference processor in `SharedVm::new` (`vm_init.rs:7612`). Past the bit, a finalizable receiver now costs the lock-free has-finalizer recipe bit, one lock-free decode of the frame's own body with every `invokespecial` counted as initializing (no class-manager lock, `find_method`, code compare or pool read), then the `ref_processor` mutex. A verified-class shortcut would need a lock-free per-class "verified" bit, which no structure in the lane's files has; the decode stays because item 5 needs it for verified code too. Tests `:6319` (switch agreement), `:6480` (source pin) |
| 7 does a failed outer-class load in `in()` / `getDeclaringClass0` record the constant-pool resolution error? | Low (verify) | FIXED by reading (not cached before; wave 19, lane innerclasses; pending orchestrator build): `lang_class.rs:39268` / `:39298` (`CRATONVM_OUTER_RESOLUTION_FAILURE_STICKY`, default ON); probe `R21Fz6LookupInLoad1` (`asks(Gone)`). The w25 run still read 2 / 4 / 1254: the loader-aware step `declaring_class_loader_aware` asked the loader with a bare `loadClass` before the recorded door, on every `getDeclaringClass0`. FIXED (wave 26, lane reserr; pending orchestrator build): that step and the slow path's load go through / respect the recorded door (`CRATONVM_DECLARING_LOADER_AWARE_VIA_ENTRY`, default ON) |
| 8 areas reviewed and found clean (rooting, marker, opt-in codegen, empty-finalize sites, ancestor walk) | Info | no action, except the notes in 8. The empty-finalize stub-upgrade gap is FIXED: the class-manager half in wave 22, the VM half (finalizer listener, recipe drop) in wave 23, lane finvm; both pending orchestrator build |

Probes, written without being run. They follow the conventions of `r21-fuzz5-probes-20261019.md`: default package,
ASCII, deterministic output, and the HotSpot expectation is written in each header.

| Probe | Targets | Verdict |
|---|---|---|
| `tools/probes/jit/r21/fuzz/R21Fz6EmptyFinalize1.java` | `CRATONVM_FINALIZER_EMPTY_IS_NONE`, interpreted and compiled `new` (phantom-reference GC count) | OPEN (run pending) |
| `tools/probes/jit/r21/fuzz/R21Fz6LookupInLoad1.java` | `CRATONVM_LOOKUP_IN_LOADS_OUTER`: NCDFE, load, spoof ICCE, no-PRIVATE control, rounds under GC; item 7 | w25: differs in all modes on `asks(Gone)` only (2, 4, 1254; HotSpot 1, 1, 1). Fix in wave 26, lane reserr (pending orchestrator build): expected `asks(Gone)=1` on all three rows |
| `tools/probes/jit/r21/fuzz/R21Fz6InnerRows1.java` | `CRATONVM_DECLARING_CLASS_ANY_OUTER_ROW` and items 2-3 (java.lang.classfile shapes) | OPEN (run pending; expected to show items 2 and 3 on `--compatible`) |

## 1. A deopt-resume scope with no class-id row takes the name or self answer before the constant-pool evidence (Low-medium)

- **Where:** `vm/src/runtime/interpreter/deopt_resume.rs:836` `resolve_unhinted_scope_class`. Step 1 is at
  `:842-849` and step 2 is at `:852`; both `return` before step 3, `scope_class_through_member_refs` at `:869`. The
  helper `supertype_named` is at `:796`.
- **Scenario:**
  - Outer (loader L1) calls `K3.d1()`. `K3` is from loader L2 and extends L2's `p/A`, which declares `d1`. The
    compile spliced L2's `A.d1`.
  - L1 also defines its own `p/A`, with a `d1()I` of the same descriptor. This is legal: loader constraints cover
    descriptor types, not the declaring class of a resolved method.
  - A single-pass chain with no `splice_scope_class_ids` row deopts inside the splice.
  - Step 2 (`find_class_by_name_for_class("p/A", outer)`) answers L1's `A`. The resume interprets L1's `A.d1`, whose
    body is not the one the compiled frame was running.
  - Step 3 would have found L2's `A` through `K3`'s supertypes, and its two-answer refusal would have caught the
    conflict, but step 3 is never reached.
  - Step 1 has the same shape when the key names the enclosing class's own name but the body came from a same-named
    supertype of a constant-pool class.
- **Second, smaller gap:** `supertype_named` returns the first depth-first match. A hierarchy that reaches two
  different classes of the key's name (a superclass from one loader and a superinterface from another; HotSpot's
  loader constraints do not forbid this) is answered with no ambiguity check.
- **Both modes.** The precondition was there before: the name rule alone had the same answer. Wave 16 presents the
  step-3 refusal as the safety net, and it does not cover steps 1-2.
- **Fix sketch:**
  - Run the step-3 scan whenever the enclosing pool has a matching `Methodref` / `InterfaceMethodref`.
  - Treat steps 1-2 as one more candidate: when the scan's answer differs from theirs, refuse.
  - When the scan has no answer, keep the step 1-2 answer.
  - Make `supertype_named` collect every match and refuse when there are two distinct ids.
  - The cost is a constant-pool scan per unhinted scope, which is already a cold path.

**Verdict (wave 19, lane deopt-resume): FIXED (pending orchestrator build).** It uses the same switch,
`CRATONVM_DEOPT_RESUME_SCOPE_BY_LOADER`, default ON. No new switch.
- `resolve_unhinted_scope_class` (`vm/src/runtime/interpreter/deopt_resume.rs:865`) works out the step 1-2 answer
  first: the enclosing class by id, or else the name from the enclosing loader. It then always runs
  `scope_class_through_member_refs` (`:904`).
  - If both answer and the answers differ, the scope is refused (fail closed).
  - If only one of them answers, that answer is used.
  - An ambiguity found inside the scan still refuses.
- `supertype_named` (`:805`) now walks the whole closure. When it meets two distinct ids of the key's name, it
  returns `Err`, which refuses the scope.
- `=0` keeps the name rule alone, unchanged.
- Tests, in `r21w16_scope_by_loader_tests`:
  - `:18426` `a_same_named_class_in_the_enclosing_loader_does_not_beat_the_cp_evidence`: the reviewer's
    scenario. It is refused by default; under `=0` it gives the enclosing loader's `A`.
  - `:18502` `agreeing_name_and_cp_answers_resolve`
  - `:18548` `a_supertype_walk_that_meets_two_classes_of_the_name_refuses`: two superinterfaces from two loaders
    reach two interfaces named `T`.
  - The test class builder gained access flags and interfaces (`class_bytes_full`, `interface_bytes`).

## 2. The `Lookup.in` walk and the `getEnclosingClass` bridge search past the first self row (Low, widens access)

- **Where:**
  - `native-builtins/src/classloader.rs:17466-17470` (`lk_outermost_enclosing`):
    `.find(|(inner, outer, _, _)| inner == &name && !outer.is_empty())`.
  - `native-builtins/src/lang_reflect.rs:609-611` (the compatible `getEnclosingClass` bridge): the same search.
- **The rule they break:** wave 16 made the FIRST self row decide (`declaring_member_row_outer`, behind
  `CRATONVM_DECLARING_CLASS_ANY_OUTER_ROW`; HotSpot's `find_inner_classes_attr`). These two walks were not moved to
  it. With rows `[(C, -, -), (C, O, "N")]` and no `EnclosingMethod`, HotSpot's `getEnclosingClass()` is null and `C`
  is its own outermost class. Both walks answer `O`.
- **Visible effect:** `privateLookupIn(C).in(O)` keeps PRIVATE. HotSpot drops PRIVATE and PROTECTED (modes 9; this
  VM gives 15). That is the access-widening direction, from hand-built bytes only. `getEnclosingClass()` also answers
  `O` on `--compatible`.
- **The ordering pitfall:** HotSpot's `check_inner_classes_circularity` ignores the whole attribute when a
  both-indices row is FOLLOWED by another row of the same inner class. So the member row has to come second to
  survive. `R21Fz6InnerRows1` (X$6) is built that way. Check that `cratonvm_reader::inner_classes_verdict` agrees.
- **Fix sketch:**
  - Both walks call `lang_class::member_row_outer_of` (the first self row, under the same switch).
  - The `getEnclosingClass` bridge keeps its `EnclosingMethod`-first order, which is HotSpot's.
  - Add a source pin like `r21w16_vm_declaring_class_uses_the_shared_rule`, so the three copies cannot drift.

## 3. Compatible-mode nesting answers for a row without `EnclosingMethod`, and for `EnclosingMethod` without a row (Low, `--compatible`)

- **Where:**
  - `native-builtins/src/lang_class.rs:37032-37050` (`row_nesting_kind`, the "with neither" arm, which answers
    Anonymous or Local from the row's shape).
  - `:37008` (`class_nesting_kind`'s `None => TopLevel`, which ignores `EnclosingMethod`).
  - `:344` (`resolve_simple_name`, which returns the row's simple name for any self row).
  - `:376` (`canonical_class_name`; the row arm is at `:406`).
- **HotSpot / JDK 25:**
  - `isLocalOrAnonymousClass` is `hasEnclosingMethodInfo`, which is `EnclosingMethod` alone.
  - `isTopLevelClass` is `!isLocalOrAnonymousClass() && getDeclaringClass0() == null`.
  - When neither holds, a self row's simple name is never read: `getSimpleName` is the binary-name tail and
    `getCanonicalName` is `getName()`.
  - With `EnclosingMethod` and no self row, the class is ANONYMOUS: `getSimpleBinaryName0` is null, the simple name is
    `""` and the canonical name is null.
- **Probe rows in `R21Fz6InnerRows1`, HotSpot against this VM's `--compatible` answer:**

  | Class | Shape | HotSpot | `--compatible` |
  |---|---|---|---|
  | X$2 | row `(X$2, -, -)`, no EM | top level, simple `"X$2"`, canonical `r21fz6.X$2` | anonymous, `""`, null |
  | X$5 | row `(X$5, -, Loc)`, no EM | top level, `"X$5"` | local, `"Loc"`, null |
  | X$4 | EM, no row | anonymous, `""`, null | top level, `"X$4"`, `r21fz6.X$4` |
  | X$6 | rows `(X$6, -, -)`, `(X$6, X, Six)`, no EM | top level | first row taken, so anonymous |

- **`--jdk-only`:** the JDK bytecode decides from the natives (`getSimpleBinaryName0`, `getDeclaringClass0`,
  `getEnclosingMethod0`), so this mode should already match. The probe checks it.
- **javac never writes these shapes.** Wave 16's comment ("the row reads as javac writes it") keeps them for the
  mock fixtures, which have no `EnclosingMethod`.
- **Fix sketch:**
  - Under `any_outer_row`: without `EnclosingMethod` and without an outer class, the answer is TopLevel.
    `class_nesting_kind`'s no-row arm answers Anonymous when `EnclosingMethod` is present.
  - `resolve_simple_name` and `canonical_class_name` follow the same kind: TopLevel takes the binary-name tail and
    `getName()`; Anonymous with no row takes `""` and null.
  - The mock tests that rely on EM-less local and anonymous rows need an `EnclosingMethod` in the mock, or the
    switch-off arm.

## 4. The redefinition forget races with a reflection computation that read the old tables (Low)

- **Where:**
  - `native-builtins/src/lang_class.rs:38471` (`forget_redefined_inner_class_answers`), called at
    `vm/src/vm/vm_exec.rs:18237`, after the class-manager writer is released.
  - The fills it races with:
    - `cache_insert` (`lang_class.rs:103`), from `resolve_simple_name` (`:344`) and `canonical_class_name`
      (`:376`).
    - The spoof-verdict memo (`spoof_verdict_memo`, `:38495`).
    - `CLASS_MODIFIERS_CACHE`.
- **Scenario:**
  1. Thread T1 runs `getCanonicalName()` on member class `M` of `X`. It reads `X`'s OLD `InnerClasses` and builds the
     name, but has not inserted it yet.
  2. Thread T2 redefines `X` (`CRATONVM_REDEFINE_SWAPS_INNER_CLASSES` swaps the table) and runs the forget.
  3. T1 inserts. The stale answer now stays for the life of the VM.

  The same applies to a spoof verdict computed from the old rows. Before wave 17 the tables never changed, so these
  memos could not go stale. HotSpot discards `reflectionData` on every `classRedefinedCount` change, so it cannot hit
  this.
- **Fix sketch:**
  - Keep a per-VM `inner_tables_generation` (an `AtomicU64` in the reflect door caches), bumped under the
    class-manager write lock next to the swap at `class_manager.rs:15228`.
  - Each fill reads the generation before it reads any table, and inserts only if the generation has not moved
    (re-read after the insert, and remove on a mismatch).
  - Alternatively, do the forget while the writer is still held, and have the fills hold the read guard through the
    insert. The memo locks are leaves, so the lock order allows it.

## 5. The `<init>`-return hook and the unwinder take the receiver from local 0 (Low)

- **Where:**
  - `vm/src/runtime/interpreter/exception_dispatch.rs:375` (`finalizer_init_returned_ctor`:
    `frame.get_local(0)` at the `return`).
  - `:273` (`unconstructed_mark_of_ctor`, pre-existing: `get_local(0)` at the throw).
- **The problem:** local 0 is an ordinary local. Verified bytecode may `astore_0` another reference after (and even
  before) the initializing call, keeping `uninitializedThis` in another local. javac never does this; hand-built
  classes may.
- **Scenarios:**
  - **(a)** JNI `AllocObject(F)`, then a `<init>` call that throws before `super(..)`. The JNI door withdraws the
    registration. The native then passes the object to `new G(obj)`, whose constructor runs `super(); aload_1;
    astore_0; return`. The hook re-registers `obj` (it is in `withdrawn_finalizables`), so `finalize()` later runs on
    an object whose `Object.<init>` never returned. HotSpot never registers it.
  - **(b)** The unwinder variant: a constructor that runs `aload_0; astore_2; aload_1; astore_0` and then throws
    before `super(..)` proves "unconstructed" for the object in local 0, which is the ARGUMENT. That withdraws the
    argument's registration, unless an interpreted `<init>` return had marked it `constructed`; a compiled
    construction never does. HotSpot finalizes the argument.
- **Fix sketch:**
  - Keep the `<init>` frame's entry receiver: the argument slot 0 as passed, which the frame may already hold in its
    `args` snapshot, or a new field written at frame push for `<init>` frames only.
  - Use that receiver in both places. Or refuse both the proof and the hook for a constructor whose code stores
    local 0 (`astore_0`, `astore 0`, `wide astore 0`). That is a one-pass scan that can be memoized per method, and
    javac never emits such a store.

## 6. The `0xb1` hook's cost and its kill switch (Low, perf)

- **Where:** the gate bit is set from `finalizer_register_at_object_init_enabled` (`interpreter.rs:10310`), not from
  `CRATONVM_FINALIZER_REREGISTER_AT_INIT_RETURN`. Past the bit, `finalizer_init_returned_ctor`
  (`exception_dispatch.rs:371`) does all of the following:
  - checks the receiver's has-finalizer bit;
  - runs `ctor_frame_pc_precedes_initialization`: a `read_recursive` of the class manager, `find_method`, a compare of
    the whole code against the frame's, a `Vec` of handler pcs, and the prefix scan;
  - takes the VM-wide `ref_processor` mutex.
- **Two problems:**
  - All of this runs for EVERY interpreted `<init>` return of a finalizable receiver, once per level of its
    constructor chain, although (per the wave-18 page `r21-finalize-reinit-residuals-FIXED-20261020.md`) only unverified
    code and JNI can ever benefit.
  - With `CRATONVM_FINALIZER_REREGISTER_AT_INIT_RETURN=0` the whole cost remains; only the processor returns early,
    under its lock.
- **Fix sketch:**
  - Latch the re-register switch in `VmConfig` and AND it into `DG_FINALIZER_INIT_RETURN`.
  - Skip `ctor_frame_pc_precedes_initialization` for a class the verifier passed: a verified `return` in `<init>`
    always follows the initializing call, so the scan can only answer `false`.
  - Optionally, read the row's `constructed` bit without the mutex (a per-object header bit, as the residual page
    already suggests).

## 7. Is a failed outer-class load in `in()` / `getDeclaringClass0` cached as a resolution error? (Low, verify)

- **Where:** `native-builtins/src/classloader.rs:17485-17492`, through `lang_class::class_seen_by_referencing_class_checked`
  (`lang_class.rs:39041`), which calls `class_id_by_name_via_referencing_class`.
- **What HotSpot does:** `compute_enclosing_class` resolves the outer class with `klass_at`. A `LinkageError` there
  (the `NoClassDefFoundError` that a loader's `ClassNotFoundException` becomes) is saved in the resolution-error
  table (`save_and_throw_exception`, JVMS 5.4.3). The second `in()`, a `getSimpleName()` or a `getDeclaringClass()`
  throws again WITHOUT calling the loader.
- **What to confirm:** if this door does not record the error, every call re-enters the user loader's `loadClass`.
  A loader's side effects (logging, counters, a loader that would succeed later) then become visible, and a class
  that appears later is found here but stays an error on HotSpot.
- **Probe:** `R21Fz6LookupInLoad1` prints `asks(Gone)`. HotSpot answers 1 after each step and after 1250 more
  attempts in the hot loop.
- **Fix sketch:** if the probe shows more than 1, record the failure against the referencing class's pool entry,
  the way the interpreter's constant-pool resolution does, and rethrow it from there.

## Wave 19 fixes (lane innerclasses), pending orchestrator build

None of this was built or run.

- **Item 2.** `lk_outermost_enclosing` and the `--compatible` `getEnclosingClass` bridge now take
  `lang_class::member_row_outer_of`, so the first self row decides. The bridge keeps its `EnclosingMethod`-first
  order. A source pin, `r21w19_member_rule_has_one_copy`, keeps all three copies on the one rule.
  - The ordering pitfall is still unsettled. `cratonvm_reader::inner_classes_verdict` IGNORES every table with two
    rows of the same inner index (it compares every row with every later row). If HotSpot does the same, the X$6
    table is dropped at define under `_need_verify`, and both VMs answer top level for that reason.
  - Run `R21Fz6InnerRows1` to settle it.
- **Item 3.** `class_nesting_of` (`CRATONVM_COMPAT_NESTING_FOLLOWS_ENCLOSING_METHOD`) is now the single decision
  for:
  - the nesting predicates;
  - `getSimpleName`: top level takes the binary-name tail; `EnclosingMethod` without a row gives `""`;
  - `getCanonicalName`: top level takes `getName()`; only a member gets a canonical name.

  Mock tests that rely on EM-less local and anonymous rows run with the switch off, through
  `legacy_em_less_rows`, a thread-scoped override. The mock has no `EnclosingMethod` setter, so the no-row
  `EnclosingMethod` case is tested only as the pure `no_row_nesting_kind`.
- **Item 4.** Per-VM `inner_tables_generation` (`CRATONVM_REFLECT_INNER_TABLES_GENERATION`).
  - The forget bumps it BEFORE it clears anything.
  - Every fill takes its guard before it reads a table: the simple and canonical names, the array simple name,
    the spoof verdicts, and the modifiers.
  - A fill skips its insert if the generation has moved, and removes its row if the generation moved during the
    insert.
  - The bump happens after the swap, when the writer is released, rather than under the lock. This is still
    sound: the swap precedes the bump, which precedes the clear.
- **Item 7.** The answer was NO: `class_id_by_name_via_referencing_class` resolves by name and records nothing.
  - Now `class_seen_by_referencing_class_checked` records a thrown `LinkageError` per VM, keyed by (referencing
    class, entry name), as the error's class and message (`CRATONVM_OUTER_RESOLUTION_FAILURE_STICKY`).
  - Later calls rethrow a new error of that class and message without asking the loader, even after the class
    has appeared.
  - A redefinition drops the class's rows, because its constant pool is new.
  - Residual: the key is the name, not the pool index, which is the same entry for every javac pool. The table is
    not shared with the interpreter's own cp-entry failure table, so an `ldc` of the same entry records
    separately. Sharing it needs a `NativeContext` door keyed by constant-pool index (`native-api`).

## 8. Reviewed, no defect found (Info)

- **`Lookup.in` rooting (wave 16):**
  - `lk_class_relation_rooted` pins every reference its callers read after the walk:
    - `this`, `lookup_class` and `target_class` in `lang_invoke.rs:19517`;
    - `target` in `classloader.rs:17625`.
  - It re-reads them on both `Ok` and `Err` and truncates the pin stack to its own base. `a` and `b` are read only
    before the first load: the walk works on class ids.
  - The order matches JDK 25: TRUSTED and same-class come first, then the module step, then `isSamePackageMember`
    (package, then `lookupClass`'s outermost class before the target's), then `checkUnprivilegedlookupClass`.
  - The ICCE text is `check_for_inner_class`'s. Loading without initializing matches `klass_at`.
- **`DECLARING_CLASS_REPLACE_MARKER`:**
  - Nothing resolves an `inlined_methods` triple to a member, and the marker is skipped where it must be:
    `names_subtype_sensitive_member`, `withdraws_for_finalizable_subtype` (`<init>` only), and the `NewSubtypeOf`
    log record.
  - The by-name matchers (`invalidate_for_class_change`, `InlinedClass`, `not_entrant.rs:2136`, the redefinition scan
    at `jit_bridge.rs:11190`) now see the body's class again, which is the intent.
  - It leaks only into diagnostics: the `[remap-frame]` lines (`vm/src/jit/conservative_roots.rs:15409`, `:16127`)
    print `p/A.<declaring-class>()I`, and the OSR refusal text (`jit/src/osr_entry.rs:1394`) counts it in "N
    inlined method(s)".
  - Any future consumer that maps the triples to methods (JVMTI `CompiledMethodLoad` inline records, JFR) must skip
    it. An accessor that filters it would make that structural.
- **Withdraw by id (opt-in):** correct as written. It relies on one invariant: a body that recorded ANY copied class
  recorded every splice's class. A body that mixes a recorded IR splice with an unrecorded legacy splice of
  `<init>` would be kept under the switch, which would make the finalizer bar fail open. Keep it opt-in until the
  finalizer probes have run with it.
- **Alloc prefetch (opt-in):**
  - The encodings are right: `0F 0D 88` disp32 is `PREFETCHW [RAX+d]`, and `0F 18 80` disp32 is `PREFETCHNTA`.
  - It reads RAX only and touches no register or flag.
  - A prefetch past the TLAB end cannot fault.
  - One serializing `CPUID` per emitted site (a VM exit under a hypervisor) is compile time only.
- **vh-const SATB null-old (opt-in):** sound for this VM's G1 and ZGC.
  - Both pre-barriers return at once on a null old value (`g1.rs:34272`, `zgc.rs:6879`). ZGC's `write_barrier` is only
    the generational card (`note_ref_store`), and the inline post gates still cover it.
  - A CAS or compareAndExchange with a null `expected` overwrites exactly null, or nothing when it fails.
  - A `set*` has the same read-then-store window as HotSpot's G1 pre-barrier, and SATB tolerates it: a value stored
    after the snapshot was reachable from the snapshot or allocated during the mark.
  - Under debug assertions, `g1.rs`'s `consume_satb_pre_barrier_epoch` check is not reached, because the inline path
    never calls `write_barrier`, and a declining post gate sends the whole operation through the helper.
- **Call-entry null guard (opt-in):**
  - Admission is receiver-bound: a private or final method, a final declaring or constant-pool class, or for
    `invokespecial` only a private method of the constant-pool class. So a subclass override can never be skipped.
  - A null receiver takes the decline edge to the call's own NPE.
  - The target class is recorded as copied bytecode, so a redefinition withdraws the body, and so do scoped
    breakpoints and method-event requests (`breakpoint_bars_compiling` at compile time; the scoped withdrawal reads
    `copied_classes` afterwards).
  - The abstract execution of the entry is closed: no call, store, allocation or throw on the skipped path, and a
    bounded step count.
  - The `volatile` bit is decoded but unused. That is correct on x86-64 (a plain load has acquire semantics under
    TSO); an AArch64 port would need `LDAR`.
- **Empty finalize (wave 17):**
  - All three `has_finalizer` writers use `class_has_finalizer_by_hotspot_rule`: the define at
    `class_manager.rs:11835` and the stub upgrade at `:17780` and `:17857`. No other code computes the flag from
    `declares_finalize`.
  - A side effect worth knowing: JDK 25's `Enum.finalize()` is `protected final` and empty, so before wave 17 every
    enum class was finalizable here.
  - Pre-existing gap: a stub upgrade recomputes only the upgraded class. Subclasses loaded while the parent was a
    stub keep the flag they got from the stub (the subclass layout propagation after `:17862` does not touch
    `has_finalizer`). This rarely matters, because JDK 25 classes with a non-empty `finalize` are rare.
  - **Wave 22 (lane finalize): still OPEN, not fixed, handed to the classloading owner.** It is the only finalizer
    item on this page or on `r21-review-w16-findings-20261019.md` that no verdict closes. The fix is in
    `classloading/src/class_manager.rs`, which lane hidden-resid owns this wave, so this lane could not make it.
    - **Divergence:** a stub upgrade can change a parent's `has_finalizer`, either way, because a stub declares no
      `finalize`. A subclass loaded while the parent was a stub keeps the old bit. HotSpot has no stubs: its
      subclass is finalizable exactly when the real parent is.
    - **Effect:** instances of such a subclass are not registered (or are registered) against HotSpot's answer.
      The published allocation recipe (`jit_alloc_class_cache`, `has_finalizer`) would also be stale.
    - **Fix sketch:**
      - after the upgrade's `has_finalizer` recompute (`class_manager.rs:18240`), when the value changed, walk the
        transitive subclasses in ascending id order (the order `recompute_subclass_layouts` already relies on);
      - recompute each one with `class_has_finalizer_by_hotspot_rule(class, parent_has, empty_is_none)`;
      - then, on the VM side, drop the subclasses' allocation recipes and run `SharedVm::note_finalizable_define`
        for each class that became finalizable (the finalizer compile bar);
      - put it behind a new default-on switch.
    - **Reach:** wherever a synthetic class stub is later upgraded, which is mainly `--compatible`. This was not
      checked for `--jdk-only`. It also needs a class with a non-empty `finalize()` that was stubbed before its
      bytes loaded.
    - **FIXED (wave 22, lane hidden-resid; pending orchestrator build), class-manager half.** Switch
      `CRATONVM_STUB_UPGRADE_FINALIZER_TO_SUBCLASSES` (correctness, default ON, `=0` leaves the subclasses' bits as
      they were).
      - `upgrade_synthetic_class` (`classloading/src/class_manager.rs`) keeps the stub's bit. When the upgraded
        class's `has_finalizer` differs from it, `propagate_finalizer_to_subclasses` walks `descendants_of`
        (breadth-first, parents before children) and recomputes each subclass with
        `class_has_finalizer_by_hotspot_rule(class, parent_has, empty_is_none)`.
      - For every subclass whose bit moved, it fires the JIT invalidate hook, which withdraws code that baked the
        allocation recipe. The function returns the ids that changed.
      - Test: `class_manager::tests::a_stub_upgrade_propagates_has_finalizer_to_loaded_subclasses`. It covers both
        directions, a grandchild, and a sibling with its own `finalize`.
      - **Residual, VM side, still OPEN:** nothing yet runs `SharedVm::note_finalizable_define` (the finalizer compile
        bar) for a subclass that became finalizable, or drops a published `jit_alloc_class_cache` recipe outside
        compiled code. The returned ids are the hook for that. Wiring them through the VM's define paths (after the
        class-manager guard drops) is outside this lane's files.
    - **FIXED (wave 23, lane finvm; pending orchestrator build), VM half.** Same switch,
      `CRATONVM_STUB_UPGRADE_FINALIZER_TO_SUBCLASSES`, read where the class manager latches it: under `=0` nothing
      is queued and nothing below runs. No new switch.
      - Stub upgrades happen inside many class-manager entry points (`load_class`, `fabricate_class`, the define
        arm), not on one VM path. So `upgrade_synthetic_class` now queues `(class, new bit)` for the upgraded class
        and for every subclass `propagate_finalizer_to_subclasses` moved, parents first. The queue is
        `ClassManager::stub_upgrade_finalizer_flips`; `take_stub_upgrade_finalizer_flips` and
        `has_stub_upgrade_finalizer_flips` read it.
      - The one place every class-manager write ends, `ClassManagerWriteGuard::drop`
        (`vm/src/vm/realms/class_realm.rs`), takes the rows while the guard is held. That costs one length test on
        a release that upgraded nothing. It takes them only when the store's owning VM is registered
        (`hook_vm_for_domain`); otherwise they wait for a later release. After the release it calls
        `SharedVm::note_stub_upgrade_finalizer_flips` (`vm/src/vm/vm_init.rs`).
      - (b) Every moved class's `jit_alloc_class_cache` recipe is dropped there. That recipe is the lock-free
        has-finalizer bit read by the interpreter's `new`, the JIT allocation helpers and `class_has_finalizer_of`.
        The drop works even when the JIT invalidate hook does not reach the VM. The next `new` rebuilds the recipe
        from the new bit. Before the upgrade no recipe could exist, because `ClassAllocInfo::build` refuses a stub
        chain. Compiled code that baked the bit is withdrawn by the hook flush, as in wave 22.
      - (a) A class that BECAME finalizable runs `note_finalizable_define`, which records its ancestors and
        withdraws their compiled constructors, exactly as a finalizable define does.
        - Wave 23 follow-up (item 1 of `r21-review-w23-findings-20261019.md`): the class's OWN `<init>`
          `jit_gate_pass` rows are dropped and its compiled constructor bodies withdrawn as well.
        - The gate's fill also re-reads the live bar after its insert.
        - Re-entrancy of the drain was reviewed: the guard is already released, and no nested guard is taken.
      - Reverse direction: a class that STOPPED being finalizable is handled as the define of a non-finalizable
        class is. No listener runs. Ancestor records left by earlier defines stay: they are sticky and fail closed.
        Instances registered while the old bit held keep their registration.
      - A raw `class_manager.write()` (not the guard) leaves rows queued; they are settled at the next guard release.
      - Tests:
        - `class_manager::tests::a_stub_upgrade_queues_its_finalizer_flips_for_the_vm` (both switch arms, a real
          upgrade);
        - `vm_init` `r21w14_finalizable_define_listener_tests::a_stub_upgrade_flip_drops_the_recipe_and_runs_the_listener`
          (both directions);
        - `::the_write_guard_release_settles_a_stub_upgrades_finalizer_flips` (define through the guard; the queue
          is empty and the subclass recipe gone after the release).
- **Ancestor walk and listener (wave 17):**
  - The walk now goes on past finalizable superclasses to `Object`.
  - With `record_reachable`, every reached ancestor is marked, and only the stale ones are withdrawn: `mark(id) &&
    stale`, where the mark always runs first.
  - The legacy JNI `DefineClass` runs the listener after `drop(cm)`.
  - I found no lock-order or fail-open path.
