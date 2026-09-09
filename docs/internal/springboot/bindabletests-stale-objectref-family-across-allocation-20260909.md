# The BindableTests residual is a FAMILY: ten `ObjectRef`s held across an allocation, and none of them the collector

| | |
|---|---|
| **Status** | **TEN ROOT-CAUSED AND FIXED**, 2026-09-09. The crash is gone; what remains is a different symptom, filed separately — see [What is left](#what-is-left). |
| **Was** | `docs/known-issues/springboot/bindabletests-local-holds-an-interior-word-of-a-retired-tlab-filler-20260909.md` |
| **Scope** | `--XX:UseGc Generational`, `CRATONVM_DBG_GC_STRESS <= 262144`. Passes at every threshold `>= 393216`, and unset. |
| **Reproducer** | `org.springframework.boot.context.properties.bind.BindableTests`, Linux x86-64 |

## The page this replaces was looking for a collector bug. There is none.

Its central finding was that a Java local reached `invokevirtual` holding "an
interior word of a retired TLAB's tail filler", and it named two candidate
producers: a native holding a raw `ObjectRef` across an allocation, and a TLAB
whose chunk was handed out twice. It asked for one measurement to separate them:

> record, per TLAB refill, the `[start, end)` chunk and the retire that filled
> it, and check whether the refused address falls inside a chunk that was live
> at the moment its filler was stamped.

That measurement now exists — three tripwires, below — and **the TLAB machinery
is exonerated on all three counts**. So are the object-start walk, the evacuator,
the card table and the per-bci liveness filter. Every failure this workload has
is the FIRST candidate: a bare `ObjectRef` that outlived an allocation.

The page could not see that because it was reading the wrong end of the defect.
"Not an object start" is a statement about the cycle that REPORTS it, and a young
semispace is reset and re-served from the same base every cycle — so one address
is a valid object start on one cycle and the interior of a filler on the next.
Dating the reports is what dissolved it.

## The first four defects

Each was found by the same instrument, in this order, each one uncovered by
fixing the one before it. All four are one shape — an `ObjectRef`, or a
`Vec<Value>` of them, that outlives an allocation without being pinned — and each
is verified by the trap that found it going silent.

### 1. `build_module`'s `layer` — `native-builtins/src/jboss_jdkspecific.rs`

`build_module(ctx, name, layer)` takes `layer` as a bare `ObjectRef`, then runs
`try_alloc_concurrent_synthetic`, `create_string` and `build_module_descriptor`
before storing it into the new `Module`'s slot 0. The caller pins `layer` across
`populate_boot_layer_modules` for exactly this reason; the pin stopped at the
function boundary, and the allocations are on the other side of it.

This is the `[rset-verify] first MISSING old->young edge: referrer=…
class_id=424 slot=0` that the predecessor page filed as **"a second, separate
defect"** and gave its own follow-up page to. It is neither second nor separate:
`class_id=424` is `java/lang/Module`, slot 0 is `layer`, and the "missing
old→young edge" was a dangling pointer, not a live edge the card table lost. It
was reported on **3 560 of 3 594 moving cycles**, and on 13 921 `[heap-stale]`
lines. After the fix both read **zero**.

### 2. `List.copyOf` / `Set.copyOf`'s `backing` — `native-collections/src/lib.rs`

```rust
let backing = try_alloc_synthetic(ctx, "java/util/ArrayList", AL_NUM_FIELDS)?;
native_al_init_from_collection(ctx, &[Value::Object(Some(backing)), src])?; // copies elements: allocates
alloc_immutable_wrapper(ctx, UNMOD_LIST_CLASS, backing)                     // pre-move address
```

`native_map_copy_of` already used the `pin_value` / `read_pinned_elem` /
`rooted_across` idiom for precisely this hazard. The list and set siblings never
got it.

### 3. `ArrayList.forEach`'s `action` — `native-collections/src/lib.rs`

`action` was pinned, but AFTER `resync_values_view` and
`al_or_collection_elements` — the second of which materialises the whole element
vector through the receiver's real iterator, running Java. The pin was two
statements too late, and the dead receiver reached `accept()`.

### 4. A constructor reference's arguments — `vm/src/runtime/interpreter/lambda.rs`

The one the page was actually chasing. `try_lambda_dispatch`'s
`NewInvokeSpecial` arm builds `full_args` from the proxy's captured fields, then
runs `ensure_class_initialized_shared` (the callee's `<clinit>`, arbitrary Java)
and `gc_alloc_object`, and only then lays `full_args` into `<init>`'s locals.
`new_obj` was pinned across `<init>` under a comment naming this exact hazard;
the arguments beside it were not.

Concretely: `TestMethodTestDescriptor::new`, reached as a constructor reference
from `MethodSelectorResolver$MethodType.createTestDescriptor`, received `arg[4]`
— the `enclosingInstanceTypes` `Supplier` — at the address collection 2 395 had
moved it away from. `DisplayNameUtils.determineDisplayNameForMethod local[0]` is
that argument, and `createDisplayNameSupplier` captured it into a second lambda
proxy, which is where the predecessor page found it and stopped.

## What ruled the collector out, and how

Every row was a live hypothesis on the predecessor page. Each is now refused by a
measurement rather than by argument.

| hypothesis | instrument | verdict |
|---|---|---|
| the allocator double-issued a TLAB chunk | `[tlab-audit] refill_tlab carved a chunk…` — full-span scan of every carved chunk, before it is zeroed | **0 hits** |
| a retire buried live objects under a tail filler | `[tlab-audit] install_tail_filler is about to stamp…` — full-span scan of the tail, before the header is written | **0 hits** |
| a TLAB kept bumping through a retired or re-served chunk | `[tlab-audit] BUMP-OVER-OCCUPIED` — every bump checks that the memory it hands out is zero | **0 hits** |
| the per-bci local-liveness filter | `CRATONVM_NO_LOCAL_LIVENESS=1` | reproduces identically |
| the zero-capture lambda singleton cache went stale | `[deadref-singleton]` | **0 hits** |
| a heap field was left un-forwarded | `[heap-stale] UN-FORWARDED` | **0 hits** |
| a frame slot was left un-rewritten | `POST-GC STALE LOCAL` / `STACK` | **0 hits** |
| the object was reclaimed while still referenced | `POST-COPY fate: pointer_map=Some(0x20012000048)` | it was **copied**; the holder was not rewritten |

The last row is the page in one line. The referent was relocated correctly. What
still named the old address was a Rust-side copy — neither a GC root nor a remap
target, which is what all four defects are.

## The instruments, and why each exists

All default-off unless marked. The first is the one that did the work: it turns
"a stale reference surfaced somewhere, thousands of cycles later" into "this line
of Rust wrote it".

| flag | report | answers |
|---|---|---|
| `CRATONVM_DBG_DEADREF_STORE` | `[deadref-store]` | a reference STORE whose value names no live object, with the Rust caller |
| ″ | `[deadref-nret]` | a NATIVE RETURN that names no live object, with the native and the Java call site — the only place that can name the native, since everything downstream sees an ordinary operand-stack value |
| ″ | `[deadref-push]` | the operand-stack push, `Value` and compact paths both, so `dup`, a local reload and the cached field/return producers are covered |
| ″ | `[deadref-pin]` | `pin_native_root` was handed an already-dead value — the caller is the defect, and no later refresh can recover it |
| ″ | `[deadref-arg]` | an argument already dead as it is laid into the callee's locals — the argument slice was held across a collection |
| ″ | `[deadref-local]` | the `set_local` where a dead reference becomes a Java-visible value |
| ″ | `[deadref-capture]` | a lambda capture dead before the proxy existed, with the Java frames and which of them still hold it |
| ″ | `[tlab-audit]` ×3 | the three ways a TLAB can be carved, filled or bumped over live memory |
| `CRATONVM_DBG_WATCH_ADDR=<hex>` | `[watch 0x…]` | one address's whole life story — every carve, filler, bump, arena hand-out, object-start verdict and post-copy fate, in order, dated by cycle |
| *(always on)* | `evacuator_verdict=` on `POST-GC RECLAIMED-WHILE-HELD` | REFUSED vs NEVER-OFFERED — two opposite defects that printed identically |
| `CRATONVM_DBG_HEAP_STALE` | `[heap-stale][PRE-gc]` / `[post-gc]` | dates a stale field to a collection instead of to a semispace parity |

### The predicate they share

`gen_heap::dead_young_ref_reason`: a young address names no live object if it is
in the INACTIVE semispace, or in the ACTIVE one with all-zero header words. The
two arms are independent, not a refinement of one another — the semispaces
alternate, so which arm a given dead address trips depends only on the parity of
the cycle it died in, and an instrument with just one arm reports every second
case and misses the rest.

`dead_young_ref_reason_global` is the same predicate without a heap handle, for
`Frame::set_local` and the argument copiers. The pre-existing `was_vacated`
ledger cannot answer this at all: it drops an address the instant the allocator
re-issues it, which is exactly when these references are read, which is why the
`set_local` guard built on it reported nothing on every run.

## Verification

`BindableTests`, Linux x86-64, `--XX:UseGc Generational`, `-Parallel 1`.

| `CRATONVM_DBG_GC_STRESS` | before | after |
|---:|---|---|
| *(unset — the default)* | PASS | **PASS** |
| 4 194 304 | PASS | **PASS** |
| 2 097 152 | PASS | **PASS** |
| 1 048 576 | PASS | **PASS** |
| 524 288 | PASS | **PASS** |
| 393 216 | *(untested)* | **PASS** |
| 262 144 | CRASH at moving cycle 2 398 | one failed assertion, different defect — below |
| 131 072 | CRASH | same |
| 65 536 | CRASH | same |

On the fixed binary at 262 144, in BOTH `--nojit` and JIT runs, EVERY probe
reads **zero**: `[deadref-store]`, `[deadref-pin]`, `[deadref-arg]`,
`[deadref-local]`, `[deadref-push]`, `[deadref-nret]`, `[deadref-capture]`,
`[deadref-singleton]`, the three `[tlab-audit]` counters, `[heap-stale]`, and
`[rset-verify]`'s missing-edge report. Before the fixes those same runs reported
3 560 missing edges, 13 921 heap-stale lines, 12 pin hits, 8 argument hits, 8
push hits and 2 stale native returns.

`cargo test -p cratonvm-types --lib`: 604 passed, 0 failed — including
`a_dbg_knob_declared_since_the_horizon_has_a_live_consumer`, which is what keeps
the two new flags from being registry entries nothing reads.

## The other six

The first four came out of the natives. Probing one step further upstream each
time — the operand stack, then the native return boundary — produced six more,
each uncovered by fixing the one before it:

5. **`Class.getProtectionDomain` unpinned `pd` one statement too early.**
   `unpin_native_roots(url_pin)` truncates to a base BELOW `pd_pin`, so it
   released `pd`'s pin too, and `populate_protection_domain_fields` — which
   allocates — then ran with nothing rooting `pd`. The native returned the
   pre-move address.
6. **`ArrayList.addAll`'s general path** pinned `this` AFTER
   `collect_collection_elements_or_real`, which drives a foreign collection's
   real `iterator()`.
7. **`HashSet.addAll`**, identical shape.
8. **`collect_via_real_iterator` pinned the ITERATOR but not the ELEMENTS.**
   Every `hasNext`/`next` runs arbitrary Java and `out` is a plain Rust `Vec`,
   so each turn of the loop could relocate everything collected so far. This one
   backs `toArray`, `addAll`, `forEach` and `stream` for every foreign
   collection — the widest of the ten.
9. **`build_empty_permissions`** returned the address it allocated rather than
   the one its `<init>` left behind.
10. **`try_lambda_dispatch`'s constructor-reference arguments** (defect 4 above,
    counted here for the total).

### The interpreter was carrying them, not producing them

Worth recording because two plausible fixes were considered and both were wrong.

`InvokeArgsRootGuard` pins an invoke's arguments and refreshes them at each
boundary, and `[deadref-arg]` fired at all four of its stages —
`push_args_to_locals`, `try_stackless_invoke` entry, `refresh`, and finally
`new`. `new` runs immediately after the arguments are read off the caller's
operand stack with nothing allocating in between, so the guard was pinning
values that were already dead. The invoke path needed no change.

`safe_native_call` already heals a stale return through `load_and_forward`, and
that could not help either: it reads the forwarding marker at the old address,
and that marker is gone once the allocator has re-served the span — which is
exactly the window a stale reference is read in. The heal is not wrong, it is
simply blind in the one case that matters, the same blind spot `was_vacated`
has.

## What is left

The crash is gone. At `<= 262144` one of the 27 tests now fails an ordinary
AssertJ assertion (`AbstractAssert.objects` reads null), and **every
stale-reference probe reads zero** in both `--nojit` and JIT runs, as does
`CRATONVM_GC_VERIFY_RSET` and `CRATONVM_DBG_HEAP_STALE`. It is a different shape
and it is filed as its own page:
[`bindabletests-assertj-objects-field-null-under-gc-stress-20260909.md`](../../known-issues/springboot/bindabletests-assertj-objects-field-null-under-gc-stress-20260909.md).

The threshold moved with it: 524 288 → **393 216**.

## A standing audit, from the same shape

A mechanical scan of `native-collections/src/lib.rs` for "an argument-derived
local pinned only AFTER the function's first allocating call" returns 28
candidates. Three of them were defects 2 and 3 above; the rest are unverified and
each needs the same treatment — read the function, confirm the value is used
after the allocation, apply the `rooted_across` idiom. `[deadref-pin]` is the
screen: it fires on exactly this mistake, and it fires at the pin rather than at
the eventual crash.

## Repro

```bash
CRATONVM_DBG_GC_STRESS=262144 \
CRATONVM_GC_RESERVE=0 \
CRATONVM_DBG_DEADREF_STORE=1 \
pwsh -NoProfile -Command "& '<repo>/apps/spring-boot-suite-runner/run-spring-boot-suite.ps1' \
  -Exe <cratonvm> -JdkHome /data/jdkimages/jdk25-linux/jdk-25.0.4+7 \
  -ClassList <core/spring-boot BindableTests> -Parallel 1 -TimeoutSec 1800 \
  -CratonArgs @('--XX:UseGc','Generational')"
```

`CRATONVM_GC_RESERVE=0` keeps decommitted granules mapped so a stale read is
reportable instead of a SIGSEGV.
