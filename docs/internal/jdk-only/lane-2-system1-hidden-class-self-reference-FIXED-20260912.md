# Lane 2 — a hidden class could not refer to itself: FIXED 2026-09-12

`java/lang/System$1`'s 28 rows were the last entry on lane 2's §4 blocker table
with a **VM defect** behind it rather than a design decision. The defect is
fixed. The rows are still not retired, and §6 says why.

Landed on `claude/l2-system1-hidden-class-20260912`, merged to dev.

## 1. What the blocker said

`lane-2-lang-values-RETIRED-20260911.md` §4:

> **`java/lang/System$1` (28), and this is new.** … every ACC_NATIVE method the
> carrier's bodies delegate to is registered here — checked first, 9 of 9. It
> fails elsewhere: real `System$1.defineClass` calls `ClassLoader.defineClass0`
> with `initialize = true` and the hidden-class flags, and this VM cannot
> initialize the resulting `MethodHandleProxies` class (`7 of 9 steps failed`).
> That error named no exception — it printed a raw heap pointer — so this wave
> also fixed the diagnostic in `vm/src/vm/vm_exec.rs`.

Two things in that paragraph turned out to matter more than they looked.

The first is that **the diagnostic fix is what made this findable.** Lane 2
replaced a raw heap pointer with `describe_throwable`, then retired the page
without re-asking the question the new diagnostic could now answer. Re-running
the same vector on the same corpus printed, immediately:

```
ClassFormatError: jdk/MHProxy1/RJdkProxyIface$Greeter/0x0:
  defineClass0: initialize after define failed for …/0x0:
  java/lang/NoClassDefFoundError: jdk/MHProxy1/RJdkProxyIface$Greeter
```

The class being defined is `…$Greeter/0x0`. The class not found is
`…$Greeter` — **the same class, without the suffix it is stored under.**

The second is that `7 of 9 steps failed` is not a VM message at all. It is
`RJdkProxyIface`'s own tally line, and that vector prints a per-step exception
class and message for each failure. The blocker was diagnosable from the
existing instrument; nobody ran it after the diagnostic landed.

## 2. The defect

A hidden class (JEP 371) is registered under `"<class-file name>/0x<counter>"`
and is deliberately in **no loader's namespace** — `set_class_hidden` is what
makes `find_class_by_name` and `Class.forName` unable to see it. That is the
point of the feature.

But its constant pool still carries its class-FILE name, and `this_class` plus
every self-naming `Fieldref` / `Methodref` resolve through that name. Nothing
taught the VM those are the same class, so **a hidden class referring to itself
raised `NoClassDefFoundError` naming the class that was doing the referring.**

`MethodHandleProxies`' generated proxy is the worked example, and its three
self-references are why this is two changes and not one. From `javap` of the
class HotSpot dumps with
`-Djdk.invoke.MethodHandleProxies.dumpClassFiles=true`:

```
static {};
   0: ldc           #13   // class RJdkProxyIface$Greeter      (the INTERFACE)
   2: putstatic     #15   // Field <ITSELF>.interfaceType
   5: return

jdk.MHProxy1.RJdkProxyIface$Greeter(Lookup, MethodHandle, MethodHandle);
   1: invokespecial #19   // Object."<init>"
   5: invokestatic  #23   // Method <ITSELF>.ensureOriginalLookup
  10: putfield      #25   // Field <ITSELF>.target
  15: ldc           #27   // MethodType (Ljava/lang/String;)Ljava/lang/String;
  17: invokevirtual #33   // MethodHandle.asType
  20: putfield      #35   // Field <ITSELF>.m0
```

`#15`, `#23`, `#25` and `#35` all name Class `#2`, which is `this_class`. Two of
them are reached before any method of the requested interface runs.

## 3. Two doors, two different reasons

**`resolve_class_loader_aware`** (`ldc`, `new`, `checkcast`, `instanceof`,
`anewarray`, and field owners via `field_access.rs`) did a name lookup that
cannot succeed for a hidden class. This is the one `<clinit>`'s `putstatic` hit,
and because it happened inside `defineClass0(initialize=true)` it surfaced as a
`ClassFormatError` **about the define** rather than about resolution — which is
why the blocker was recorded as a `defineClass0` failure.

**`dispatch_static`'s `self_class_id`** and **`invoke`'s `self_match`** recognise
a self-call so they can answer from the frame's own `ClassId` instead of a
loader-blind name lookup. Both did it by EXACT string equality:

```rust
.filter(|c| c.name.as_ref() == method_class_name.as_ref())
```

A hidden class satisfies neither half: not the equality (its stored name carries
the suffix) and not the fallback lookup (it is in no namespace). This is the one
`<init>`'s `invokestatic` hit, and fixing only the first door moved the failure
here rather than removing it — the second measurement below is that step.

Both now go through one pure predicate over the `"{original}/0x{id:x}"` shape
that all six mint sites write off `HIDDEN_CLASS_COUNTER` (`classloader.rs`,
`lookup_define.rs` ×2, `lang_system.rs`, `unsafe_natives.rs`,
`unsafe_natives_ext.rs`).

The constant-pool arm is guarded by a relaxed load of that counter, so a process
that has never defined a hidden class pays one branch per class resolution.

**The predicate is deliberately strict in the permissive direction**, and both
gates assert that half: `A/0x1$Inner` must not answer for `A`, a non-hex suffix
must not, and a class that is not hidden gets no shortcut at all. The bug being
fixed is a MISSING answer; a wrong answer would be worse than what it replaces.
`unsafe_natives_ext.rs` mints `<HOST>/0x<id>`, where the stored name belongs to
the host and not to the anonymous class's own class-file name — the predicate
simply does not match there, which is correct: that path has no self-reference
to rescue.

## 4. Measured

`regression-suite/src/RJdkProxyIface.java`, JDK 25.0.4+7, Azure Linux. The armed
arm is `CRATONVM_ENFORCE_NATIVE_SHADOW=java/lang/System$1`.

| arm | dev `46d7b7211` | fix 1 only | both fixes |
|---|---|---|---|
| HotSpot oracle | 38/38 PASS | 38/38 PASS | 38/38 PASS |
| compatible | 38/38 PASS | 38/38 PASS | 38/38 PASS |
| `--jdk-only` | 38/38 PASS | 38/38 PASS | 38/38 PASS |
| `--jdk-only`, `System$1` armed | 7 of 9 FAILED | 7 of 9 FAILED | **38/38 PASS** |

The middle column is the load-bearing one. After fix 1 the tally is *identical*
and the failure is not: it moves from a `ClassFormatError` raised by `define`
to a bare `NoClassDefFoundError` raised inside `<init>`, and the proxy counter
stops advancing per step (`MHProxy1` gets reused), which only happens once the
define and its `<clinit>` succeed. A run scored on pass/fail alone would have
called fix 1 worthless.

The stack trace, from a purpose-built minimal probe because the vector prints
only the exception class and message:

```
at jdk.MHProxy1.L2Sys1Min$Greeter.0x0.<init>(Unknown Source)
at java.lang.invoke.MethodHandleProxies.asInterfaceInstance(MethodHandleProxies.java:189)
```

Corpus, on the merged release build `e6064bf9e5a85c6b`:

| arm | before | after |
|---|---|---|
| `--jdk-only` | 136/136 | 136/136 |
| `SUITE=all` | 136/136 | 136/136 |
| `SUITE=core` | 95/95 | 95/95 |
| `--jdk-only` + `System$1` armed | **39/1 `RJdkProxyIface`** | **136/136** |

(§4's table calls this a 40-vector corpus. It is 136 vectors now. The 39/1 and
40/0 figures on that page are August/September numbers against a smaller list,
so they are not comparable to these as counts — only as pass/fail.)

Gate set, `docs/contributing/jdk-only-lane-operations.md` §5, on the merged tree
`852eebfa4`: `cratonvm-types` 15 targets / 0 failing; `native-builtins` default
11/1, management 11/1, synthetic-jdk 11/1, all three being
`raw_lock_constructions_do_not_grow`, which is red on pristine dev and belongs to
another lane; `cratonvm-vm --lib` 2673 passed / 1 failed, that one being
`ffm_group_layout_force_native_covers_member_layouts`, also dev's, whose
assertion is about a force-native GATE ENTRY for `ValueLayouts$OfLongImpl.carrier`
and which a class-resolution arm cannot reach.

One operational note, because it cost two hours. `--no-fail-fast` is the ops
page's own spelling and it is not decoration: the same
`cargo test -p cratonvm-native-builtins --tests` invocation reports **11**
targets with it and **5** without, because it stops at the first red TARGET and
`lock_discipline_ratchet` sorts fifth of ten. Every gate this change could
plausibly have moved lives after it. Separately, the management arm's lib target
hung for 2.5 hours: a `net_phase_e` test reports `ok` and leaves a server thread
blocked in `inet_csk_accept`, which then blocks the harness from exiting. All 80
`net_phase_e` tests had completed. Re-run alone the target is clean — 4296 passed
/ 0 failed — so it is a leaked thread and not a failure, but it reads as a
stalled gate. Filed.

## 5. Two things found on the way, neither of them this fix

**Compatible mode already passes this vector, and nothing was watching.** The
`ClassCastException` in `RJdkProxyIface`'s own header — the shim returning the
`MethodHandle` itself as the proxy — was fixed by the
`!drops_real_layout_synthetic()` guard in `lang_invoke.rs`. But `RJdkProxyIface`
lives in `JDKONLY_CLASSES`, so the corpus only ever runs it with `--jdk-only`,
and the mode this surface actually ships in has no arm at all. The fix has been
live and unverified; it is verified here as a side effect.

**`classloader.rs`'s `define_class_via_full` swallows the eager-init failure.**

```rust
if initialize {
    if let Err(msg) = ctx.initialize_class(cid) {
        tracing::warn!("defineClass0 initialize: <clinit> for {name} failed: {msg}");
    }
}
```

`initialize = true` is a contract; this warns and returns the mirror as though
`<clinit>` had succeeded. It is not the live path for `defineClass0` —
`lang_system.rs` registers that triple later and so owns the slot, and its
version propagates, which is the only reason the error in §1 was legible at all.
Left alone deliberately: changing a shadowed path's error behaviour is a separate
change with its own blast radius. Filed.

## 6. What this does NOT claim

**No row was retired.** `java/lang/System$1` stays out of
`RETIRED_SHADOW_L2_TRIPLES` and its guard in `retired_shadow.rs` still asserts
that it is not retired.

The enforcement dial **rejects a retirement and cannot promote one** — a leaked
dial row is not evidence, and `armed == control` is precisely what a promotion
looks like whether or not the retirement is safe. And §4's own warning applies to
the corpus result: *a clean arm means the corpus did not ask.* `java/lang/ref/`
is the campaign's worked example of a family that passed a 36-vector screen and
was then rejected by a 102-vector arm.

So the honest statement is narrow: **the named blocker is gone.** The page said
this VM cannot initialize the resulting `MethodHandleProxies` class; it can. What
these 28 rows need next is the two-binary score over the corpus, which is a
different piece of work from this one.

The fix stands on its own regardless: a hidden class could not refer to itself,
at any door, in any mode, and `MethodHandleProxies.asInterfaceInstance` is only
the caller that made it visible.
