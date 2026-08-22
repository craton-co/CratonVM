# The nine `MethodHandle` / `java.net.URI` divergences left open on 2026-08-21 — ALL FIXED, same day

## Status
**All nine CLOSED, and the three probes that carry them are now byte-for-byte
equal to HotSpot 25.0.3+9 across all 762 rows** — `MhVarargsNullProbe` 75,
`MhIdentityProbe` 39, `OpaqueUriProbe` 648. Nothing in any of the three
diverges any more, so a future diff against those oracles is a pure regression
signal rather than a list with known exceptions in it.

The rows were opened by the earlier same-day change (the retired
`two-new-fails-20260821-fullsuite-rerun` write-up), which fixed two Spring
FAILs and recorded these as measured-but-not-taken. They fell into four groups.

---

## A. The inexact `invoke` door could not see its call-site type — C03/C07/C09

`MethodHandle.invoke` compiles to `asType(callSiteType)` followed by an exact
invocation, and `asType` is where a varargs collector decides to WRAP the
trailing argument or pass it through: the shortcut is taken only when the
caller's trailing parameter type is assignable to the collector's array type.
So the same handle and the same runtime value give two answers:

```java
mh.invoke((String[]) null)   // passthrough -> Arrays.toString(null)     "null"
mh.invoke((Object)   null)   // collect     -> Arrays.toString({null})   "[null]"
```

A `NativeMethodRegistry` callback receives only `&[Value]`, and a `null` carries
no runtime type, so the shim had nothing to decide on.

**The channel.** `cratonvm_native_api::poly_call_site` — armed immediately
before the native call, TAKEN by the consuming native. Take rather than peek,
because a value left armed would be read by a later dispatch through a door
that never armed one, and a WRONG call-site type is worse than none. A miss
degrades to the permissive path, never to a wrong answer.

**Where it is armed matters, and the obvious place was the wrong one.**
`vm_exec.rs`'s signature-polymorphic block is the block with the descriptor in
hand, and arming it there changed nothing: the consuming native printed `None`
for all sixteen rows. That block is on the ClassId-resolved route.
`MethodHandle.invoke` from the interpreter arrives at
`vm/src/runtime/interpreter/invoke.rs`'s **stackless** path — the one whose own
comment already says "registered under the erased `Object[]` descriptor while
the call site carries a concrete one". Both are armed now; the stackless one is
the one that fires.

A VIRTUAL handle needs one more step: its call site names the receiver and
`MH_DESC` does not, so the two parameter lists have to be aligned before their
trailing types can be compared at all. The unit test that pins that alignment
(`the_receiver_is_stripped_from_a_virtual_call_site`) is what caught the helper
rebuilding `(java/lang/String)V` — `parse_descriptor_types` unwraps a class type
to its NAME, so rejoining its output yields a string that is not a descriptor.
Rows G10-G13 now exercise the path end to end.

**And it has to be TAKEN first inside the native.** `adapt_invoke_args` unboxes
wrappers, which can re-enter the interpreter and reach the poly block again —
and that block CLEARS the channel for every name it does not arm.

## B. Two places CratonVM was more forgiving than HotSpot — B06, I06

Both are now the same rule, in one function. `mh_entry_adapt` implements
`asType(callSiteType)` for the two doors where the call-site type is known:

* a **collector** requires at least N-1 arguments and otherwise gathers the
  trailing ones into a fresh array — unconditionally, so an already-packed
  array becomes the single ELEMENT of a new one and the element cast throws
  (B06), exactly as HotSpot does;
* a **non-collector** adapts exactly or throws. It never gathers. A fixed-arity
  handle handed two arguments is a `WrongMethodTypeException` on every JDK and
  was a silently gathered array here (I06, H02, H08, H10).

**Why this is a second function and not a flag on `collect_trailing_varargs`.**
That one is a REPAIR — it reshapes an argument list that arrived flat, and it is
deliberately permissive because it also serves adapter chains and internal Rust
callers whose arity it cannot vouch for (JRuby's `insertArguments`-built call
sites are the case it was written for). This one is a CONTRACT. Keeping them
separate is also what bounds the blast radius: the strict path is taken only for
`MH_KIND_STATIC` and `MH_KIND_CONSTRUCTOR`, and only without a bound receiver —
the kinds where `MH_DESC` is exactly the parameter list the caller supplies, so
the arity comparison means what it says. Every other kind, and every other door,
runs the code it ran before.

**One measurement corrected a wrong assumption mid-flight.** A CratonVM array
object's `class_id_of_object` resolves to its COMPONENT class, so a `String[]`
reports `java/lang/String`. Reading array-ness off that name inverted every
array row at once — `fa.invokeWithArguments(new String[]{"x"})` threw
`Cannot cast java.lang.String to [Ljava.lang.String;` for an argument that IS a
`String[]`, and B06's packed array sailed through the check it was supposed to
fail. Array-ness comes from `object_is_array` now, and the CCE message rebuilds
the array's own name around the component.

The cast check FAILS OPEN throughout: an unloaded target, an interface, an
unresolvable class all answer "castable". Every "no" is a
`ClassCastException` a caller did not get before, so only the ones the class
hierarchy positively contradicts are worth producing — the same rule, and the
same reason, as `NativeContext::aastore_element_assignable`'s "must never
produce a FALSE ArrayStoreException".

## C. `asType` adapted the receiver in place — I13/I14

`asType` and its twin `MethodHandles.explicitCastArguments` stamped the new type
onto the RECEIVER and returned it. Not a cosmetic identity difference: two
`asType` calls off one handle overwrote each other, so the FIRST result silently
acquired the SECOND's type (I36-I39). Both now return a slot-for-slot copy
through the `mh_clone_handle` the earlier change added for
`asFixedArity`/`asVarargsCollector`.

`newType == type` returns the receiver, which is the JDK's own first line
(`if (newType == type) return this`) and keeps the common no-op allocation-free
— compared on the DESCRIPTOR, since CratonVM mints a fresh `MethodType` per call
and a reference comparison would never fire.

## D. `URI.resolve` and `URI.normalize` were RFC 3986, not RFC 2396 — R13/R15

`java.net.URI` predates RFC 3986 and the difference is visible in five places.
All five were measured before any of them was implemented — the S-block and
N-block of `probes/OpaqueUriProbe.java`, 46 rows:

| | JDK | was |
| --- | --- | --- |
| an EMPTY reference | keeps the base's DIRECTORY, drops its query — `https://h/a/b?q=1` resolve `""` is `https://h/a/` | returned the base |
| a LONE FRAGMENT | the one arm that keeps both path and query | (correct) |
| a reference with a SCHEME, an AUTHORITY, or an ABSOLUTE PATH | taken VERBATIM, never normalized | normalized |
| `..` at the root | SURVIVES — `/a/../../x` is `/../x` | discarded |
| an authority-bearing base with an empty path | `https://h` resolve `""` is `https://h` | fabricated a `/` |

The `..` rule lives in `uri_remove_dot_segments_units`, which `URI.normalize`
shares, so both are fixed by the one change — and the JDK's own `removeDots`
says so in a comment ("DEVIATION: RFC2396 says .. can be removed even if it is
at the start") before declining to do it.

---

## Regression measurement

**A 1222-class slice of the Spring Framework suite, twice per class,
ABBA-interleaved**, same driver, same classpath, binary the only variable. The
slice is deliberately much wider than the previous change's 360: MethodHandle
dispatch is now CHECKED on two doors, so the exposure is every module that
builds handles at all — all of `spring-expression`, `spring-aop` and
`spring-beans`, plus every class in the tree naming a
`Uri`/`Url`/`Resource`/`Path`, `Script`/`Groovy`/`Kotlin`, `MethodHandle`/
`Handle`/`Invoke`/`Invoc`, `Proxy`/`Cglib`/`Aspect`/`Lambda`/`Reflect`,
`Bind`/`Convert`/`Bean`/`Method`/`Function`/`Record` or `WebClient`/`Codec`
concern.

The slice was swept TWICE — once against the binary that first carried the
change, and again from scratch against the final binary, because the
receiver-stripping fix landed after the first sweep and it moves a real
dispatch decision.

| sweep | | OK | FAIL | NORESULT |
| --- | --- | --- | --- | --- |
| first | baseline `0172b8c02` | 1218 | 2 | 2 |
| | with the fix | 1219 | 1 | 2 |
| final | baseline `0172b8c02` | 1217 | 3 | 2 |
| | with the fix | 1219 | 1 | 2 |

**Three rows differed across the two sweeps and all three are baseline-arm
flakes, not fixes.** Each was re-run three or four times on BOTH binaries and
scored identically every time:

| | in the sweep | on re-run, both binaries |
| --- | --- | --- |
| `InterfaceBasedMBeanInfoAssemblerMappedTests` | base 0/17 | 17/17 |
| `InstanceSupplierCodeGeneratorTests` | base 5/26 | 24/26 |
| `AspectJTypeFilterTests` | base 7/8 | 8/8 |

So the honest reading is **zero attributable differences across 1222 classes,
measured twice** — including `JRubyScriptTemplateTests`, whose
`insertArguments`-built call sites are what `collect_trailing_varargs`'
permissive triggers exist for and the thing most at risk from making a door
strict.

That every flake landed on the baseline arm is a property of the host, not of
the change: this box runs ~400 sessions and a loaded interval makes whichever
arm it hits look bad. It is also why a single sweep row is not a result here —
the re-run is the measurement.

The two originally-fixed Spring classes still pass on both binaries, three runs
each: `VariableAndFunctionTests` 16/16, `WebClientUtilsTests` 13/13.

## In-tree gates

Run on this branch and on a detached pristine `0172b8c02` in the same session,
same host, same toolchain, so the comparison is a set difference rather than a
recollection.

| gate | mine | pristine |
| --- | --- | --- |
| `cargo test --workspace --no-fail-fast` | 17 failing | 20 failing |
| `cargo test -p cratonvm-vm --lib --features synthetic-jdk` | 12 failing | the SAME 12 |
| `cargo test -p cratonvm-native-builtins --lib --features synthetic-jdk` | RC=0 | — |
| `cargo clippy --all-targets` on the three touched crates | 1 error | the SAME 1 error |

Nothing fails on this branch that does not fail on pristine. The workspace
delta runs the other way — three timing/JIT flakes
(`t1_gc_pause_budget_100k_objects_under_200ms`,
`test_pgo02_guarded_virtual_inline`,
`a_capturing_sam_call_site_dispatches_through_a_thunk`) fired on the pristine
arm and not on mine. The single clippy error is `never_loop` at
`native-builtins/src/net_uri_inet.rs:1414`, in a file this change never opens.

`rustfmt --check` reports a handful more diffs on the touched files than
pristine does (`lang_invoke` 55 vs 53, `net_phase_e` 69 vs 68, `native-api/lib`
64 vs 61, `poly_call_site` 1). The tree carries 2442 pre-existing fmt diffs and
formatting is its own CI job, so this neither blocks nor hides anything — noted
rather than papered over, and NOT worth a `cargo fmt` that would rewrite
thousands of untouched lines.

## Files

```
native-api/src/poly_call_site.rs       NEW — the call-site descriptor channel
native-api/src/lib.rs                  its module declaration
vm/src/runtime/interpreter/invoke.rs   arms it (THE door for MethodHandle.invoke)
vm/src/vm/vm_exec.rs                   arms it on the ClassId-resolved route too
native-builtins/src/lang_invoke.rs     mh_entry_adapt and its cast/refusal
                                       helpers, mh_with_stamped_type, and the
                                       entry doors' descriptors
native-builtins/src/net_phase_e.rs     uri_resolve_ref's five arms, the merge,
                                       and the un-clamped `..`
probes/{MhVarargsNullProbe,MhIdentityProbe,OpaqueUriProbe}.java + .expected.txt
```

## Repro

```bash
javac -d /tmp/cls probes/MhVarargsNullProbe.java probes/MhIdentityProbe.java \
                  probes/OpaqueUriProbe.java
for p in MhVarargsNullProbe MhIdentityProbe OpaqueUriProbe; do
  java -cp /tmp/cls $p > /tmp/$p.hs
  <cratonvm-bin> --java-home $JDK25 -cp /tmp/cls $p 2>/dev/null > /tmp/$p.cvm
  diff <(grep -v '^#' probes/$p.expected.txt) /tmp/$p.cvm
done
```

Diff on **stdout only** — CratonVM's tracing goes to stderr and `2>&1` puts it
in the diff.
