# H7-1 — the JIT's six collection helpers do not reimplement the natives; they re-enter them, and the guard in front of them names the wrong class

**Status: FIXED-UNVERIFIED — no binary carrying these changes has been built or
run.** Four commits, all in `vm/src/jit/helpers.rs` and `jit/src/lib.rs`. No
number below labelled "after" was measured; every prediction is marked
**PREDICTED** and says what falsifies it.

**Provenance: SOURCE-ONLY**, except where a line quotes
`scripts/baselines/jdk-only-kind-map-25-linux.tsv` (a checked-in measurement)
or `H0-3` (a run). Worktree
`C:/craton/cratonvm/.claude/worktrees/agent-aa5de1db5ee5afe3d`, branch
`claude/jdk-only-mode-handoff-09b48c` at `db71dfb40`, 2026-08-20. I was not
permitted to build, run or measure.

**This moves COMPATIBLE mode only. Strict mode moves by exactly zero**, and §3
shows why with the file and the baseline row that settle it. Per
`HANDOFF-20260820.md` §1 that is the expected shape: only retiring
`Bridge`-tagged shadows moves strict mode, and nothing here retires anything.

Lane H7, 2026-08-20.

---

## 0. The assignment, and the two things it was wrong about

`H4-1` O1 names six call sites in `vm/src/jit/helpers.rs` as **blocking** for
any map/set retag, on the grounds that they

> call `native_chm_get`, `native_hashmap_get_exact`, `native_hashmap_put_exact`
> and the two `jit_overlay_hashmap_*` entry points straight from compiled code,
> **past dispatch and past the kind check entirely**

and that `jit_concurrent_hashmap_get_direct` in particular "consults no policy
and no kind". `HANDOFF-20260820.md` §7 item 4 repeats it as *"six JIT direct
helpers … reimplement these natives"*.

**Both halves are wrong about the tree, and the corrections point in opposite
directions.**

| `H4-1` O1 says | the tree says | §  |
|---|---|---|
| the six **reimplement** the natives | none of the six contains a second implementation. Five *call* the registered Rust function; the sixth (`jit_overlay_hashmap_*`) is a `pub` re-export of the private first stage the native itself calls. This is **not** the `jit-thin-direct-helpers-reimplement-natives` species that `E18-1`/`E27-1` describe | §1 |
| they run **past the kind check entirely** | two of the six (`hashmap_native_callback`) already route through `admit_jit_fast_native`, and the other four are refused at **bind** time by `jit::direct_native_helper`, one level up, at compile time. Under `--jdk-only` today **none of the four can bind at all** | §3 |
| therefore a retag needs O1 landed first | a retag needs O1 landed first for a **different** reason: the guard that refuses them is asked about the wrong class, so the tagging change itself is what would open the door | §4 |

So the danger is real and `H4-1` was right to call it blocking. It is just not
the danger it named, and the difference decides what the fix is.

---

## 1. H7-A — what the six actually are

Line numbers are pre-change, matching `H4-1` O1 so the rows can be lined up;
anchor on the function name.

| # | site | callee | is it a second implementation? | agrees today? | when it runs |
|---|---|---|---|---|---|
| 1 | 13059, in `jit_concurrent_hashmap_get_direct` | `native_chm_get` | **no** — it is the registered callback for both `ConcurrentHashMap.get` and `ConcurrentMap.get` | **NO, two ways** (§2a, §2b) | compiled `invokeinterface java/util/concurrent/ConcurrentMap.get`, exact-CHM receiver, `--real-jdk` only |
| 2 | 13164, in `jit_hashmap_get_direct` | `jit_overlay_hashmap_get` | **no** — one-line `pub` wrapper over `try_hm_int_fast_get`, which is `native_hashmap_get_exact`'s own second stage | yes | compiled `HashMap.get` / `Map.get`, exact-HashMap receiver, `--real-jdk` only |
| 3 | 13185, `jit_hashmap_get_direct`'s fallback | `native_hashmap_get_exact` **through `safe_native_call_prevalidated_objects`** | **no** | yes | as row 2, when the overlay declines |
| 4 | 13353, in `jit_hashmap_put_direct` | `jit_overlay_hashmap_put` | **no** — `pub` wrapper over `try_hm_int_fast_put`, `native_hashmap_put_exact`'s **first statement** | **NO** (§2c) | compiled `invokevirtual java/util/HashMap.put`, exact receiver, `--real-jdk` only |
| 5 | 13538, in `hashmap_native_callback` | `native_hashmap_put_exact` | **no** | yes | site-cache fill in `jit_invoke_dispatch`, receiver-class-guarded, **already policy-routed** |
| 6 | 13541, in `hashmap_native_callback` | `native_hashmap_get_exact` | **no** | yes | as row 5 |

A seventh entry `H4-1`'s grep could not see, because it goes through
`NativeContext` rather than a `cratonvm_native_collections::` path:
`jit_hashmap_get_direct` probes `ctx.hashmap_string_node_cache_get_object`,
which is `native_hashmap_get_string_fast`'s own first statement. Agrees.

### Why "not a reimplementation" is the load-bearing fact

`native_hashmap_get_exact` and `native_map_get` are two names for one body once
the receiver is exactly `java/util/HashMap`, and that is checkable rather than
asserted:

```rust
fn native_map_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    …
    ht_reject_null_key(ctx, this, args.get(1))?;          // no-op: is_plain_hashtable_receiver
    if is_tree_map_receiver(ctx, this) { return native_tm_get(ctx, args); }   // false
    if is_chm_receiver(ctx, this)      { return native_chm_get(ctx, args); }  // false
    if is_lhm_receiver(ctx, this)      { return native_lhm_get(ctx, args); }  // false
    native_hashmap_get_exact(ctx, args)
}
```

and the same for `put`: `native_map_put_evict`'s `CF_EXACT_HASHMAP` arm is
`try_hm_int_fast_put` → `materialize_hm_int_fast` → `native_map_put_evict_pinned`,
which is `native_hashmap_put_exact`'s body line for line. Every helper here is
guarded on an **exact** `java/util/HashMap` receiver
(`jit_hashmap_receiver_is_exact`, and the site cache's own
`class.name.as_ref() == expected_class`), so the ladder those two natives differ
by is all no-ops.

Two guards inside the natives are skipped by the JIT and both are moot for the
same reason:

* `native_hashmap_get_exact`'s `is_identity_map_receiver` — `CF_IDENTITY_MAP` is
  set only when the class chain contains `java/util/IdentityHashMap`
  (`native-collections/src/lib.rs:314-325`), which an exact `java/util/HashMap`
  receiver cannot.
* `native_map_get`/`native_map_put_evict`'s `is_bare_java_lang_object` /
  `is_unmod_wrapper` / `ht_reject_null_*` — same.

So the correct reading is: **the JIT does not hold a second answer, it holds a
second door.** That is still exactly the population `H4-1` §1 and `H0-3` are
about — a writer of a container's state that the registry never sees — but the
remedy is different. There is nothing to reconcile; there is an entrance to
close and a guard to aim properly.

---

## 2. The three places they DID disagree, with both bodies

### 2a. An unvalidated key answered `null` instead of falling back

`jit_concurrent_hashmap_get_direct`, before:

```rust
let key = if key == 0 {
    Value::Object(None)
} else if let Some(key) = vm.mem.heap.is_object_address(key as usize) {
    Value::Object(Some(key))
} else {
    return 0;                       // <-- Java-visible `null`
};
```

`jit_hashmap_get_direct`, its own sibling in the same file, for the identical
situation:

```rust
// The key is dereferenced for the first time downstream, by
// `unbox_wrapper`'s `class_id_of`. Keep the arena-membership probe:
// it is the only check between a stale/fabricated key argument and
// that dereference.
match vm.mem.heap.is_object_address(key as usize) {
    Some(object) => Value::Object(Some(object)),
    None => break 'fast,            // <-- full dispatcher
};
```

One helper treats "I cannot find this argument in the heap" as a failed
validation and hands the call to the dispatcher; the other treats it as the
answer `null`. Interpreted, the same source line always takes the dispatcher.
**That is a tier-dependent wrong answer, in the plainest possible form**, and it
is the shape `chm-get-misses-stored-key-in-process-RETIRED-20260804.md` already
records once — reporting ABSENT for a pair that was never compared.

Fixed: `break 'fast`, plus the alignment / 48-bit screen on `key` that the
sibling had and this one did not, so a fabricated argument is refused before
`is_object_address` rather than inside it.

### 2b. `native_chm_get` was called with no funnel

Before:

```rust
// `native_chm_get` pins its receiver/key before every operation
// that can invoke Java or collect. Calling it directly avoids the
// second, redundant safe-native wrapper/root snapshot on a hot
// read-only lookup while retaining its canonical error contract.
let result = {
    let mut ctx = crate::vm::NativeContextImpl { shared: vm, thread: &mut *thread };
    cratonvm_native_collections::native_chm_get(&mut ctx, &values)
};
```

The premise in that comment is a **premise in a comment, not a compile-time
link**, and it is wrong twice:

* Pinning is a *service of the funnel*. `ctx.pin_native_root` writes into the
  ring whose watermark `safe_native_call_impl` sets and truncates; a native that
  pins is a native that assumed a funnel.
* `safe_native_call_leaf`'s doc enumerates exactly what the funnel supplies and
  what a **leaf** may drop: argument pinning, the STW probe, the `NativeRunning`
  transition — *"the state exists so the STW census WAITS for a native holding
  raw `ObjectRef`s in Rust locals"* — the JNI pending-exception drain, and
  `catch_unwind`. `native_chm_get` is not a leaf by any of the four criteria
  `set_leaf` states: it reaches `chm_key_hash`, which dispatches the key's real
  `hashCode()`. Its own sibling `native_chm_contains_key` carries a comment
  saying that this hazard was caught **live** by `RMapGcStress` with the
  `CRATONVM_DBG_STALE_OBJREF` canary firing.

And the tell was in the same file: `jit_hashmap_get_direct` runs its two leaf
probes wrapper-free and its non-leaf fallback (`native_hashmap_get_exact`)
through `safe_native_call_prevalidated_objects`. One file, one hazard, two
rules.

Fixed: `native_chm_get` now goes through
`safe_native_call_prevalidated_objects`, the same wrapper the interpreter's
route uses for the same callback.

**I am NOT claiming to have found a live GC bug here.** I could not run one. The
claim is narrower and does not need a run: two siblings answered one question
two ways, and the answer that skipped the funnel was defended by a comment whose
premise the funnel's own documentation contradicts.

### 2c. The put helper inserted, then re-dispatched the same put

Before:

```rust
let probe = { … cratonvm_native_collections::jit_overlay_hashmap_put(&mut ctx, recv_obj, vals[0], vals[1]) };
match probe {
    Some(Ok(Some(Value::Object(Some(object))))) => { … return object.as_ptr() as i64; }
    Some(Ok(Some(Value::Object(None)))) | Some(Ok(None)) => return 0,
    // The overlay stores whatever Value was put; an object-typed map
    // returning a non-object Value is out-of-contract — take the
    // full dispatcher rather than guessing an encoding.
    Some(Ok(Some(_))) => break 'fast,        // <-- the insert ALREADY happened
    …
}
```

`try_hm_int_fast_put` **has already inserted** by the time it returns
`Some(Ok(Some(old)))`. `break 'fast` falls to `jit_invoke_dispatch`, which
re-executes the put; the second execution's "previous mapping" is the value this
call just wrote. `HashMap.put` returns the previous mapping, so the Java-visible
answer is wrong.

The GET sibling has the byte-identical arm and may keep it, because a read is
idempotent. **The two were written as if they were the same shape and they are
not.** The comment quoted above is the same comment in both functions.

Reachable? The arm needs a non-object `Value` stored under an `i32` key in the
overlay. `H4-1` §1a counts **42 direct Rust `native_map_put_pub` call sites**
that are under no obligation to store `Value::Object`. So: reachable in
principle, not demonstrated. It is fixed regardless, because a mutation that can
be applied twice is not a thing to leave behind a comment saying it is
out-of-contract.

Fixed by removing the private door entirely: the helper now calls
`native_hashmap_put_exact` through the funnel. That native's **first statement**
is the same `try_hm_int_fast_put`, so the overlay fast path is unchanged; what
is gone is the JIT's own entry into it and the arm that could double-apply.
The residual out-of-contract case is a `debug_assert!` plus `null` — the one
reply that does not mutate a second time.

---

## 3. Whether this moves strict mode: no, and here is the file that says so

`H4-1` O1's "past the kind check entirely" is refuted by two mechanisms, in two
different files.

### 3a. The two `hashmap_native_callback` rows were never past it

`vm/src/jit/helpers.rs`'s `hashmap_native_callback` ends in
`admit_jit_fast_native(vm, "java/util/HashMap", …)`, whose `JdkOnly` arm builds
`(callback, kind_of_id(id))` and hands it to `resolve_native_dispatch_wave1`
with `bytecode_available = jit_fast_native_has_bytecode(…)`. A `Bridge` in front
of present bytecode returns `None`, the fast path is not cached, and the call
falls to the generic tail. Rows 5 and 6 of §1 have been gated since
JDK-ONLY-WAVE2 §4.

### 3b. The four direct helpers are refused at BIND time, one level up

`jit/src/lib.rs::direct_native_helper` runs inside `try_compile`'s bytecode
scan. Under `jdk_only` it admits **only** a triple the registry calls
`NativeKind::Intrinsic`, and returns `0` — the established "not wired" sentinel
— otherwise, recording a `NativeShadowsBytecode` violation tagged
`jit-thin-direct-helper`. `jdk_only` is threaded **per VM** through
`try_compile_with_invokespecial_resolver`, not read from a process latch (that
was JDK-ONLY-WAVE2 §2's fix), and the resolver is supplied by
`vm/src/runtime/interpreter/jit_bridge.rs`.

The four triples these ladders name, from the checked-in baseline
`scripts/baselines/jdk-only-kind-map-25-linux.tsv`:

```text
6554  java/util/HashMap	get	(Ljava/lang/Object;)Ljava/lang/Object;	0	bridge	0	1
6560  java/util/HashMap	put	(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;	0	bridge	0	1
6851  java/util/Map	get	(Ljava/lang/Object;)Ljava/lang/Object;	0	bridge	0	1
7602  java/util/concurrent/ConcurrentMap	get	(Ljava/lang/Object;)Ljava/lang/Object;	0	bridge	0	1
```

All four `bridge` ⇒ all four refused ⇒ **under `--jdk-only` no compiled site
binds a collection direct helper at all**, today, before this lane. That is why
`H0-3`'s armed run (`CRATONVM_ENFORCE_NATIVE_SHADOW=…ConcurrentHashMap`, strict
arm, 104 → 93) did not produce a *tier*-dependent failure on top of everything
else: in strict mode there is no second tier for these calls.

**Consequence, stated plainly.** Every change in this record is a
compatible-mode change. Strict mode's verdict must be byte-identical. This is
also the second lane in two days to find that the remedy an O-item prescribes
for strict mode is a compatible-mode remedy — `H4-1` §5c made the same
correction to `H0-2` §5.

---

## 4. H7-B — what I changed, and why this shape

`E18-1`/`E27-1`'s history is the reason the brief lists "delete" first and
warns that a gate is "a second rule to keep in step — the thing this codebase
keeps paying for". That history does not apply here, and the reason is §1: there
is no duplicate rule to delete, because there is no duplicate rule. `E18-1`'s
`jit_string_latin1_to_lower_direct` carried a **private copy of the case fold**;
these six carry no copy of anything.

So the fix is the third option, applied literally.

| change | option | why |
|---|---|---|
| `jit_concurrent_hashmap_get_direct` → `safe_native_call_prevalidated_objects` | **call the one implementation, through the one funnel** | the callee was already the one implementation; only the funnel differed, and it differed from its own sibling three hundred lines away |
| unvalidated key `return 0` → `break 'fast` | **call the one implementation** | the sibling's rule, applied to the sibling's hazard |
| `jit_hashmap_put_direct` → `native_hashmap_put_exact` via the funnel | **call the one implementation** | `jit_overlay_hashmap_put` is that native's own first statement; going through the native keeps the fast path and deletes the private entrance and the double-apply arm with it |
| `direct_native_helper_for_impl` | **make the guard mirror the dispatch** | §4a — not a second rule; the existing rule, asked about the right row |

I did **not** delete the helpers, and I did not gate them on the shadow dial.
Deleting them would be a pure `--real-jdk` throughput change with no correctness
argument behind it once §1 is established, and I cannot measure what it costs.
Gating them on `CRATONVM_ENFORCE_NATIVE_SHADOW` would be the second-rule trap
for real: that dial is read at exactly one site
(`vm/src/runtime/interpreter/native_override.rs:7433`, inside
`resolve_step1_native`) and is `is_jdk_only()`-gated, so a JIT-side copy of it
would be a copy of a rule that cannot fire in the mode the JIT-side helpers can
bind in. Nominated instead (N2).

I also did **not** "fix" anything by making the JIT match the current native's
behaviour where the native looks wrong. §5's `IdentityHashMap` finding is left
as a nomination against `native-collections`, untouched.

### 4a. The guard named the wrong class — the finding that changes the plan

`direct_native_helper` asks the registry about the triple it is handed, and the
ladders hand it **the class named at the call site**. For two of the three
collection ladders that is an interface:

```rust
if direct_jit_callee_calls_enabled
    && invoke_kind == 2
    && class_name == "java/util/concurrent/ConcurrentMap"      // <- asked about
    …
    let entry = direct_native_helper(&CONCURRENT_HASHMAP_GET_DIRECT_FN, jdk_only,
                                     intrinsic_resolver, &class_name, …);
```

but the helper it binds runs `native_chm_get`, which is
`java/util/concurrent/ConcurrentHashMap`'s registration. Same for the
`java/util/Map.get` arm, whose helper runs `native_hashmap_get_exact`,
registered on `java/util/HashMap`.

Today both rows in each pair are `bridge`, so both questions refuse and the
answer is right by coincidence. **The coincidence is exactly the thing `H4-1`
and `H0-3` are asking somebody to change.** A retag that moved
`ConcurrentMap.get` (5 rows, which `H4-1` §7b's table explicitly says must NOT
move) or `Map.get` to `Intrinsic` while the implementing row stayed `Bridge`
would bind, under `--jdk-only`, a helper whose implementation is a `Bridge` —
compiled code back in front of bytecode the interpreter runs, and no arm can
diff the two. This is the `a-serviceability-predicate-must-mirror-the-dispatch-it-guards`
species.

Closed by `direct_native_helper_for_impl`, which requires **both** rows to be
admitted and skips the second question when the two classes coincide. Zero
behaviour change today; strictly more conservative under any retag.

---

## 5. H7-C — the instrument, and the one that already exists but reads 0

### 5a. Written: two instruments, both in my files

1. **`HASHMAP_GET_DIRECT_SITES` / `HASHMAP_PUT_DIRECT_SITES` /
   `CONCURRENT_HASHMAP_GET_DIRECT_SITES`**, with
   `collection_direct_helper_sites() -> (u64, u64, u64)`. Same pattern as the
   existing `THREAD_CURRENT_THREAD_SITES_*` / `census_direct_helper_sites`.
   Cold-path relaxed adds at bind time.

   Read beside the existing `jdk_only_direct_native_refusals()` they separate
   the two things a green arm cannot:

   | sites | refusals | reading |
   |---|---|---|
   | 0 | 0 | **no compiled site ever saw one of these calls — the run says nothing about the gate** |
   | 0 | >0 | the gate fired; strict mode took the interpreter's route |
   | >0 | 0 | compiled code holds a second entry into the map's state (expected under `--real-jdk`) |
   | >0 | >0 | two VMs in one process, or a policy that changed between compiles |

   The top-left cell is the point. `LEAF_NATIVE_HITS`'s own doc says *"timings
   cannot tell 'the fast path was never installed' from 'it was installed and is
   no faster'"*; the same sentence with "the gate refused" and "nothing reached
   it" is what a strict arm has been unable to say about this door.

2. **`strict_mode_refuses_every_collection_direct_helper`** in
   `jit/src/lib.rs`'s test module. It pins the §3b fact — the one keeping strict
   mode consistent, written down nowhere until now — per triple, asserts the
   refusal is *counted* rather than a silent zero, asserts compatible mode still
   binds (i.e. that this whole family is a compatible-mode concern), and asserts
   §4a's closed hole in both directions.

### 5b. Not written, and why not: the both-implementations differential

The brief's first idea — a debug-mode assertion that runs both implementations
and compares — is the right instrument and I did not write it, for a reason
worth recording rather than a lack of time. There is no second implementation to
run (§1). A differential here would compare `native_chm_get` with
`native_chm_get`. The disagreements this record found are all in the *wrapper*
and the *arms around* the call, and a value-differential would have seen none of
the three: §2a is a path taken instead of the native, §2b is invisible to a
return value, §2c only fires on a `Value` shape the differential would itself
discard.

What would have caught all three is **a vector that runs the same map operations
cold and after forced tier-up and diffs the answers**, which does not exist —
see §6. That is N1.

### 5c. What the union census can and cannot say

`--jdk-only-report` already marks the four registry rows'
`invocations` incomplete via `DIRECT_CALL_HELPER_NATIVES`
(`vm/src/jit/helpers.rs`), and `run.sh` now prints the union on every strict
arm. **That mark is a static claim about wiring, not a count**, as its own doc
says, and under `--jdk-only` the four helpers never bind, so the mark is
pessimistic there and the census cannot distinguish that from a bind that
happened. The counters in 5a are the missing half and cost one relaxed add per
compile.

---

> **VERIFIED AGAINST A BINARY 2026-09-02.** §6a.1's test has been compiled and
> run — and §6a.1's own warning is the reason it needed doing properly:
> "`cargo check -p X` builds the LIB only", so a test inside `jit/src/lib.rs`'s
> `mod tests` is **not compiled at all** by the obvious command.
>
> ```text
> cargo test -p cratonvm-jit --lib strict_mode_refuses_every_collection_direct_helper
>   tests::strict_mode_refuses_every_collection_direct_helper ... ok
>   1 passed, 0 failed, 2194 filtered out
> ```
>
> Run by NAME, and the `2194 filtered out` is what says the name still resolves
> rather than the filter matching nothing.
>
> **§6a.4 is NOT verified here, and the reason is §6b's own point.** The strict
> report's `refusals` object exists and reads:
>
> ```json
> {"jit_direct_native_binds": 0, "jit_inline_cache_natives": 0,
>  "jit_fastpath_admissions": 5, "interpreter_bytecode_preferred": 35,
>  "interpreter_shadow_unenforced": 47}
> ```
>
> That report came from a small probe, not a strict ARM over the corpus, so a
> zero here is a statement about the workload and not about the counter. **A
> count of 0 from a workload that cannot reach the path is not a measurement** —
> which is exactly what §6b established from the other direction: `RJitGc`
> contains no map at all, no vector in `regression-suite/src/` declares a
> `ConcurrentMap`-typed variable, and so
> `CONCURRENT_HASHMAP_GET_DIRECT_SITES` reads 0 on every arm regardless.
>
> §6b's framing is the right one and stands unchanged: *"That is not a reason to
> doubt them; it is the measurement of how much the suite can say."*
>
> **§6a.2 and §6a.3 have expired.** They pin `--jdk-only` 104/104, `SUITE=all`
> 99/104, and the stub ratchet at 1615 / 12857 and 1626 / 13225. Today the suite
> is 125–128 vectors and the ratchet reads 1634 / 13529 and 1645 / 13897, moved
> by three weeks of other lanes — the same expiry as `H3-1` §5, `W7-30` §11 and
> `H4-1` §7a.

## 6. VERIFICATION PLAN

### 6a. For what landed

1. `cargo check -p cratonvm-jit -p cratonvm-vm`. **Standing rule: `cargo check
   -p X` builds the LIB only** — the new test lives in `jit/src/lib.rs`'s
   `mod tests`, so it needs `cargo test -p cratonvm-jit --lib
   strict_mode_refuses_every_collection_direct_helper` (or at minimum
   `cargo check -p cratonvm-jit --tests`) or it is not compiled at all.
2. **All three arms verdict-identical to `HANDOFF-20260820.md` §1**:
   `--jdk-only` 104/104, `SUITE=all` 99/104 with the same five,
   `SUITE=core` 63/64.
3. **The stub ratchet must not move**: 1615 / 12857 and 1626 / 13225. Nothing
   here touches a registration or a kind; if it moves, the cause is elsewhere in
   the merge.
4. `jdk_only_direct_native_refusals()` on a strict arm should be **> 0 and
   unchanged from pristine** — §4a adds a second refusal only when the two
   classes differ AND the first one passed, which no row does today.

### 6b. Which vectors exercise these paths — checked, not trusted

The brief nominated `RJitGc`, `RMapResizeGc`, `RMapGcStress`,
`RChmKeySetView`. Measured against the corpus:

| vector | binds a collection direct helper? | evidence |
|---|---|---|
| `RJitGc` | **no — it contains no map at all** | the whole file is arithmetic loops, binary trees, a megamorphic `Op[]` dispatch and an `int[]` scan. Grep for `Map` returns nothing |
| `RMapResizeGc` | **yes, the `get` helper**, and it is the best coverage in the corpus | `fill`/`verify` take `Map<K,String>`, so `m.get(...)` is `invokeinterface java/util/Map.get` — the recognised arm; `hm` is an exact `java.util.HashMap`; `n = 20000` by default, hot enough to tier up. Keys are the custom class `K`, not `Integer`, so the overlay declines and the **wrappered `native_hashmap_get_exact` fallback (§1 row 3) is what runs** |
| `RMapGcStress` | same shape, `n = 3000` | as above |
| `RChmKeySetView` | **no** | every receiver is declared `ConcurrentHashMap` or `Map`, never `ConcurrentMap`; loops are `iters = 500` |
| **any vector at all** | **no vector in `regression-suite/src/` declares a `ConcurrentMap`-typed variable** | `grep -l 'ConcurrentMap<' regression-suite/src/*.java` → empty |

**So `CONCURRENT_HASHMAP_GET_DIRECT_SITES` will read 0 on every arm**, and §2a
and §2b are unexercised by the suite in either mode. That is not a reason to
doubt them; it is the measurement of how much the suite can say, and it is why
N1 exists.

Two further scope facts the orchestrator needs before reading a 0:

* **These three ladders are single-pass-backend only.** `jit/src/lib.rs`'s IR
  ladder carries an explicit scope note: the other six `*_DIRECT_FN` helpers
  "are still single-pass-only", and only `Thread.currentThread`,
  `Preconditions.checkIndex` and `Reference.reachabilityFence` were added at the
  optimizing/OSR door. A method that reaches the optimizing tier or is
  OSR-compiled therefore never binds a collection helper. A hot loop can be
  full of `HashMap.get` and still report 0 sites.
* `HashMap.put` is recognised **only** on the `invokevirtual java/util/HashMap`
  arm. A `Map`-declared receiver's `put` is not recognised at all, which is why
  `RMapResizeGc`'s `fill` exercises `get` and not `put`.

### 6c. What to look for

* `--real-jdk` (`SUITE=all`): the five failures must be the same five.
  `RMapResizeGc` and `RMapGcStress` pass today and must still pass — they are
  the only vectors touching a changed code path.
* `--jdk-only`: 104/104, and **`collection_direct_helper_sites()` == (0, 0, 0)**.
  A non-zero there falsifies §3b and is a bigger finding than anything in this
  record.
* `--real-jdk` on `RMapResizeGc`: `collection_direct_helper_sites().0 > 0`. A
  zero means the single-pass door never fired for it and §6b's "best coverage"
  claim is wrong — in which case **nothing in the corpus exercises any of the
  four direct helpers** and every change here is verified only by
  `cargo check` + the arms staying neutral. Say so if that is what happens.
* Any Spring / H2 / Tomcat suite run, for the §2c and §2b paths: those workloads
  are `HashMap`-dense and `--real-jdk`, which is the one mode where these
  helpers bind.

### 6d. PREDICTED, with falsifiers

| prediction | falsified by |
|---|---|
| all three arms verdict-identical | any vector flipping. `RMapResizeGc`/`RMapGcStress` flipping would point at the put rewrite or the `get` funnel; anything else points elsewhere |
| stub ratchet unchanged in both configurations | any movement — nothing here is a registration |
| `collection_direct_helper_sites()` is `(0,0,0)` under `--jdk-only` | a non-zero, which would mean `direct_native_helper` is not reached from some compile door |
| no measurable `--real-jdk` throughput change on the arms | an A/B **on one binary** (`CRATONVM_JIT` toggles, not two builds — the mistake `census_direct_helpers_enabled`'s doc records) showing a map-put-dominated workload regressing beyond noise. The put path gains one funnel entry per overlay-servable put; that is the only place a cost can appear |

---

## 7. OUT-OF-FILE EDITS REQUIRED

**None are required for what landed.** The four commits are self-contained in
`vm/src/jit/helpers.rs` and `jit/src/lib.rs`.

The following are required **before** any map/set retag, and none is in a file
this lane owns.

**O1 — `vm/src/jit/helpers.rs::hashmap_native_callback` has the same
wrong-class problem §4a fixed in the ladders, one layer down.** It passes a
hard-coded `"java/util/HashMap"` to `admit_jit_fast_native` while the *call
site* may be `java/util/Map`. That direction is the safe one (it asks about the
implementing class, which is what runs), so it is **not** a defect today — but
if the ladder question and the site-cache question ever have to agree, they are
currently asking about different rows for the same call. Recorded so the next
person does not have to re-derive it. Owner: this file, but it is the site-cache
path, not the direct-bind path, and I did not want one commit spanning both
mechanisms.

**O2 — `native-collections/src/lib.rs::try_hm_int_fast_put` has no
identity-map guard while `try_hm_int_fast_get`'s caller does.**
`native_hashmap_get_exact` opens with
`let identity_mode = is_identity_map_receiver(ctx, this);` and skips both the
string and int fast paths when it is set. `native_hashmap_put_exact` calls
`try_hm_int_fast_put` **unconditionally**, and `native_map_put_evict` only
consults `is_identity_map_receiver` *inside* `native_map_put_evict_pinned`,
below the overlay. The overlay keys on the unboxed `i32`, so for an
`IdentityHashMap` receiver two distinct `Integer` objects of equal value
outside the `-128..=127` cache collapse into one entry, where the JDK holds two.
Not reachable from the JIT helpers (they guard exact `java/util/HashMap`), which
is why it is out of scope here rather than fixed. Nominated as N3.

**O3 — `regression-suite/src/` needs the vector §5b describes.** N1.

**O4 — `docs/known-issues/jdk-only/INDEX.md` needs a row for this record.**
This lane was explicitly forbidden to edit `INDEX.md`.
`HANDOFF-20260820.md` §5 records that all eight wave-H records got their rows
the day they landed, and that "the four previous INDEX passes each discovered
the wave then in flight had none" — so this one is a known-missing row, not an
oversight. Suggested row, matching the bullet form the H-wave entries use
(`INDEX.md:1127` for `H4-1`):

```text
- [H7-1](H7-1-the-second-door-into-the-map-and-the-guard-that-named-the-wrong-class-20260820.md) — `FIXED-UNVERIFIED` · **SOURCE-ONLY, compatible mode only.** `H4-1` O1's six JIT collection helpers do **not** reimplement the natives — five call the registered Rust function and the sixth is a `pub` re-export of the private stage the native itself calls — and they are **not** past the kind check: two route through `admit_jit_fast_native`, the other four are refused at BIND time by `jit::direct_native_helper`, and all four triples are `bridge` in `scripts/baselines/jdk-only-kind-map-25-linux.tsv`, so under `--jdk-only` none of them can bind. A call-site census cannot see a bind-time refusal. Three real disagreements fixed: `jit_concurrent_hashmap_get_direct` answered `null` for a key address the heap did not recognise (its sibling falls back) and called the non-leaf `native_chm_get` with no funnel; `jit_hashmap_put_direct` inserted through the overlay and then re-dispatched the same put, returning the value it had just written as the previous mapping. **The guard names the wrong class**: two ladders ask §1.4's question about the call site's INTERFACE (`java/util/Map.get`, `ConcurrentMap.get`) while the helper runs the implementation's native — right today only because both rows are `bridge`, which is exactly what a retag would change. Closed. Also: `RJitGc` contains no map, and NO vector in the corpus declares a `ConcurrentMap`-typed variable.
```

---

## 8. NOMINATIONS

**N1 — a tier-differential vector, `RJitMapTierDiff`, is the instrument this
whole class of defect needs and the corpus has never had.** §6b establishes that
no vector runs a map operation cold and warm and diffs the two answers, and §5b
establishes that no value-differential inside the VM would have caught any of
§2's three. The shape: for each of `HashMap.get` (virtual and via `Map`),
`HashMap.put`, and `ConcurrentMap.get` (which requires a `ConcurrentMap`-declared
local — **the corpus has none**), run N operations interpreted, force tier-up,
run the same N again, and assert the two answer sequences are identical. Include
the arguments the fast paths screen on: a `null` key, an `Integer` key, a
`String` key, a key whose `hashCode()` allocates. `run.sh` diffs stdout against
HotSpot, so the vector needs no in-VM oracle. It is the only thing that would
have made `H4-1` O1's stated failure mode — *"a tier-dependent wrong answer no
arm diffs for"* — an arm-visible one.

**N2 — `CRATONVM_ENFORCE_NATIVE_SHADOW` is read at exactly one site, and that
is a smaller population than its doc implies.** `env_cache`'s
`jdk_only_enforce_shadow_for` has one consumer in the tree
(`native_override.rs:7433`, inside `resolve_step1_native`). Every other dispatch
edge — `jdk_only_admit_jit_fast_native`, `direct_native_helper`,
`resolve_native_dispatch_wave1`'s own callers — computes `bytecode_available`
without it. That is defensible (those edges refuse rather than yield, so they are
already more conservative than the dial), but it means **`H0-3`'s 104 → 93 is
the blast radius of the dial at ONE site**, and a retirement acts at all of
them. `H0-3` N1's five per-family runs will inherit the same narrowing. Worth
one line in whatever plans the migration.

**N3 — O2's identity-map asymmetry.** `put` files an `IdentityHashMap`'s
`Integer` keys into a content-keyed overlay; `get` refuses to read it and
materialises instead. One afternoon, one vector, no cluster.

**N4 — the three collection ladders are single-pass-only and nobody has said
what that costs.** The IR-ladder scope note is explicit that this is "a real and
separate finding" deliberately not fixed, because none of the six had been A/B'd
at the optimizing tier. Since OSR is where hot loops go, the practical reach of
`perf/halfgap-20260717` on any real workload is unknown. Now measurable in one
run with `collection_direct_helper_sites()` against
`thread_current_thread_bound_sites()`, which reports all three doors.

---

## 9. Where I found a record, a comment or the brief wrong about the tree

Each with the grep or the file that settles it.

**9a. `H4-1` O1 — "six JIT direct helpers … reimplement these natives", "past
dispatch and past the kind check entirely", "consults no policy and no kind".**
None of the three survives §1 and §3. The six contain no second implementation;
two are routed through `admit_jit_fast_native`; the other four are refused at
bind time by `direct_native_helper`, whose `jdk_only` input is threaded per-VM
and whose verdict for all four triples is "refuse" against a checked-in
baseline. `HANDOFF-20260820.md` §7 item 4 repeats the wording and inherits the
correction.

The reason the error is understandable is worth stating, because it will
recur: `H4-1`'s recipe was
`grep -rn 'cratonvm_native_collections::' vm/src/jit/helpers.rs`. That grep sees
the *callee* and cannot see the *gate*, which is one crate away in
`jit/src/lib.rs` and one phase earlier (compile time, not dispatch time).
**A call-site census cannot see a bind-time refusal.**

**9b. `H4-1` O1's prescription — "the correct shape is a guard at the bind
site … the binder should decline when `find_with_kind(class, method, desc)`
yields nothing".** That guard exists and has since JDK-ONLY-WAVE2 §4; it is
`direct_native_helper`, and it is stricter than the prescription (it declines
unless the kind is `Intrinsic`, not merely when nothing resolves). O1 was
written "as the requirement rather than as a patch, because the bind site is in
a file I could not read into" — the requirement was already met. What was
*not* met is §4a, which O1 did not ask for and which is the part that actually
breaks under a retag.

**9c. The brief's candidate vector list — `RJitGc`.** It contains no map.
`grep -n 'Map' regression-suite/src/RJitGc.java` is empty; the file is
arithmetic, trees, a megamorphic lambda loop and an `int[]` scan. `RChmKeySetView`
is also not a candidate: no vector in the corpus declares a `ConcurrentMap`-typed
variable, so the `invokeinterface ConcurrentMap.get` shape that binds
`jit_concurrent_hashmap_get_direct` does not occur. Of the four named, one
(`RMapResizeGc`) is real coverage and one (`RMapGcStress`) is the same shape
smaller.

**9d. `jit_concurrent_hashmap_get_direct`'s own comment — "`native_chm_get`
pins its receiver/key … Calling it directly avoids the second, redundant
safe-native wrapper".** §2b. Pinning is a service of the wrapper being called
redundant; the wrapper also supplies the `NativeRunning` transition, the
argument-forwarding barrier, the exception drain and `catch_unwind`, per
`safe_native_call_leaf`'s own table of what only a **leaf** may drop.
`native_chm_get` is not a leaf.

**9e. `jit_hashmap_put_direct`'s comment — "take the full dispatcher rather
than guessing an encoding".** §2c. Copied verbatim from the GET sibling, where
it is correct. On the PUT path taking the full dispatcher re-applies a mutation
that has already happened. A comment that is true in one function and false in
its copy is the same failure `H5-1` found in `FileInputStream.read` (a comment
defending a duplicate registration by describing two callbacks that were one).

**9f. `jit/src/lib.rs`'s JDK-ONLY-NOTE item 3 — "`build_helpers` … should skip
the `set_*_direct_fn` registrations entirely under `JdkOnly`".** That advice was
followed and then **reversed** on 2026-08-06, and the note was not updated.
`vm/src/jit/helpers.rs::build_helpers_opt` now registers them
*unconditionally*, with a comment explaining that withholding them was a
process-wide side effect rather than per-VM protection. The JDK-ONLY-NOTE and
the code it describes now say opposite things. Not fixed here (the note is a
32-line block about six items and I did not want to touch five of them in a
commit about the sixth); recorded.

**9g. And one thing I asserted to myself and had to withdraw.** I first read
`jit_hashmap_get_direct`'s two leaf probes (`hashmap_string_node_cache_get_object`
then `jit_overlay_hashmap_get`) as an *ordering* divergence from
`native_hashmap_get_exact`, which probes the object-keyed cache, then the
text-keyed cache and the string chain, and only then the int overlay. It is not
a divergence: the overlay declines for a `String` key (`unbox_wrapper` yields no
`Value::Int`), so the JIT falls through to the native, which redoes both cache
probes and continues from there. The paths converge; the JIT pays one redundant
probe. **An ordering difference between two ladders is only a divergence if some
input can be answered by both**, and I nearly wrote up one that cannot.
