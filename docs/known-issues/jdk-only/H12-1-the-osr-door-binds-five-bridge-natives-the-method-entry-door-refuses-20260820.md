# H12-1 — the OSR door binds five `bridge` natives under `--jdk-only` that the MethodEntry door refuses, and the comment saying that cannot happen names two gates, one of which was deleted three months ago

**Status: MEASURED (the defect) / FIXED-UNVERIFIED (the fix).** The divergence
is a measurement, not an argument: one command, three arms, printed counters.
The fix is one commit in `vm/src/jit/helpers.rs` and **no binary carrying it has
been built or run**.

**Provenance.** Runs are MEASURED on
`C:/craton/target-jdkonly-h2/release/cratonvm.exe`, built at `fe59bf9d9` —
which is this worktree's HEAD before my commit, so the runs describe the tree
*including* lane H7's merge and *excluding* my change. Oracle is
`/c/Program Files/Microsoft/jdk-25.0.3.9-hotspot` (JDK 25.0.3+9), resolved via
`dirname $(dirname $(command -v javap))`, not copied from a record. Source
claims are ARGUED and each names its file and line. Registry kinds are quoted
from `scripts/baselines/jdk-only-kind-map-25-linux.tsv`, a checked-in
measurement, not a live registry dump.

Worktree `C:/craton/cratonvm/.claude/worktrees/agent-a0d4c411522945622`, branch
`claude/jdk-only-mode-handoff-09b48c`. Lane H12, 2026-08-20.

**Worktree gap.** My worktree was cut at `26e4b5db4`; the branch tip was
`fe59bf9d9`. `git merge --ff-only` brought in the whole H-wave: the H0-2/H0-3/
H0-4/H0-5, H1-1, H2-1, H3-1, H4-1, H5-1, H6-1, H7-1 and H8-1 records,
`HANDOFF-20260820.md` itself, **lane H7's four commits to `vm/src/jit/helpers.rs`
and `jit/src/lib.rs`** (`0e5f3807a`), and the `probes/SWT*` files. Without the
merge I would have audited a `helpers.rs` predating the very fixes I was told to
build on. Four of four lanes have now hit this.

**LOUD NOTICE FOR LANE H10.** §6 changes JIT behaviour under `--jdk-only`: five
thin direct-call helpers now decline and route to the generic dispatcher. Any
`RJitMapTierDiff` expected values, and any counter baseline, must be re-taken
against a binary carrying commit `9b7ad0f07`. §7 is the vector specification you
asked for.

---

> **VERIFIED AGAINST A BINARY 2026-09-04. The acceptance measurement passes,
> and the door is closed in strict mode only.** Status was *"MEASURED (the
> defect) / FIXED-UNVERIFIED (the fix) … no binary carrying it has been built or
> run."*
>
> `probes/OsrDoor.java` is the INDEPENDENT REPRODUCTION probe transcribed from
> this record: one `static long hot()` called **exactly once**, containing a
> 300,000-iteration loop over `Thread.currentThread().hashCode()`. Called once so
> the MethodEntry door cannot tier it; 300,000 iterations so OSR must.
>
> ```text
> CRATONVM_INTRINSIC_STATS=1 cratonvm <mode> -cp . OsrDoor
>
> mode                    Thread.currentThread direct calls      OSR    single-pass
>                          recorded (fe59bf9d9)   measured now
> --jdk-only                  298,000                   0         1        0/0
> --real-jdk                  298,000             298,000         1        0/0
> --jdk-only --nojit                0                   0         0        0/0
> ```
>
> **`--jdk-only` goes to zero and `--real-jdk` is unchanged — to the digit.**
> That is exactly the acceptance both this record and `H20-1` §4 specify.
>
> **The fix closes the bind, not the door.** `OSR 1` is still reported under
> `--jdk-only`: the method is still OSR-compiled, and what stopped is the
> binding of the `bridge` native. That distinction is the whole point of
> `H20-1`'s title — filtering downstream "would have traded an open door for a
> wild jump" — and the counters show the open door closed without the jump.
>
> **Determinism was checked, and a single run would have understated it.** Three
> runs per arm: `--jdk-only` 0/0/0, `--real-jdk` 298,000/298,000/298,000. The
> FIRST `--real-jdk` run of the session reported 297,000. One run of a debug
> binary is not the number; three are, and three reproduce this record's figure
> exactly.
>
> **One difference from the recorded table, named and NOT explained.** This
> record's `--jdk-only` row shows single-pass **`0/7`** — *"seven `invokestatic`
> sites examined in strict mode, seven refused"* — and it leans on that
> denominator to make its point that auditing only the MethodEntry door reads as
> reassuring. Today that column reads **`0/0`** in every arm: zero sites
> examined, not seven refused. Whether the single-pass ladder no longer reaches
> those sites, or the tiering path changed underneath, is **not determined
> here**. The 298,000/0 result does not depend on it, but the record's §3 point
> about the two doors disagreeing is now resting on a denominator that has moved.
>
> **What this does NOT verify.** §4 is titled *"What I could NOT witness: a
> wrong value"* and that is still true — this probe's answer agrees with HotSpot
> in every arm, so the exposure was latent before and is closed now, but no
> miscompile was ever witnessed and none is witnessed here. §§7-9 (the H12-C arm
> for lane H10, the out-of-file edits, the nominations) are untouched.

## 0. What the assignment said, and the one word in it that was wrong

`HANDOFF-20260820.md` §7 item 4 calls this blocking:

> Six JIT direct helpers in `vm/src/jit/helpers.rs` reimplement these natives,
> so a tier-dependent wrong answer is possible that **no arm diffs for**.

Lane H7 corrected "reimplement" (they do not — five call the registered
function, the sixth is a re-export) and then corrected "past the kind check"
(four are refused at **bind** time, one crate away). Both corrections are right
about the code they looked at.

**The predicted failure mode is nevertheless real, and it is live today.** H7
looked at the ladders inside `jit/src/lib.rs::try_compile_inner`. There are two
more ladders, in two other files, belonging to the other two compile doors, and
neither asks any policy question at all.

H7-1 §6b states the opposite in one sentence:

> A method that reaches the optimizing tier or is OSR-compiled therefore never
> binds a collection helper.

That is the sentence this record falsifies, and §2 gives the file and §3 the
measurement.

---

## 1. STANDING TRAP 1 first: the tree does know, and what it knows is wrong

Before asserting a missing check I grepped for one. There is a 23-line comment
at `vm/src/jit/helpers.rs:12289` (pre-change) whose entire purpose is to explain
why these bodies are unguarded. It is not an oversight; it is a *claim*, and it
is the reason this defect survived three lanes looking at these helpers.
Verbatim:

```text
// Each is a VM-side reimplementation of a registered native, baked straight
// into the emitted `CALL` — no dispatch helper, and so no policy check, on the
// path. None of them carries an internal JDK-only check, deliberately: under
// `JdkOnly` they are unreachable, gated twice and both gates upstream of any
// code that could execute.
//
//  1. `build_helpers` does not register their addresses at all under
//     `JdkOnly`, so the `*_DIRECT_FN` cells stay `0` — the established
//     "not wired, use the generic dispatch helper" sentinel.
//  2. `jit::direct_native_helper` refuses to bind a non-zero address once
//     `set_jit_execution_policy` has latched strict, and records a
//     `NativeShadowsBytecode` violation when it does.
//
// Adding a third, per-invocation check inside these bodies would put a policy
// read on the hottest boxing/collection paths in the VM to defend against a
// state that cannot occur. If either gate above is ever removed, this comment
// is the reason these bodies look unguarded.
```

That last sentence is the contract. Both gates are gone or partial.

### 1a. Gate 1 has not existed since 2026-08-06 — ARGUED, from the other crate's own comment

`jit/src/lib.rs:8528`, in the module block governing exactly this feature:

> * the `*_DIRECT_FN` helper addresses are registered **unconditionally**
>   (2026-08-06) — they are process-invariant Rust `fn` pointers, so
>   withholding them was never per-VM protection — and the bind decision is
>   threaded per compilation as an argument instead;

The same file's JDK-ONLY-NOTE, item 3 (`jit/src/lib.rs:8823`), still lists
withholding them as an **open ask**:

> `vm/src/jit/helpers.rs::build_helpers` — … and **should** skip the
> `set_*_direct_fn` registrations entirely under `JdkOnly` (belt and braces:
> this crate already refuses to bind them, but not registering them at all makes
> the refusal unreachable rather than merely correct).

So one file says gate 1 was deleted, another says it is still wanted, and a
third asserts it as a live protection. This is
`a-premise-in-a-comment-is-not-a-compile-time-link` in its purest form: the
premise was deleted by a commit that never visited the comment depending on it.

### 1b. Gate 2 exists, works, and covers one door of three — ARGUED

`direct_native_helper` / `direct_native_helper_for_impl` are defined at
`jit/src/lib.rs:8698` / `8768`. Every call site is inside `try_compile_inner`
(`17756`, `17794`, `17816` — the IR ladder; `19460`, `19499`, `19548`, `19581`,
`19615`, `19943`, `19978`, `20053`, `20075` — the single-pass ladder), plus the
crate's own tests at `22267`+. There is no other caller in the repository.

`try_compile_inner` is reached only from
`try_compile_with_invokespecial_resolver`. That is one door.

---

## 2. There are three doors, the tree says so in a table, and two of them have their own ladders

`jit/src/compile_gate.rs`'s module doc is titled **"There are THREE doors, not
two"** and carries this table verbatim (`compile_gate.rs:20-24`):

| Door | Where | Reaches the backend via |
|---|---|---|
| `CompileDoor::MethodEntry` | `try_compile_with_invokespecial_resolver` | `try_compile_inner` → `x64::compile_with_param_slots` |
| `CompileDoor::EagerFirstCall` | `execute`'s first-call compile (`vm/src/runtime/interpreter.rs`) | `x64::compile_with_param_slots` **directly** |
| `CompileDoor::Osr` | `compile_osr_artifact` (`vm/src/runtime/interpreter/jit_bridge.rs`) | `x64::compile_with_param_slots` **directly** |

and its next paragraph says why this keeps happening:

> Only the first went through the admission checks. The other two grew
> *hand-copied* subsets of them, each added reactively after its own bug.

`compile_with_param_slots` takes `direct_calls: Vec<(usize, JitDirectCall)>`
(`jit/src/x64/driver.rs:309`) as **data**. A door that builds that vector itself
never goes near `direct_native_helper`. Both direct doors build it themselves:

* **OSR** — `vm/src/runtime/interpreter/jit_bridge.rs`, `direct_calls2`,
  declared at line 687, eleven `push` sites (855, 891, 912, 961, 1006, 1028,
  1061, 1110, 1136, 1173, 1507), handed to `compile_with_param_slots` at 1965.
* **EagerFirstCall** — `vm/src/runtime/interpreter.rs`, `direct_calls_early`,
  declared at 2483, push sites at 2581/2611/2635, handed over at 3207.

### 2a. What the OSR ladder binds, and its registry kind

Each row is the raw helper address, taken as
`crate::jit::helpers::NAME as *const () as usize` — **not** a read of the
`*_DIRECT_FN` cell. Kinds are MEASURED from the checked-in baseline
(columns are `class name descriptor ordinal kind kind_stated kind_chosen`).

| jit_bridge.rs | triple | helper | kind |
|---|---|---|---|
| 959 | `java/lang/Thread.currentThread()Ljava/lang/Thread;` | `jit_thread_current_thread_direct` | **`bridge`** |
| 1006 | `jdk/internal/util/Preconditions.checkIndex(IILjava/util/function/BiFunction;)I` | `jit_preconditions_check_index_direct` | **`bridge`** |
| 1028 | `java/lang/ref/Reference.reachabilityFence(Ljava/lang/Object;)V` | `jit_reachability_fence_direct` | **`bridge`** |
| 1061 | `java/lang/Integer.valueOf(I)Ljava/lang/Integer;` | `jit_integer_value_of_direct` | `intrinsic` — permitted |
| 1136 | `java/lang/Integer.intValue()I` | `jit_integer_int_value_direct` | `intrinsic` — permitted |
| 1173 | `java/util/HashMap.put(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;` | `jit_hashmap_put_direct` | **`bridge`** |
| 1173 | `java/util/HashMap.get(Ljava/lang/Object;)Ljava/lang/Object;` | `jit_hashmap_get_direct` | **`bridge`** |

**Five `bridge` rows bound at the OSR door with no policy question.** The
MethodEntry door refuses all five (`direct_native_helper` admits only
`Intrinsic` under `jdk_only`).

The two `Integer` rows are `intrinsic`, so binding them is inside §1.4's
reviewed exception. That is a coincidence of the current tagging, not a property
of the door — the door asks nothing — and it is the same species H7-1 §4a
identified one layer up.

### 2b. The bypass is deliberate, and its own comment says why

`jit_bridge.rs:952`, on the `Thread.currentThread` bind:

> Address taken directly, for the same reason the `Integer.valueOf` bind below
> states: `build_helpers` registers the jit-crate atomic only AFTER this
> construction block, so reading it here would give 0 on the first OSR compile
> in a process.

So gate 1 could not have covered this door **even when gate 1 existed**: the
door deliberately does not read the cell gate 1 was supposed to zero. The two
protections named in §1's comment are a gate that was deleted and a gate this
door was written to avoid.

### 2c. The EagerFirstCall door — unguarded, but not currently wrong

`direct_calls_early` binds only `Math.sqrt` (a true inline intrinsic, not a
registered-native shadow), `Integer.valueOf(I)` and `Integer.intValue()` — both
`intrinsic` — plus the elidable-`<init>` rewrite. So this door is **incidentally
correct today and structurally unguarded**: it asks no policy question, and the
only reason it is not a second live divergence is that nobody has yet added a
`bridge` helper to it. It is the fourth-door hazard already realised as a third.

---

## 3. MEASURED — the divergence, three arms, one command

`--jdk-only` does **not** disable the JIT: there is no coupling between the
execution policy and JIT enablement (`grep -rn 'jdk_only' vm-cli/src vm/src/vm/config*`
finds nothing touching JIT enablement; `jit_bridge.rs` supplies `jdk_only` to
`try_compile_with_invokespecial_resolver` at 4731/4926/6313, and the OSR builder
at 1949 does not receive it at all). So the OSR door runs in strict mode.

### 3a. The instrument, and why this probe and not the obvious one

There is **no counter on the OSR door's HashMap binds** — H7-1's
`HASHMAP_GET_DIRECT_SITES` / `CONCURRENT_HASHMAP_GET_DIRECT_SITES` live in
`jit/src/lib.rs`'s ladder and are therefore blind to the door where the bind
actually happens. H7-1 §6c's prediction that
`collection_direct_helper_sites() == (0,0,0)` under `--jdk-only` is consistent
with the door binding freely; the instrument cannot see it. **That is the
instrument reporting its own reach.**

There *is* a counter on the OSR door's `Thread.currentThread` bind —
`cratonvm_jit::THREAD_CURRENT_THREAD_SITES_OSR` (`jit_bridge.rs:959`), reported
per door by `CRATONVM_INTRINSIC_STATS=1` (`vm-cli/src/main.rs:5091`). So
`Thread.currentThread` is the discriminator: same door, same ladder, same
absence of a policy question, and a printed number.

Probe `H12Door.java` — a loop inside a method invoked **once**, so its body can
only be compiled by the OSR door; the loop contains
`invokestatic Thread.currentThread()` and exact-`HashMap` `get`/`put`.

### 3b. The three arms

```
$ CRATONVM_INTRINSIC_STATS=1 cratonvm.exe --jdk-only -cp . H12Door 300000
acc=44999850000
[cratonvm] compiled Thread.currentThread direct calls: 298000
  (sites bound per compile door: single-pass 0/7, IR 0/0, OSR 1; …)

$ CRATONVM_INTRINSIC_STATS=1 cratonvm.exe --real-jdk -cp . H12Door 300000
acc=44999850000
[cratonvm] compiled Thread.currentThread direct calls: 298000
  (sites bound per compile door: single-pass 0/0, IR 0/0, OSR 1; …)

$ CRATONVM_INTRINSIC_STATS=1 cratonvm.exe --jdk-only --nojit -cp . H12Door 300000
acc=44999850000
[cratonvm] compiled Thread.currentThread direct calls: 0
  (sites bound per compile door: single-pass 0/0, IR 0/0, OSR 0; …)
```

Three facts, all MEASURED:

1. **`single-pass 0/7` under `--jdk-only`.** The MethodEntry door examined seven
   `invokestatic` sites and bound zero. Gate 2 is real and it fired. This is the
   positive control for the guard: it is not a ban that does not exist
   (`verify-the-jit-ban-exists-before-ab-testing-it`).
2. **`OSR 1`, and 298 000 executions.** In strict mode, compiled code entered a
   CratonVM native 298 000 times on a triple the same binary refuses to bind at
   the other door, and whose registry kind is `bridge` — the kind `--jdk-only`
   exists to make defer to real `java.lang.Thread` bytecode.
3. **`--real-jdk` reports the identical `OSR 1` and the identical 298 000.** The
   OSR door does not know which mode it is in. That equality is the cleanest
   single statement of the defect: the strict arm and the compatible arm compile
   the same binding.

The `--nojit` arm is the negative control and is all zeros.

Unexplained and reported as observed, not theorised: the single-pass denominator
is `0/7` under `--jdk-only` and `0/0` under `--real-jdk`. The numerator is 0 in
both, which is the part this record depends on.

---

## 4. What I could NOT witness: a wrong *value*

Being precise, because "route diverged" and "answer diverged" are different
claims and this directory has been burned by merging them.

`H12Osr.java` (three key shapes — `Integer`, `String`, a custom `hashCode()` —
1024 keys, 200 000 iterations, plus a full cold re-read) and `H12Split.java`
(one map object, `HashMap`-declared receiver for `put`/`get` so the OSR ladder
binds it, `Map`-declared receiver for a second `get` so that one takes the
policy-checked dispatcher) both produce **byte-identical stdout** on HotSpot
25.0.3+9, on `--jdk-only`, and on `--jdk-only --nojit`:

```
hot.missViaInterface=0   hot.missViaExact=0   acc=159999600000
cold.miss=0   cold.sizeMismatch=0   cold.size=4096
```

**So: MEASURED, no value divergence on these inputs.** I did not witness the
wrong answer; I witnessed the wrong route.

And I can say why the routes agree here, which matters more than the null
result — ARGUED, `native-collections/src/lib.rs:9222`:

```rust
ctx.resolve_field_index_by_class_id(class_id, "table")
```

`native_hashmap_get_exact` walks the **real `java/util/HashMap.table` field**
and real `HashMap$Node` objects. It is a re-implementation over the *same
representation* the JDK bytecode uses, not a side table. **This is a correction
to `H4-1`'s predicted failure mode** for this particular door: H4-1's "silently
empty map" argument is about producers that mint a `cratonvm/internal/*` carrier,
and it does not transfer to the OSR HashMap binds, which share state with the
bytecode. That is why my probes agree, and it is why a probe that only checks
`get` returns the value `put` stored will *never* fail here.

What remains untested, and I did not establish it either way: the overlay fast
paths (`try_hm_int_fast_get` / `try_hm_int_fast_put` /
`materialize_hm_int_fast`) are a genuine second representation, and I have **no
evidence about whether they were active during any of my runs**. A vector that
does not force the overlay is not testing the surface that can diverge. §7.

The divergence surface that is left is therefore not "different storage" but
"two implementations of one contract" — precisely the population H7-1 §2
documents three members of, one of which (`jit_hashmap_put_direct` inserting and
then re-dispatching the same put, returning the value it had just written as the
previous mapping) **was live until `0e5f3807a`, and the OSR door is the only
door in strict mode that could have executed it.** That is the strongest
available statement about this class: it is not hypothetical, it has already
happened in this exact helper, and it was invisible because no arm ran the
compiled tier against the interpreted one.

---

## 5. H12-A — the latent guard, and why the fix is not where the brief expected

The brief asked me to make the "correct only because everything is tagged
`bridge`" condition non-latent by making the guard read the implementation's
interface. **Lane H7 already landed that**:
`jit/src/lib.rs::direct_native_helper_for_impl` (8768) requires both the
call-site row and the implementing row to be admitted, and is wired into the
`ConcurrentMap.get` ladder (19978), the `Map.get`/`HashMap.get` arm (20075) and
the `HashMap.put` arm (20053). I verified all three call sites and found no
remaining ladder in `try_compile_inner` where the site class differs from the
implementing class and the plain `direct_native_helper` is used. **H12-A as
specified is closed, by H7, and I confirmed it rather than redoing it.**

What is *not* closed is the premise underneath it. `direct_native_helper_for_impl`
is a correct guard reached by one door in three. Making it stricter improves the
door that already refuses; it does nothing for the door that does not ask. A
retag of the map/set cluster is safe at MethodEntry and unguarded at OSR either
way.

So the landmine the brief describes is real but its blast radius is the opposite
of the one stated: it is not that a retag would *open* a door, it is that a door
is already open and a retag would change what walks through it.

---

## 6. What I changed — commit `9b7ad0f07`, one file

I own `vm/src/jit/helpers.rs`. The two ladders that need the bind-time fix are
in `jit_bridge.rs` and `interpreter.rs`, which I do not own and six concurrent
lanes are working near; those are specified as out-of-file edits in §8 rather
than applied.

What *is* fixable inside my file is the callee side, and it has a property the
bind-time fix does not: **it cannot be forgotten by a door that has not been
written yet.** `compile_gate.rs`'s own history — three doors, three independent
rediscoveries of one admission list — is the argument for putting the check in
the callee.

| helper | kind | change |
|---|---|---|
| `jit_thread_current_thread_direct` | `bridge` | decline → existing cold arm (`jit_invoke_dispatch` + `THREAD_CURRENT_THREAD_INFO`), which its own doc calls "byte-for-byte the route the site took before this helper existed" |
| `jit_hashmap_get_direct` | `bridge` | decline → `break 'fast` → `jit_invoke_dispatch` + `HASHMAP_GET_DIRECT_INFO` |
| `jit_hashmap_put_direct` | `bridge` | as above, `HASHMAP_PUT_DIRECT_INFO` |
| `jit_concurrent_hashmap_get_direct` | `bridge` | as above — a backstop, not a live fix (no door binds it today) |
| `jit_preconditions_check_index_direct` | `bridge` | decline → the generic dispatcher its throwing case already uses |
| `jit_integer_value_of_direct`, `jit_integer_int_value_direct` | `intrinsic` | **deliberately NOT guarded** — §1.4's reviewed exception, the one kind `direct_native_helper` also admits |
| `jit_reachability_fence_direct` | `bridge` | **deliberately NOT guarded** — see below |
| `jit_string_latin1_to_lower_direct` | `intrinsic` | not guarded; not bound by either direct door |

Every refusal uses a fallback route the helper **already had** and already takes
for every non-exact receiver, so the change adds no new path — it re-uses an
exercised one. That is the whole reason I was willing to write it without a
build.

`jit_reachability_fence_direct` is declined for a stated reason rather than
skipped: it takes `_vm_ptr` (unused), has no dispatch fallback, and its
registered native is `black_box(arg); Ok(None)`. Guarding it would mean
inventing a synthetic `JitInvokeInfo` and a new dispatch path for a body whose
observable behaviour is identical to the thing it shadows. It is a contract
violation with no reachable consequence; it is nominated (N3), not patched
blind.

Refusals are counted (`JIT_DIRECT_HELPER_JDK_ONLY_REFUSALS`,
`jit_direct_helper_jdk_only_refusals()`) with the doc stating the 0/0 ambiguity
explicitly, because `0 refusals` and `0 binds` read identically and that is the
failure this whole record is about. In the map helpers the check sits **after**
the receiver screens so the counter means "a call the fast path would otherwise
have served", not "a call that entered the helper".

I also rewrote the §1 comment to say what is true, with the measurement in it.

### 6a. What I did NOT verify — the honest list

* **It does not compile.** I was forbidden to build and did not. I checked brace
  balance structurally and that no CRLF entered the file; that is all.
* **Verdict-neutrality is unmeasured.** The acceptance bar is verdict-neutral,
  not green: `--jdk-only` 104/104, `SUITE=all` 99/104 with the same five,
  `SUITE=core` 63/64.
* **The per-call cost is unmeasured.** The §1 comment's objection to a
  per-invocation policy read is not answered by this record, only overruled on
  correctness grounds. `dispatch_policy(vm)` is `shared.config.execution_policy()`
  (`vm/src/vm/vm_exec.rs:1101`) — a field read, no lock, no allocation — and in
  the map helpers it sits behind an object-address probe and a class comparison
  that already dominate it. That is an argument, not a number.
* **`--real-jdk` throughput** is where any cost would land, since strict mode
  now takes the dispatcher anyway. The A/B must be on **one binary** with
  `CRATONVM_JIT` toggles, not two builds.
* Possible clippy lint: I did not run clippy. The `jit_thread_current_thread_direct`
  guard is a flat early check specifically to avoid adding nesting.

---

## 7. H12-C — the arm that would catch this, specified for lane H10

H7-1 N1 nominates `RJitMapTierDiff` and describes it as a cold/warm answer
diff. §4 above says why that shape, as described, would have passed on the day
it was written: for `HashMap` the native and the bytecode share the `table`
field, so a vector that stores and reads back agrees in both tiers.

Four changes make it discriminating. Named here so H10 can take them.

1. **The hot region must be inside a method invoked ONCE.** This is the whole
   trick and it is not optional. A loop in a method called many times is
   compiled by the MethodEntry door, where the guard already refuses and the
   vector measures nothing. `H12Door.java`'s shape — `main` parses an argument
   and calls `hot(n)` exactly once — is what puts the body on the OSR door.
   Memory's `a-benchmark-loop-in-main-measures-interpreted-code` is the adjacent
   trap; this is its OSR twin.
2. **The receiver must be declared `HashMap`, not `Map`.** The OSR ladder
   matches `invoke_kind == 0 && target_class == "java/util/HashMap"`
   (`jit_bridge.rs:1156`). A `Map`-declared receiver is not recognised at that
   door at all, so it silently tests nothing. `javap -c` and grep for
   `invokevirtual .*java/util/HashMap` before believing the vector binds.
3. **It must force the OVERLAY, which is the only genuinely separate
   representation.** `try_hm_int_fast_put` / `try_hm_int_fast_get` /
   `materialize_hm_int_fast` in `native-collections/src/lib.rs` are the second
   store; the `table` walk is not. I did not establish whether my probes reached
   the overlay at all, and a vector that inherits that gap inherits the null
   result. Include `Integer` keys **outside** `-128..=127` (outside the identity
   cache) and interleave a `Map`-declared read of the same object, which under
   `--jdk-only` takes real bytecode against the real `table`.
4. **It must include the argument shapes where the two implementations differ**,
   not just the ones where they agree. From H7-1 §2, which found three: a key
   the heap does not recognise (`§2a`); a stored non-object `Value` (`§2c`, the
   double-apply arm); a key whose `hashCode()` allocates or triggers GC
   (`§2b`, the funnel). Plus a `null` key, and a `put` whose return value —
   *the previous mapping* — is asserted, not just the subsequent `get`. §2c was
   a wrong `put` return with a correct map state; a get-only vector cannot see
   it.

**The assertion.** Not "the answer is right" — HotSpot is the oracle via
`run.sh`'s stdout diff, so print the sequence. The vector's own job is to make
the two tiers run the same operations: N operations before the loop tiers up
and the same N after, with both answer sequences printed. `run.sh` diffing
stdout against HotSpot then catches a divergence in either tier.

**The counter it should be read beside.** `THREAD_CURRENT_THREAD_SITES_OSR` is
today the only per-door bind counter that reaches the OSR door. Until N2 lands,
a `RJitMapTierDiff` that prints `OSR 0` for its own binds is unfalsifiable —
put a `Thread.currentThread()` call in the same loop purely as a door witness,
exactly as `H12Door.java` does, so a green run can be told from an unreached one.

Reminder from §6: expected values must be re-taken against `9b7ad0f07`, on which
the strict arm's answers come from the dispatcher rather than the helpers.

---

## 8. OUT-OF-FILE EDITS REQUIRED

None for what landed; the commit is self-contained.

The following are the *proper* fix — a bind-time refusal costs nothing at run
time, where my callee-side check costs one field read per call. Both are in
files I do not own and six lanes are working near.

**O1 — `vm/src/runtime/interpreter/jit_bridge.rs`'s OSR ladder must ask the
policy before binding.** Five `bridge` rows, listed in §2a. The door has
`shared` in scope (it reads `shared.classes.class_manager` at the
`AtomicInteger` arm, line ~1085), so `crate::vm::dispatch_policy(shared)` is
available without a signature change. The minimal correct shape is the one the
MethodEntry door uses: consult the registry's `NativeKind` and admit only
`Intrinsic`, recording a refusal via
`cratonvm_jit::record_jdk_only_direct_native_refusal` so the refusal is counted
rather than silent. Leaving the two `Integer` rows bound is correct and should
be explicit, not incidental. **Add a per-door site counter for the HashMap binds
while you are there** — the absence of one is why H7-1 §6c's prediction was
unfalsifiable (N2).

**O2 — `vm/src/runtime/interpreter.rs`'s eager first-call ladder needs the same
question**, even though its two current binds are `intrinsic` and therefore
correct. §2c: it is unguarded, not safe. The next helper added there is a live
defect on the day it is added, and nothing will say so.

**O3 — `jit/src/lib.rs`'s JDK-ONLY-NOTE list is missing its most important
item.** The six numbered obligations at `8800` enumerate paths from compiled
code into natives that skip `resolve_dispatch`, and **not one of them mentions
the OSR or EagerFirstCall direct-call ladders** — the two largest such paths.
Item 3's belt-and-braces ask (skip `set_*_direct_fn` under `JdkOnly`) should
also be marked as what it now is: insufficient, because neither direct door
reads those cells. Suggested item 7:

```text
//  7. `vm/src/runtime/interpreter/jit_bridge.rs`'s OSR direct-call ladder and
//     `vm/src/runtime/interpreter.rs`'s eager first-call ladder each build
//     `direct_calls` themselves and hand it to `x64::compile_with_param_slots`,
//     so neither reaches `direct_native_helper`. Both take helper addresses as
//     `NAME as *const () as usize` rather than reading the `*_DIRECT_FN` cell,
//     so item 3's belt-and-braces would not cover them either. MEASURED
//     2026-08-20: under `--jdk-only` the OSR door bound
//     `jit_thread_current_thread_direct` (a `bridge` row) and compiled code
//     called it 298 000 times while the MethodEntry door refused all seven
//     sites it examined. See H12-1.
```

**O4 — `H7-1` §6b's scope sentence needs the correction in §0**, and
`HANDOFF-20260820.md` §7 item 4 should be re-marked: the predicted failure mode
is confirmed, at a door neither the item nor its two corrections named.

**O5 — `docs/known-issues/jdk-only/INDEX.md` needs a row.** This lane was
forbidden to edit it. Suggested row, in the H-wave bullet form:

```text
- [H12-1](H12-1-the-osr-door-binds-five-bridge-natives-the-method-entry-door-refuses-20260820.md) — `MEASURED` defect / `FIXED-UNVERIFIED` fix. The thin direct-call helpers' module comment claims they are "gated twice" under `JdkOnly`; **gate 1 was deleted 2026-08-06** (`jit/src/lib.rs:8528`: the `*_DIRECT_FN` cells are registered "unconditionally") and **gate 2 (`direct_native_helper`) is reached by only one of the three compile doors**. `compile_gate.rs`'s own three-door table says the OSR and EagerFirstCall doors reach `x64::compile_with_param_slots` directly; both build `direct_calls` themselves and ask no policy question, and both take helper addresses raw rather than through the gated cell. MEASURED under `--jdk-only`: `single-pass 0/7, IR 0/0, **OSR 1**` and **298 000** compiled calls into `jit_thread_current_thread_direct`, a `bridge` row — with `--real-jdk` reporting the identical `OSR 1`, i.e. the OSR door is mode-blind. Five `bridge` rows bind there (`Thread.currentThread`, `HashMap.get`, `HashMap.put`, `Preconditions.checkIndex`, `Reference.reachabilityFence`). **No value divergence witnessed**: `native_hashmap_get_exact` walks the real `table` field (`native-collections/src/lib.rs:9222`), so H4-1's "silently empty map" does not transfer to this door. Callee-side backstop landed in `vm/src/jit/helpers.rs`; the bind-time fix is O1/O2, out of file.
```

---

## 9. NOMINATIONS

**N1 — the three-door table should be a compile-time obligation for direct
calls, the way `CompileAdmission` already is for the backend entry.**
`compile_gate.rs` solved exactly this problem once, and its module doc states
the principle: *"The type system stops the accident … A fourth door written
without the gate does not compile."* It applied that to the admission checks and
not to `direct_calls`. Make `JitDirectCall` un-constructible outside a
constructor that takes a policy witness — the same trick, the same file, one
field. Then O1 and O2 stop being things a reviewer has to remember and become
things the compiler asks for. This is the single highest-leverage item in this
record and it is smaller than the audit that found the defect.

**N2 — the OSR door's HashMap binds have no counter, and that is why a
prediction about them was unfalsifiable.** `THREAD_CURRENT_THREAD_SITES_OSR`
exists only because `native-call-funnel-per-call-floor-item2-20260805.md` was
burned by an inert bind; the HashMap binds at the same door, added in the same
spirit, got none. H7-1 §6c predicts `collection_direct_helper_sites() == (0,0,0)`
under `--jdk-only` and treats a non-zero as the falsifier — but the counters it
names cannot see the OSR door at all, so the prediction is confirmed by a
measurement that is blind to the case it is about. One relaxed add per compile.

**N3 — `jit_reachability_fence_direct` is a `bridge` shadow with no dispatch
fallback.** §6. Its registered native is `black_box(arg); Ok(None)`, so the
strict-mode contract violation has no observable consequence today. Worth
either a fallback or an explicit, sourced note saying it is exempt because the
shadow and the shadowed are the same no-op — the point being that "no
consequence" should be written down, not inferred each time.

**N4 — `--jdk-only` and `--real-jdk` producing an identical door-bind profile is
a cheap, general gate nobody runs.** §3b's third fact took one command. Any
counter that is supposed to differ between the two modes and does not is a gate
that is not wired. `thread_current_thread_bound_sites()` already returns all
three doors; a two-arm assertion over it would have caught this the day the OSR
bind landed, and would catch the next one.

**N5 — the overlay is the untested representation, and every probe in this
record missed it.** §4/§7.3. `try_hm_int_fast_*` and `materialize_hm_int_fast`
are the only genuine second store under `java.util.HashMap`, and neither H7-1's
analysis, nor `RMapResizeGc`, nor any of my three probes establishes whether it
was even active. Until something says "the overlay served N of M gets", a green
map vector is evidence about the `table` walk only.

---

## INDEPENDENT REPRODUCTION (lane H0, 2026-08-20)

Reproduced from scratch on the pristine binary at `fe59bf9d9` — which does not
contain `9b7ad0f07` — with a probe written without reading this lane's fixture:
a single `static long hot()`, **called exactly once**, containing a 300,000-iteration
loop over `Thread.currentThread().hashCode()`. Called once, so the MethodEntry
door cannot be what tiers it up; 300,000 iterations, so OSR must.

```
CRATONVM_INTRINSIC_STATS=1 cratonvm.exe <mode> -cp . OsrDoor
```

| mode | `Thread.currentThread` direct calls | single-pass | IR | OSR |
|---|---:|---|---|---:|
| `--jdk-only` | **298,000** | **0/7** | 0/0 | **1** |
| `--real-jdk` | **298,000** | 0/0 | 0/0 | **1** |
| `--jdk-only --nojit` | 0 | 0/0 | 0/0 | 0 |

**Every number matches this record, including the 298,000.** Three things it
establishes on its own:

1. **The MethodEntry guard is real and it fired.** `0/7` — seven `invokestatic`
   sites examined in strict mode, seven refused. Anyone auditing only that door
   would correctly conclude strict mode refuses these binds.
2. **The OSR door bound anyway**, and compiled code then made 298,000 calls
   through a `bridge` row **under `--jdk-only`**.
3. **`--real-jdk` is byte-identical on the OSR column.** The door does not
   consult the mode. The `0/7` vs `0/0` difference in the *single-pass* column
   is the only place the two modes differ at all, which is precisely why an
   audit of that door reads as reassuring.

This is the `HANDOFF-20260820` §7 item 4 hole, found at the door that item did
not name — it named `vm/src/jit/helpers.rs`'s six helpers, and `H7-1` then
correctly showed those six are not the problem. **Both were right about their
own door and the hole was in the third one.** `the-osr-door-is-the-third-compile-door`
is a standing note in this project and it was still missed by two audits.

**What this does NOT show, and the record above is straight about it:** no wrong
*value* was witnessed. The probe's answer agrees with HotSpot. The exposure is
latent — an open door, not a live miscompile — and it becomes live exactly when
the rows it binds stop being `bridge`, which is what the retag plan proposes.
