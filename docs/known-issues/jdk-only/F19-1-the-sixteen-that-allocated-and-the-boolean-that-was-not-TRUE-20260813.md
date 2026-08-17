# F19-1 — The sixteen that allocated, the `Boolean` that was not `TRUE`, and the fifth boxing implementation that is the only correct one

**2026-08-13, lane F19.** Applies F11-1's NOMINATIONS N1, N2 and N3. Edits
exactly three files: `native-builtins/src/lang_invoke.rs`,
`vm/src/vm/vm_exec.rs`, and the new `regression-suite/src/RJdkReflBox.java`
(plus this record). **This lane did not build or run CratonVM**: every "before"
is a fact read out of the tree, every CratonVM "after" is explicitly
**PREDICTED**, and every HotSpot value is **MEASURED** on this host against
`openjdk 25.0.3 2026-04-21 LTS (25.0.3+9-LTS)` (Microsoft build) from
`scratchpad/f19/ReflBoxOracle.java` before it was written down.

---

## 1. Verdict

| | |
|---|---|
| oracle rows measured | **105**, byte-identical across three runs, under `-Xint`, and under `-XX:-UseCompressedOops` — so no row is a JIT or heap-layout artefact |
| `box_value` call sites in `lang_invoke.rs` | **21** (F11-1's count) |
| switched to `box_value_canonical` | **16** |
| left on `box_value`, MEASURED fresh | **4** — the `"F"`/`"D"` arms of the two collector loops |
| left on `box_value`, no observable | **1** — `getMemberVMInfo`'s vmindex, the brief's named exclusion |
| **the brief's "all 21 are measured canonical" is wrong** | 4 of the 21 are `F`/`D` arms and HotSpot answers **fresh** on every one (`mhnc.asTypeFloat` = false, `vhnc.fieldFloat` = false, `neg.floatValueOf` = false). Switching them would have been behaviour-identical — `box_value_canonical` delegates `F`/`D` straight back — but "all 21 canonical" is not a statement the measurement supports (§3.1) |
| `vm_exec.rs` proxy boxing | `Z` arm now resolves the real `Boolean.TRUE`/`FALSE`; payload normalised to 0/1; three Rust tests added |
| **a fifth boxing implementation, found by grepping the shape** | `classloading/src/proxy_gen.rs` emits `X.valueOf` **bytecode** into every generated `$ProxyN`. It is the only one of the five that is right by construction, because it does not implement boxing — it delegates to Java (§4.2) |
| new regression vector | `RJdkReflBox`, **102 checks**, `fails=0`, `PASS` on HotSpot; 116 lines out, 116 through `extract()`; `harness-selfcheck.sh` reports **1 vectors sound, 0 flagged**; **mutation-checked in three directions** (§5.3) |
| NOMINATIONS raised | **4** |

**The one thing in this record that a reader should not skim.** F11-1 said the
proxy `args[]` defect was "the same bug, one layer over, in a file that cannot
see the fix". That is true of the code. It is *not* the whole picture: this
tree has **two** proxy argument-boxing implementations, and the other one —
`proxy_gen.rs`'s emitted `valueOf` bytecode — is already correct. Which of the
two answers is a ROUTE question that no amount of reading settles (§4.2). The
fix is still right; the claim "any handler that identity-tests a boxed argument
disagrees with HotSpot here" is one route's worth of right, and the record now
says which route and how to discriminate it.

---

## 2. The oracle

`scratchpad/f19/ReflBoxOracle.java` asserts nothing and prints
`<label>=<identity>`, where `identity` is `==` against `X.valueOf(v)`. Every
in-bound operand is inside every cache (`int 7`, `char 'a'`, `byte 3`,
`short 9`, `long 5`, `true`), so a `false` there can only mean "this path
allocates". Operands are non-`final` statics: a `static final int` initialised
with a literal is a compile-time constant and `javac` folds every use of it.

```
== 1. Field.get, instance, in bound ==                 == 5. MH collector / varargs ==
field.int                        true                  mhcoll.int              true
field.char                       true                  mhcoll.char             true
field.byte                       true                  mhcoll.bool             true
field.short                      true                  mhcoll.long             true
field.bool                       true                  mhcolloob.int1000       false
field.boolTRUE                   true                  mhvar.int               true
field.long                       true                  mhvar.char              true
field.intStable100               true
field.intStableSameField100      true                  == 6. VarHandle ==
                                                       vh.fieldInt             true
== 1b. Field.get, static ==                            vh.fieldChar            true
fieldstatic.int                  true                  vh.fieldBool            true
fieldstatic.char                 true                  vh.fieldBoolTRUE        true
fieldstatic.bool                 true                  vh.fieldLong            true
fieldstatic.long                 true                  vh.fieldByte            true
                                                       vh.fieldShort           true
== 1c. the arms a cache must NOT take ==               vhnc.fieldFloat         false
fieldoob.int1000                 false                 vhoob.fieldInt1000      false
fieldoob.char200                 false                 vh.staticInt            true
fieldoob.short1000               false                 vhoob.staticInt1000     false
fieldoob.long1000                false                 vh.getAndSetInt         true
fieldoob.static1000              false                 vh.compareAndExchangeInt true
fieldnc.float                    false                 vh.arrInt               true
fieldnc.double                   false                 vh.arrChar              true
fieldoob.selfid                  false                 vh.arrBool              true
fieldnc.floatSelfid              false                 vh.arrLong              true
blind.equalsOob                  TRUE                  vhoob.arrInt1000        false
blind.equalsFloat                TRUE                  vh.byteViewInt          true
                                                       vh.byteViewLong         true
== 2. java.lang.reflect.Array.get ==
array.int                        false                 == 7. FFM layout VarHandle ==
array.char                       false                 ffm.layoutInt           true
array.byte                       false                 ffmoob.layoutInt1000    false
array.short                      false                 ffm.layoutChar          true
array.bool                       false                 ffm.layoutLong          true
array.boolTRUE                   false                 ffm.layoutByte          true
array.long                       false
array.selfid                     false                 == 8. Proxy args[] ==
array.blindEquals                TRUE                  proxy.int               true
array.refPassthrough             TRUE                  proxy.char              true
array.getIntAutobox              TRUE                  proxy.bool              true
                                                       proxy.boolTRUE          true
== 3. Method.invoke ==                                 proxy.long              true
invoke.int                       true                  proxy.byte              true
invoke.char                      true                  proxy.short             true
invoke.byte                      true                  proxyoob.int1000        false
invoke.short                     true
invoke.bool                      true                  == 9. controls ==
invoke.boolTRUE                  true                  neg.newObjects          false
invoke.long                      true                  neg.floatValueOf        false
invokenc.float                   false                 neg.doubleValueOf       false
invokenc.double                  false                 ctrl.integerValueOfCached   true
invokeoob.int1000                false                 ctrl.integerValueOfUncached false
invokeoob.char200                false                 ctrl.byteValueOfAll     true
invokeoob.selfid                 false
                                                       == 4. MH return adaptation ==
                                                       mh.asTypeInt/Char/Bool/Long/Byte/Short true
                                                       mhnc.asTypeFloat        false
                                                       mhoob.asTypeInt1000     false
                                                       mh.invokeAsObject       true
                                                       mh.invokeWithArgsInt/Char/Long true
```

### 2.1 Six rows F11-1 did not have, and what each one buys

| row | answer | why it is worth a row |
|---|---|---|
| `array.refPassthrough` | **true** | `Array.get` on an `Object[]` hands back the stored reference. "Fresh" is about BOXING, not copying — a fix that made `Array.get` allocate unconditionally would break this and no other row |
| `array.getIntAutobox` | **true** | `Array.getInt` returns a primitive; the autobox at the call site goes through `valueOf` like any other. So `Array` is not uniformly fresh — only its boxing native is |
| `vh.compareAndExchangeInt` | **true** | the RMW funnel has more than one mode; `getAndSet` alone does not cover it |
| `ffm.layoutChar/Long/Byte` | **true** | F11-1 measured only `JAVA_INT`. One reachable carrier is not the surface — the same four-of-nine shape `lang_invoke.rs`'s own FFM comment records |
| `ctrl.byteValueOfAll` | **true** | `ByteCache` covers all 256 values and has **no fresh arm at all**, which is why no `byte` row belongs in the out-of-bound family |
| `field.intStable100` / `…SameField100` | **true** | 100 repeats through a fresh `Field` object and through one cached `Field`. A cache consulted only on the first call, or invalidated by accessor inflation, passes every single-shot row and fails these two |

**Every `false` row above is still `.equals`-equal** (`blind.equalsOob`,
`blind.equalsFresh`, `array.blindEquals` — all `true`). An equality-shaped
assertion passes against this defect **in both directions**, which is why not
one row in §5 compares a payload.

---

## 3. N1 — `lang_invoke.rs`

### 3.1 The 21, decomposed

| # | site (pre-edit line) | function | measured | after |
|---|---|---|---|---|
| 1 | `:570` | `layout_vh_get` (FFM layout VH) | `ffm.layout*` = true | **canonical** |
| 2 | `:3176` | `segment_vh_get` (`MemorySegment` VH) | same family | **canonical** |
| 3 | `:3336` | `vh_box_access_result` (RMW funnel) | `vh.getAndSetInt`, `vh.compareAndExchangeInt` = true | **canonical** |
| 4 | `:3383` | `varhandle_get`, `byte[]` view | `vh.byteViewInt/Long` = true | **canonical** |
| 5 | `:3395` | `varhandle_get`, `ByteBuffer` view | same | **canonical** |
| 6 | `:3408` | `varhandle_get`, array-element fast path | `vh.arr*` = true | **canonical** |
| 7 | `:3438` | `varhandle_get`, instance field by index | `vh.field*` = true | **canonical** |
| 8 | `:3455` | `varhandle_get`, instance field by name | same | **canonical** |
| 9 | `:3477` | `varhandle_get`, static field | `vh.staticInt` = true | **canonical** |
| 10 | `:3497` | `varhandle_get`, `VH_KIND_ARRAY` | `vh.arr*` = true | **canonical** |
| 11 | `:8742` | `box_direct_primitive_return` | `mh.asType*` = true | **canonical** |
| 12 | `:9576` | `mh_dispatch` collector, `"I"` | `mhcoll.int` = true | **canonical** |
| 13 | `:9577` | `mh_dispatch` collector, `"J"` | `mhcoll.long` = true | **canonical** |
| 14 | `:9578` | `mh_dispatch` collector, `"F"` | **fresh** | `box_value` |
| 15 | `:9579` | `mh_dispatch` collector, `"D"` | **fresh** | `box_value` |
| 16 | `:10597` | `build_varargs_array`, `"I"` | `mhvar.int` = true | **canonical** |
| 17 | `:10598` | `build_varargs_array`, `"J"` | true | **canonical** |
| 18 | `:10599` | `build_varargs_array`, `"F"` | **fresh** | `box_value` |
| 19 | `:10600` | `build_varargs_array`, `"D"` | **fresh** | `box_value` |
| 20 | `:10634` | `auto_box_return` | `mh.invokeWithArgs*` = true | **canonical** |
| 21 | `:12702` | `native_mhn_get_member_vm_info` | no observable | `box_value`, annotated |

16 switched, 5 not. Switching #14/15/18/19 would be behaviour-identical, since
`box_value_canonical` delegates `F`/`D` to `box_value`; they are left spelled
`box_value` so the measured asymmetry is visible **at the site** rather than
hidden inside a helper. `Float`/`Double` have no cache on HotSpot at all
(`neg.floatValueOf` = false), so "completing the family to eight" is a
regression, not a completion.

### 3.2 The two hazards, checked per site rather than assumed

**Hazard 1 — the `Value` variant, not only the descriptor.** Checked at each
of the 16. The result splits three ways, and the split is the finding:

* **Cannot mismatch (7 sites): 1, 2, 4, 5, 6, 10, and the four collector
  arms.** In every one, the value and the descriptor are produced by a single
  `match` on the same discriminant — `layout_vh_read`/`layout_vh_carrier_desc`
  on `shape.carrier`, `seg_decode_value`/`seg_shape_desc` on `SegShape`
  (including `Address`, which is `Value::Long` and `"J"` in both),
  `byte_view_get`/`byte_view_desc` on `elem`, and the collector arms, which
  match on the variant and pass the descriptor as a literal. The guard is
  inert here and the comment says so.
* **Can mismatch, and it is load-bearing (5 sites): 3, 7, 8, 9, 11, 20.**
  `vh_access_value_desc` resolves the VARIABLE's declared descriptor while the
  value came from wherever the access mode computed it; `ctx.get_field`
  returns the RAW slot; `ret_desc` is the target's DECLARED return type. A
  `("J", Value::Int)` pair is reachable at every one of them, and
  `box_value_canonical` answers it by falling back to `box_value` — i.e.
  today's behaviour verbatim, never a cached `Long.valueOf(0)`.
* **Already-null shapes (1 site): 5.** `byte_buffer_view_get(…)
  .unwrap_or(Value::Object(None))` can present `(desc, Object(None))`. That
  matches none of the six cached arms and falls back, preserving the existing
  (separately nominated) shape byte-for-byte.

**Hazard 2 — never `unwrap_or(Value::Object(None))`.** Not applicable at any
site: F11-1 put the failure-to-`box_value` fallback inside
`box_value_canonical` itself, so no call site can reintroduce the null mapping
without editing the helper. Verified by reading `lang_class.rs:4788-4791`.

### 3.3 A pre-existing defect found at sites 7/8/9, NOT fixed here

`lang_class::native_field_get` calls `coerce_reflective_field_value` before
boxing, because "a long field whose slot currently carries an Int must be
widened before it is placed in a Long wrapper. Otherwise `Long.longValue()`
later receives the raw compact-Int bits as a supposed long". **The VarHandle
field-read path has no such widener.** That is a wrong-ANSWER defect, older
than and independent of anything in this lane, and `coerce_reflective_field_value`
is a private `fn` in a different file. Nominated (§7 N3); the site comment
names it so the next reader does not re-derive it.

### 3.4 The unused-import trap this created

Before the edit, `box_value` was imported unqualified and used at 12 sites.
After it, the only unqualified use left was in a `#[cfg(test)]` fixture at
`:13046` — an `unused_imports` warning in every non-test build. `box_value` is
therefore no longer imported; the four `F`/`D` sites and `getMemberVMInfo`
spell `crate::lang_class::box_value` in full, and the test's fixture was
qualified with a comment saying why it must stay FRESH (the canonical route
would hand it the shared `Boolean.FALSE`, whose slot 0 the test then writes).

---

## 4. N2 — `vm_exec.rs`

### 4.1 What changed

`proxy_box_value_for_desc`'s `Z` arm now calls a new
`proxy_canonical_boolean(shared, v != 0)`, which loads `java/lang/Boolean`,
walks its static fields for `TRUE`/`FALSE`, and returns the live static —
exactly the resolution `native_boolean_value_of` performs, on the `&SharedVm`
side of the boundary where the native's `&mut dyn NativeContext` is not
reachable. On a miss it returns `None` and the caller falls through to the
existing allocation, **never to `null`**.

`get_static_shared`'s miss answer is `Value::Int(0)`, so the result is
match-checked for `Object(Some(_))` rather than unwrapped: a caller that
trusted it would put an `Int` into an `Object[]` slot.

**A second, non-identity change at the same arm.** The old code stored the raw
slot, so a `boolean` parameter whose slot held 5 produced a `Boolean` carrying
5 — a wrong ANSWER, since `booleanValue()` and every `toString` path read that
slot. It is now normalised with the same `val != 0` the native uses. The
contrast (an `int` parameter carrying 5 is still 5) has its own assertion.

**The other five arms are still fresh.** `C B S I` and `J` need `lang_math`'s
process-global caches, which are `pub(crate)` to `cratonvm-native-builtins` and
reachable only through a `&mut dyn NativeContext`. Minting a cache in
`vm_exec.rs` would make it the fifth boxing implementation *and* the second
`IntegerCache`. Nominated (§7 N1), not done.

### 4.2 The fifth boxing implementation — and the route question

The brief said to grep the shape, not the symbol, because there might be a
fifth. There is, and it is the interesting one:

```
classloading/src/proxy_gen.rs:1258-1277
    DescKind::Int    -> cp.add_methodref(wrapper, "valueOf", "(<c>)L<wrapper>;")
    DescKind::Long   -> cp.add_methodref("java/lang/Long",   "valueOf", "(J)Ljava/lang/Long;")
    DescKind::Float  -> cp.add_methodref("java/lang/Float",  "valueOf", "(F)Ljava/lang/Float;")
    DescKind::Double -> cp.add_methodref("java/lang/Double", "valueOf", "(D)Ljava/lang/Double;")
```

Every generated `$ProxyN` boxes its arguments with a real `invokestatic
X.valueOf`, and `java/lang/Integer.valueOf(I)` **is** registered in the
essential/real-JDK registry (`lang_math::register_wrapper_natives`, called from
`lib.rs:14679` under a comment that says it is kept there precisely so
real-JDK bytecode's autoboxing calls link). So that route is canonical **by
construction**: it is the only one of the five that does not implement boxing
at all — it delegates to Java, and therefore cannot drift from `Integer.valueOf`
because it *is* `Integer.valueOf`.

`CRATONVM_REAL_PROXY` and `CRATONVM_REAL_PROXY_SUPER` both default **ON**, so
`Proxy.newProxyInstance` produces one of those real classes. But
`class_chain_reaches_proxy_instance` matches `java/lang/reflect/Proxy` as well
as the synthetic `Proxy$Instance` (`typecheck.rs:965`), and both
`invoke.rs:1852`'s `is_proxy_dispatch` and `vm_exec.rs:9855` intercept on it
**before** the receiver's own bytecode runs. Reading the sources therefore says
the emitted `valueOf` calls are unreached and `proxy_box_value_for_desc` is the
live boxing.

**That is a reading, not a measurement, and this lane cannot make it one.**
`RJdkReflBox --only=proxy` is the discriminator, and it distinguishes three
outcomes rather than two:

| `proxy` family, on CratonVM | conclusion |
|---|---|
| green **before** this change | the emitted bytecode answers; `proxy_box_value_for_desc` is dead on this path and §4.1 is a correctness fix for the degrade route only |
| red before, green after | the shim answers; §4.1 is on the live path, as read |
| still red after | a third route, or the `C B S I J` arms §4.1 deliberately left fresh — read the per-row `CK` lines, they name which |

`real_proxy_strict()` defaults **OFF**, so a genuine generation failure
*silently* degrades to the shim. That alone makes the shim worth fixing
whichever way the discriminator lands: a silent degrade that also changes
observable identity is two defects wearing one.

### 4.3 The tests, and what they deliberately do not assert

Three, in `vm_exec.rs`'s own test module:

* `a_boolean_proxy_arg_is_never_null_even_with_no_canonical_instance` — the
  fallback stays an object.
* `a_boolean_proxy_arg_carries_0_or_1_and_never_the_raw_slot` — five raw
  slots including `i32::MIN`, plus the `"I"` contrast so the normalisation
  cannot over-reach. Holds on BOTH routes, so it is not a fixture-shape test.
* `the_boolean_proxy_arm_still_routes_through_the_canonical_resolver` — a
  source witness, because the two behavioural tests pass unchanged if the arm
  is reverted: the fallback they exercise **is** that code. Needles are
  assembled with `format!` at runtime (spelled as literals they would match
  the test's own source, since the file being searched is that file) and
  matched against a whitespace-stripped copy, so rustfmt cannot break them.
  Verified: each of the three needles occurs **exactly once** in the file both
  as written and after `rustfmt --emit stdout`.

**Not asserted here:** that the returned object IS the object in
`java.lang.Boolean.TRUE`. That needs a bootstrapped `java/lang/Boolean` whose
`<clinit>` has run, which a `SharedVm::new` fixture does not have. It is
asserted end-to-end instead, by `RJdkReflBox`'s `proxy.boolTRUE` row against
the oracle — which is the better place for it.

---

## 5. N3 — `regression-suite/src/RJdkReflBox.java`

### 5.1 Why a new class rather than rows in `boxid`

F11-1 §6.1's "0 of 69" result is mechanical *because* `RJdkIntrinsics3` imports
no `java.lang.reflect` type at all. Adding reflection there would invalidate a
published result. New class.

### 5.2 Shape

102 checks in 11 families, each closed by `sectionEnd(<name>, <n>)` with an
exact denominator:

```
fieldget 9   fieldstatic 4   fieldfresh 11   arrayget 11   invoke 12
mhreturn 12  mhcollect 4     varhandle 20    ffm 5         proxy 8    controls 6
```

Both directions are covered and neither can stand in for the other:
`fieldget`/`fieldstatic`/`invoke`/`mhreturn`/`mhcollect`/`varhandle`/`ffm`/`proxy`
assert **canonical**, `arrayget` and the `*oob`/`*nc` rows assert **fresh**,
and `controls` reads through no reflective machinery at all so it separates
"the reflective path is wrong" from "`==` has stopped discriminating" and from
"the caches themselves are wrong".

Dialect, per `harness-guard.sh`: one observable per `CK RJdkReflBox
<name>=<ACTUAL>` line carrying the **VM's own answer**; `fails=` and `checks=`
on **separate** lines (a combined line makes `harness_check_count` return the
string `"N fails=M"` and G3 silently no-ops inside its own `2>/dev/null`);
`PASS RJdkReflBox (N checks)` parenthesised and on the clean path only.

**Nothing throws mid-run.** Each family is wrapped; a throwable is reported as
a row (`CK RJdkReflBox FAILED <family> threw=<class>`), the family denominator
is re-synced, and the process exits non-zero only at the end. A VM that fails
half this file still prints the other half — the difference between a diff that
names the defect and a diff that says "it stopped". `sectionEnd` mismatches
reach `fails` for the same reason rather than throwing.

### 5.3 Evidence that the vector works, and that it can lose

Measured on HotSpot 25.0.3+9, in the scheduled shape:

```
rc=0, PASS RJdkReflBox (102 checks), fails=0
116 lines out, 116 through extract()          -> G1 cannot fire
md5 identical over 3 runs; identical under -Xint
ONLY="RJdkReflBox" bash regression-suite/harness-selfcheck.sh
    -> HARNESS SELF-CHECK: 1 vectors sound, 0 flagged
javac -Xlint:all                              -> no warnings
--list and --only=<family> both work
```

**Mutation-checked in three directions.** A guard that has never been shown to
fire is the same species of defect as the one it guards against:

| mutant | simulates | result |
|---|---|---|
| A | a VM that "unified" its two boxing helpers: `Array.get` becomes canonical | `array.int`, `array.char`, `array.selfid` FAILED, `fails=3`, exit 1, **no `PASS` line** |
| B | a VM whose `Field.get` allocates (this tree before F11-1) | `field.int`, `field.char` FAILED, `fails=2`, exit 1, no `PASS` |
| C | a row silently lost to an edit (`array.byte` deleted) | `arrayget ran 10 checks, header says 11`, `fails=1`, exit 1, no `PASS` |

Mutant A is the one that matters: it is the direction **no equality-shaped
check anywhere in the suite can see**, and before this file nothing in
`regression-suite/src` could fail on it.

### 5.4 Three rows deliberately left out, with the measurement

`mhcoll.char`, `mhcoll.bool` and `mhvar.char` are all **true** on HotSpot and
are **predicted red on CratonVM after this change**, for a reason this file is
not the gate for. `mh_dispatch`'s collector arm picks the wrapper CLASS from
the VM `Value` variant, and `boolean`/`char`/`byte`/`short` all share
`Value::Int` with `int` — so a `char` element is boxed as `Integer`. That is a
wrong-wrapper-type defect, not a wrong-identity one; fixing it needs the
target's `MethodType`, which that arm does not have (`comp` is the collector
ARRAY's component and is `Ljava/lang/Object;` for the ordinary case).

Including them would make the vector red for a defect it is not measuring,
which is how a gate stops being read. They are nominated (§7 N2) with the
measurement and are three lines from being moved in.

### 5.5 Predicted effect

**PREDICTED, not measured.** On CratonVM before this lane's change,
`RJdkReflBox` is predicted RED on every canonical row — that is the gate doing
its job. After it, the predicted state is green except where §7 names a
residual. Rows whose verdict is *predicted to flip* versus merely *become
reachable*:

* **flip** (`false` -> `true`): every row in `fieldget`, `fieldstatic`,
  `invoke`, `mhreturn`, `varhandle`, `ffm`, `mhcollect`'s canonical rows —
  these are F11-1's three sites plus this lane's sixteen.
* **already correct, asserted for the first time**: `arrayget` (untouched by
  F11-1 and by this lane — `lib.rs::native_array_get` inlines its own
  `alloc_wrapper`), every `*oob`/`*nc` row, and `controls`.
* **route-dependent**: `proxy` — see §4.2's three-way table.
* **unknown**: `vh.fieldLong`, which additionally depends on whether the raw
  slot presents as `Value::Long` (§3.3). Written by `putfield` with a `Long`
  in this fixture, so it is expected to fire the canonical arm; if it is the
  single red row, §3.3 is the reason and N3 is the fix.

If a row this section says cannot move does move, start there.

---

## 6. GC

No new cached storage. §3 routes callers into the six caches that already live
in `lang_math.rs`, are already reported by `gc_scan_value_of_cache_roots` and
already remapped by `gc_update_value_of_cache_refs` — F11-1 §5 verified the two
sets are identical mechanically and that `vm/src/memory/native_roots.rs`
registers them as one `VmRootSource { scan, remap }` pair, so they cannot be
wired independently.

§4 adds no storage at all: `proxy_canonical_boolean` returns a **live static
field**, which is a GC root in its own right (class statics are scanned) and
therefore cannot go stale — the same argument `native_boolean_value_of`'s
comment makes for not keeping a private mirror.

One residual, stated so a bisect can land on it: canonical instances now reach
more places, and a wrapper is immutable *in Java* but not in Rust. F11-1 §8
searched and found no consumer-side writer of a wrapper's slot 0 outside the
allocating sites themselves. This lane adds no writer; the one Rust test that
writes a wrapper slot (`auto_box_return_preserves_already_boxed_primitive_result`)
was explicitly kept on the FRESH helper for that reason (§3.4).

---

## 7. NOMINATIONS

### N1 — `native-builtins/src/lang_math.rs`: a read-only canonical lookup, so `vm_exec.rs` stops being fresh for `C B S I J`

`proxy_box_value_for_desc`'s other five arms are measured canonical on HotSpot
(`proxy.int`/`char`/`long`/`byte`/`short` all true) and still allocate. The
blocker is reach, not difficulty: the caches are `pub(crate)` and keyed on
`vm_identity()`, and `vm_exec` has a `&SharedVm`, not a `&mut dyn
NativeContext`.

Smallest correct shape — a `pub fn` beside the caches that takes a raw
`vm_identity: usize` and a value, and **reads without allocating**:

```rust
pub fn canonical_wrapper_if_cached(vm_identity: usize, desc: &str, v: Value) -> Option<ObjectRef>
```

`None` when the value is out of bound or the slot is unpopulated; the caller
keeps its existing allocation for that case. It must not allocate or populate,
because populating a cache without a `NativeContext` cannot run `<clinit>`.
Do **not** add a second cache in `vm_exec.rs` — that is how a tree gets two
`IntegerCache`s. Sizing and ownership are the picking lane's.

### N2 — `native-builtins/src/lang_invoke.rs`: the collector arms box by `Value` variant, not by parameter type

MEASURED on HotSpot 25.0.3+9: `mhcoll.char` = true, `mhcoll.bool` = true,
`mhvar.char` = true — i.e. `asCollector(Object[].class, 1).invoke('a')` yields
`Character.valueOf('a')`. On CratonVM the two collector loops
(`mh_dispatch` ~`:9731`, `build_varargs_array` ~`:10756`) choose the wrapper
from the `Value` variant, and `Z C B S I` share `Value::Int`, so a `char`
element becomes an `Integer`. Wrong CLASS, not wrong identity.

The fix needs the target handle's `MethodType` parameter types at the boxing
loop; `comp` is the collector array's component and is `Ljava/lang/Object;` in
the ordinary case, so it cannot supply them. Not attempted here because it is a
feature, not a switch. The three fixture rows are written out in
`RJdkReflBox.java`'s `mhcollect` comment and drop straight into that family
with `sectionEnd("mhcollect", 7)`.

### N3 — the VarHandle field path has no `coerce_reflective_field_value`

`lang_class::native_field_get` widens a `long` field's raw slot before boxing;
`varhandle_get`'s three field arms (`lang_invoke.rs` ~`:3550`, `:3572`,
`:3597`) do not. A `long` field whose slot presents as a compact `Value::Int`
therefore yields a `Long` wrapper carrying the raw compact-Int bits, and
`longValue()` reads them as a long — the wrong-ANSWER shape, older than this
lane and independent of it. `coerce_reflective_field_value` is a private `fn`
in `lang_class.rs`, so the fix is to make it `pub(crate)` and call it at those
three sites (and to check `vh_box_access_result` for the same shape).
`RJdkReflBox`'s `vh.fieldLong` is the row that would catch it.

### N4 — `regression-suite/run.sh`: schedule `RJdkReflBox`

**Not applied by this lane** — `run.sh` is outside its write scope. Exact
literal edit, one line, `JDKONLY_CLASSES`:

OLD (the tail of the `JDKONLY_CLASSES=` line, `run.sh:205`):
```
RJdkMapViews RServiceLoaderDoubleSource"
```
NEW:
```
RJdkMapViews RServiceLoaderDoubleSource RJdkReflBox"
```

`JDKONLY_CLASSES` rather than `CORE_CLASSES`, for two reasons: it is the corpus
this lane serves, and it is where the machinery the vector uses already lives
(`RJdkProxy`, `RJdkHandles`, `RJdkForeign`). Checked before nominating:

* no `HARNESS_NONDISCRIMINATING` (G5) row is needed —
  `HARNESS_MODE_RE='synthetic-jdk'` and the source does not match it (verified
  by running the regex against the file);
* no `harness-uncounted.txt` entry is needed — the vector publishes
  `checks=102`, so G3 is satisfied;
* `UNREGISTERED_CLASSES` must NOT gain it; unregistered is exactly the state
  the census warns about, and fatally so under `STRICT_COVERAGE=1`;
* `ONLY="RJdkReflBox" bash regression-suite/harness-selfcheck.sh` already
  reports **1 vectors sound, 0 flagged** against the oracle.

One caveat, and it is pre-existing rather than introduced: this vector imports
`java.lang.foreign`, which was preview before JDK 22, so it cannot compile
under `RELEASES="17 21 25"`. `RJdkForeign` (JDK-only list) and
`RForeignLayoutJdkInterfaces` (**core** list) already have that property, so
scheduling this one changes nothing about which levels the suite can build —
but if the multi-release path is ever repaired, all three move together.

---

## 8. Residuals

* Nothing in this record claims a CratonVM behaviour was observed. §5.5 is a
  prediction, written so that one run can falsify it.
* Both edited Rust files were parse-checked with `rustfmt --emit stdout` on a
  **copy** in the scratchpad (rc=0 both), no added line exceeds 100 columns,
  and both files remain pure CRLF (14,122 / 29,229 lines, zero bare LF).
  Neither was compiled: `cargo` is the orchestrator's.
* `cglib_enhancer.rs:4978` remains `box_value`. F11-1 reasoned it should
  switch (CGLIB's generated `FastClass` boxes with `valueOf` bytecode) but
  could not measure it, and there is no CGLIB on this host, so this lane
  neither confirms nor acts on it. It is also not this lane's file.
* `craton_gpu.rs:1327` remains `box_value`, correctly: there is no oracle for
  a CratonVM-only API.
* `lib.rs::native_array_get` still does not say that allocating IS its
  contract (F11-1 N4, unaddressed — not this lane's file). It is one sentence
  away from a "deduplication" that silently turns `array.int` green, and
  `RJdkReflBox`'s `arrayget` family is now the thing that would catch it.
