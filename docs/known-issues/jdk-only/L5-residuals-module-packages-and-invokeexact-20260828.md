# L5's two recorded-open items, closed — and a third the probe found on the way

**Status: COMPLETE 2026-08-28.** Extends lane L5 of
`HANDOFF-20260828-SCOPE.md` past the 207 `native-won` triples it closed.
Worktree `h2-known-issues-206dee`, branch
`claude/jdk-only-mode-handoff-09b48c`.

`L5-reflection-lane-complete-20260828.md` closed the lane's dispatch worklist
and named two items it left open, both recorded in
`primitive-class-had-a-loader-and-two-deeper-gaps-20260827.md` §3 and §4:

* `java.lang.Module.getPackages()` for `java.base` is short (`<100`);
* `MethodHandle.invokeExact` does not enforce its exact signature.

Both are real. Probing them turned up a third defect family neither record
mentions, and one plain wrong VALUE on the `invoke` side.

## 1. The instrument

`probes/L5ModuleInvokeSweep.java` — 125 rows against HotSpot 25.0.3+9, both
modes, stdout only. **38 differing rows at the start, 2 at this point**,
identical in Compatible and `--jdk-only` throughout — so every one is an
ordinary defect and none is a mode defect.

Hygiene notes specific to this surface, each of which cost a row:

* `getPackages()` returns an UNORDERED `Set`, so no row may print the set, an
  iteration order, or "the first element that is missing". Every row is a size
  bucket, a membership test, or a COUNT. The first draft printed the first
  missing export and named a different package on two consecutive runs.
* `MethodHandle.asType` to a NARROWER argument type is refused by HotSpot, and
  my first draft called `asType((long,long)long)` on an `(int,int)int` handle
  believing it a widening. It is not — `asType` converts the CALL's arguments
  to the HANDLE's, so `long` arguments would have to narrow. The oracle threw,
  main died at that line, and the 40 rows after it went untested in all three
  VMs at once.
* That last one is the reason `sweep.sh` now checks that every file ends in
  `DONE`. The row-count check it already had compares the VMs against the
  ORACLE, so it is blind by construction to a probe bug that kills all three
  at the same row: three equally-truncated files agree perfectly.

## 2. `Module.getPackages()` — a premise in a comment outliving its subject

**Result: all 15 rows clean in both modes**, on a binary carrying this fix and
nothing else.

```text
java.base.getPackages().size() > 100        HotSpot true    CratonVM false
java.base.getPackages() has jdk.internal.loader       true            false
getPackages().equals(getDescriptor().packages())      true            false
exported packages absent from getPackages()              0               54
```

`module_package_names` special-cased two names to hand-maintained lists:

```rust
"java.base" => BOOT_JDK_PACKAGES.iter()...,   // 63 entries
"java.xml"  => JAVA_XML_PACKAGES.iter()...,
_           => ctx.module_packages(name)...,  // the VM's own ModuleRegistry
```

with the stated reason "the registry does not enumerate jimage packages".

**It does.** `build_module_descriptor` has always read `ctx.module_packages`,
which is why `getDescriptor().packages()` answered >100 on the same VM, in the
same process, for the same module. The special case did not protect a gap; it
shadowed the live source for exactly the two names that source knows best.

So the two answers disagreed **in both directions** — the descriptor held
packages `getPackages()` lacked, and the hand list held names the descriptor did
not — and 54 packages `java.base` *exports* were missing from the set that is
supposed to contain them. A caller enumerating a module's packages for a scan
got a short answer with no error.

The fallback direction is unchanged and still deliberate: an over-inclusive set
is safe for the `getPackages().contains(pn)` callers this file serves, an
under-inclusive one is not. An empty registry answer still falls back to the
hand list. What changed is only which one is asked first.

**This is the same species as `getDefinedPackages()` returning `Object[]`
earlier in this lane** — a correct mechanism standing behind a cheaper one that
answers first. There the shadow was a lookup where a load belonged; here it is a
literal table where a query belonged. Both were present, reached, and inert.

## 3. Every module collection accessor was MUTABLE — 7 rows, one shape

Not in either record, and found only because the probe asked:

```text
getPackages().add("x.y")                    HotSpot UnsupportedOperationException
getPackages().iterator().remove()                   UnsupportedOperationException
getDescriptor().packages() / uses() / exports() /
  opens() / requires() / provides()   -- all six    UnsupportedOperationException
                                            CratonVM: all seven SUCCEEDED
```

`build_package_set` and `build_string_hash_set` hand back the live `HashSet`
they have just built. A caller could edit the VM's published view of the module
graph and pass it on, and nothing downstream could tell the edited set from a
real one. Fixed by wrapping each in `Collections.unmodifiableSet`, the same
statement `jca::provider_chain::wrap_unmodifiable` makes for
`Provider.getServices` and for the same reason — and carrying the same
`--synthetic-jdk` caveat, where that method is bound to the identity function
and the wrapper is inert.

## 4. `invokeExact` — 17 rows, and arity was not checked either

**Result: all 17 rows clean in both modes**, plus the three field-accessor and
constructor rows added later in the same probe.

The recorded item said the signature was not enforced. The measurement says
rather more than that:

```text
max = findStatic(Math, "max", (int,int)int)

  max.invokeExact(1L, 2L)          HotSpot WrongMethodType   CratonVM 0
  max.invokeExact((short)1,(short)2)                         2
  max.invokeExact((byte)1,(byte)2)                           2
  max.invokeExact('a','b')                                   98
  max.invokeExact(Integer,Integer)                           2
  max.invokeExact(1, 2L)                                     1
  (Object) max.invokeExact(1,2)                              2
  (Integer) max.invokeExact(1,2)                             2
  max.invokeExact(1,2)  [void site]                          no-throw
  max.invokeExact(1)                                         1
  max.invokeExact(1,2,3)                                     2
  ((Object) ctor.invokeExact(4))                             (a Box)

len = findVirtual(String, "length", ()int)

  len.invokeExact((Object) "abcd")  HotSpot WrongMethodType            4
  len.invokeExact((CharSequence) "abcd")                               4
  len.invokeExact((String) null)    HotSpot NullPointerException       0
```

`max.invokeExact(1)` answering `1`, and `max.invokeExact(1,2,3)` answering `2`,
are not conversions that should have been refused — they are calls with the
wrong NUMBER of arguments answering anyway. And `len.invokeExact((String)null)`
returning `0` is the same shape as `Field.getAnnotation(null)` earlier in this
lane: `0` is `length()`'s ordinary answer for the empty string, so the caller
cannot tell the failed call from a real one.

### Why it had not been enforced, and why that reason did not survive

The registration carried a warning: a strict ARITY check there once aborted the
VM and killed every Groovy `IndyInterface` call site ("internal error:
WrongMethodTypeException: expected 2 args, got 1"), because CratonVM's
`MH_KIND_*` adapters keep their inner target's descriptor.

That is true of `MH_DESC`, which is what such a check reads. It is not true of
the `type` field, which `mh_type_descriptor`'s own doc comment describes as
"the ADAPTED MethodType ... Combinators must chain off this so a stack of
adapters tracks arity correctly". The two are different encodings of one
concept, and the objection was recorded against the wrong one.

The check reads `type`, and the CALL SITE arrives through
`cratonvm_native_api::poly_call_site` — the channel built for `invoke`'s
collect-or-passthrough decision, now armed for `invokeExact` as well at all
three doors that arm it.

### What it is allowed to speak about

Adapters are still excluded. The argument above is a good reason to believe they
would survive a check; it is not a measurement, and the cost of being wrong is a
refusal of working code on Groovy's hot path. Four kinds are in — STATIC,
VIRTUAL, SPECIAL, CONSTRUCTOR — the ones `finish_method_handle` builds with the
two HotSpot adjustments it applies. `bindTo` and `asType` preserve the direct
kind and RESTAMP `type`, so a bound or retyped direct handle is checked against
its restamped signature, which is the correct one.

**The field accessors nearly went out for a reason the oracle refuted.** I read
`finish_method_handle`'s comment — "STATIC/CONSTRUCTOR/GETTER keep raw `desc`",
where VIRTUAL and SPECIAL get the receiver prepended — concluded that an
instance `findGetter` handle must therefore report `()int` where HotSpot reports
`(Box)int`, found `regression-suite/src/RJdkHandles.java:159` making exactly
that call, and excluded both kinds to avoid refusing it.

The premise was wrong. `findGetter`'s raw `desc` ALREADY names the receiver, so
there is nothing to prepend, and CratonVM renders `(Box)int`, `(Box,int)void`
and `()int` for an instance getter, an instance setter and a static getter —
byte-identical to the oracle. The comment describes a prepend that would be
redundant, not a signature that is short.

Both readings were plausible from the source. Only one survived
`ie.type.getter`, and it is the two-line probe row that separated them — not the
half hour of reading that produced the wrong one. **A comment about a mechanism
is not a statement about the value that mechanism produces.**

Every way of not knowing is an accept: unknown kind, no `type`, either
descriptor unparseable, either signature unrenderable — and the fabricated
`()V` that `finish_method_handle` writes when no descriptor was supplied, which
is a stand-in for "unknown" and would otherwise turn every such handle into a
throw at its first real call site.

## 5. `asType` was a stamp, and the value went missing

The narrowest finding, and the one with the largest blast radius, is on the
`invoke` side:

```text
(long) findStatic(Math,"max",(int,int)int).invoke(1, 2)
  HotSpot   2
  CratonVM  0
```

A well-formed call. No exception anywhere. `invoke` is specified as
`asType(callSiteType)` followed by an exact invocation, and a widening RETURN
conversion is part of that `asType`. The result was boxed as `Integer` against
the LEAF descriptor, and the coercion downstream could not match an `Integer`
against `J`, so it fabricated a zero.

The same mechanism sits behind `asType` itself: `mh_with_stamped_type` clones
the handle and writes the new `MethodType` into `type`, leaving `MH_DESC` — the
descriptor dispatch actually runs against — as the leaf member's. So
`max.asType((int,int)long).invokeExact(1, 2)` ran the leaf, produced an `int`,
and lost it the same way. `vm_exec::mh_strict_invokeexact`'s own comment names
this as its known blind spot ("already broken for every non-zero value ...
converts silently wrong except at zero into loudly wrong always"). This is the
half that closes it: the return value is now converted to the target type —
widening only, exactly JLS 5.1.2 — and boxed against the target rather than the
leaf.

## 6. The lane's own regression — found by the arms, and fixed on dev before this landed

**The bigger finding of this round, and it is about the acceptance, not the
code.** Running the three `regression-suite` arms — which the lane record's
acceptance had not done — turned up two red vectors that were MINE, pushed to
`dev` the day before:

```text
RJdkFailure.java:168   an array CNFE must name the element, not the descriptor:
                       [Lcom.cratonvm.absent.NoSuchClass20260731;
RExceptions.java:382   ...naming the element, not the descriptor, got:
                       [Lcom.cratonvm.absent.NoSuchClass20260812;
```

`Class.forName("[Lcom.foo.Missing;")` must throw
`ClassNotFoundException("com.foo.Missing")` — JVMS 5.3.3 creates an array class
from its ELEMENT type, so the name that reaches a loader, and therefore the
message that comes back, is the element's. Both vectors assert it for `[L...;`
and `[[L...;`, and both say so in their own comments.

**The fix in the tree is `39e2ded07`'s, not this lane's.** Another lane hit the
same two reds, diagnosed the same cause, and fixed it on `dev` inside the hour
this lane spent building and running arms. Theirs also carried the control this
lane had not run — a detached worktree at pristine `d17feaad2`, built from
scratch, failing both vectors on its own, which convicts the commit without
reference to any other binary. This lane's duplicate was backed out on merge.
The account below is kept because it is this lane's defect and this lane's
escape, and because the escape is the part worth reading.

The cause is the shape this whole campaign keeps producing. Stopping `forName`
from delegating an array descriptor to `loadClass` was CORRECT — the two doors
owe different contracts and
`L5-reflection-lane-complete-20260828.md` §4 records two wrong placements of
that guard before the right one. But **delegation was also what produced the
right message**: the loader saw the element name and reported it. Removing the
delegation removed a second thing it had been doing that nothing recorded it
was doing.

### Why no gate could have caught it

The lane's acceptance ran the gate set — 20 test binaries green — and pushed.
Every one of those gates is a registration count, a conformance manifest, a
flag declaration or a doc citation. **A message string inside an exception
raised by a native is behaviour, and only the corpus runs behaviour.** The two
instruments are disjoint:

| instrument | sees | blind to |
| --- | --- | --- |
| `cargo test` gates | registration / manifest / flag / doc CONTRACTS | every observable behaviour |
| the three arms | behaviour on 112 vectors | every contract above |

The in-tree note that says "the corpus arms are not the gates" had only ever
been written in one direction. Both directions have now cost a landed red.

### Attributing it took a control, not a hunch

Three vectors were red. `RJdkEnumerations` is documented known-red in
`HANDOFF-20260828-SCOPE.md`. The other two could have been anyone's, including
the six `dev` commits that had landed since. Running the same three on the
PRE-change binary, ABBA-interleaved 2x2, reproduced all three identically —
which absolved this round's `invokeExact` work and convicted the lane's own
earlier commit. Absolving yourself needs a control exactly as much as crediting
yourself does.

## 7. Left open, and why

**One row of 125 is left, and it is the one whose fix is not safe yet.**

* `cat.invoke("ab", (Object) Integer.valueOf(3))` is a `ClassCastException` on
  HotSpot and a `NoSuchMethodError` here. `invoke` is `asType(callSiteType)`,
  and the REFERENCE half of that conversion is a `cast` — an assignability
  question. `bindTo`'s own in-tree note records why that is not yet safe: the
  only predicate `NativeContext` offers is `is_subclass`, which answers FALSE
  for a fabricated stand-in against a real JDK interface, and `bindTo` sits on
  the Groovy-indy / SpEL / log4j paths where interfaces and subtypes are the
  norm. A false `ClassCastException` there refuses working code, which is worse
  than the wrong exception TYPE it would replace — and note that this row is
  already an error, not a silent wrong answer, which is what makes it the right
  one to leave.

  The PRIMITIVE half of the same conversion did land, because it needs no
  hierarchy at all: `invoke_narrowing_arg_refusal` refuses a call-site primitive
  that would have to narrow to reach the handle's declared parameter, which is
  what `(int) max.invoke(1L, 2L)` was answering `0` to.

  Whoever takes the reference half: `vm_exec::boxed_primitive_supertypes` is a
  MEASURED closed table of every transitive supertype of the eight wrappers,
  built for the `VarHandle` rule against the same hazard, and its two-halves
  design (a real hierarchy walk, with the table as the fallback that keeps an
  unresolved hierarchy from firing) is the pattern to copy.

**Also still open, from the wider surface rather than this probe:**
* **The adapter kinds are unchecked** — `insertArguments`, `asCollector`,
  `asSpreader`, `dropArguments`, `guardWithTest` and the rest. `type` is
  maintained for them (`mh_type_descriptor`'s doc comment is explicit that
  combinators chain off it, and `bindTo`'s INSERT path recomputes it), so the
  check would probably be correct there too. "Probably" is not a measurement,
  and the cost of being wrong is a refusal of working code on Groovy's hot
  path. It wants a lane that can run Groovy, Netty and Spring against it —
  the same three the return-type half was A/B'd on.

## 7b. A fix this lane wrote and then dropped, and why that was right

While verifying, this lane independently diagnosed and fixed the
`cratonvm/internal/ArrayListViewItr` refusal that killed its caller under
`--jdk-only` — the last mode defect in the corpus. L6 landed the same fix first
(`844c581fa`, `SnapshotItrRoute::ViewCollection`), so this lane's duplicate was
dropped on merge in favour of theirs.

**Theirs is better, and specifically it avoids a hazard this lane's version
had.** `real_snapshot_iterator` uses the array it is handed DIRECTLY when the
length already matches the count. This lane passed the view's own backing array,
and the route's `remove()` shifts that same array — so the cursor would skip the
element after every removal. L6's version copies first, and its record says so
in as many words. A 21-row probe did not expose it; reading the callee did.

**The lesson is about how the duplicate was missed, not about the fix.** This
lane DID check `dev` for duplicate work immediately before merging — by grepping
for `SnapshotItrRoute::ArrayListView`, its own name for the route. L6 called it
`ViewCollection`. **Grepping your own identifier only ever finds your own work;
check the SITE.** The right query was the mint site, `alloc_arraylist_iterator`,
which would have hit on the first try.

## 8. Verification

```text
probe: probes/L5ModuleInvokeSweep.java, 125 rows, both modes

                            differing rows        measured on
baseline                          38              a binary with none of this
  + Module fix                    23              cratonvm-modulefix
  + invokeExact / asType fix       2              cratonvm-mhfix
  + narrowing-argument refusal      1              cratonvm-l5final

three arms (cratonvm-l5final)          before this round
  strict (--jdk-only)   111 / 112          109 / 112
  all                   111 / 112          109 / 112
  core                   72 /  72           71 /  72

  The single remaining failure in the strict and `all` arms is
  `RJdkEnumerations`, dev's documented known-red (`a0168ed03`). The +2/+2/+1
  is exactly §6's two regressed vectors coming back. `RJdkHandles` -- the
  40-row MethodHandle vector, and the one the Groovy objection was about --
  passes with the strict `invokeExact` check on.
```

### The kill switch, exercised rather than asserted

`CRATONVM_MH_STRICT_INVOKEEXACT=0` — the name the return-value half of this rule
already used, reused rather than adding a second switch for one rule.

```text
                        default   =0
  ie.max.longArgs        WMTE      0        every refusal reverts
  ie.max.tooFewArgs      WMTE      1
  ie.len.nullRecv        NPE       0
  iv.max.longArgs        WMTE      0
  iv.max.retLong         2         2        <- the VALUE fix does not revert
```

That last row is the point of running the switch instead of asserting it. The
refusals are behind the flag; `box_return_against_target` is not, and should not
be. A fabricated zero in place of `2` is a wrong answer with no constituency —
there is no state of the world in which someone flipping off a strictness rule
wants it back. Had I not exercised the switch I would have written that it
reverts "the change", which would have been wrong in the one direction a field
revert cares about.

The remaining row is `invoke`'s reference-argument conversion (§7). The
`--synthetic-jdk` caveat on §3 applies: in that mode
`Collections.unmodifiableSet` is the identity function, so a probe there will
still report a mutable set, and that is a separate defect one layer down.

## Reproduce

```bash
cratonvm --java-home "$JDK" --jdk-only -cp probes/out L5ModuleInvokeSweep
```

**The probe IS in your checkout, and that is worth a sentence.**
`3b2901531` ("major doc consistency update before the release") untracked all
862 sources under `probes/`, so most lane records now cite probe paths that no
longer travel with the repository. `probes/` itself is not gitignored — only
`apps/` is — so this probe, and the eight test fixtures the same cleanup took
out, are tracked there normally. If you are reading a record whose probe you
cannot find, that is why, and `probes/` is where to put it back.
