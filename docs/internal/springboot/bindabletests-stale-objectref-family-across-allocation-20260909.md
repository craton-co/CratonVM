# The BindableTests residual is a FAMILY: four `ObjectRef`s held across an allocation, and one still open

| | |
|---|---|
| **Status** | **FOUR ROOT-CAUSED AND FIXED**, 2026-09-09. One residual remains, named and instrumented — see [The residual](#the-residual-arguments-held-across-a-collection-in-the-invoke-prologue). |
| **Was** | `docs/known-issues/springboot/bindabletests-local-holds-an-interior-word-of-a-retired-tlab-filler-20260909.md` |
| **Scope** | `--XX:UseGc Generational`, `CRATONVM_DBG_GC_STRESS <= 262144`. Passes at every threshold `>= 524288`, and unset. |
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

## The four defects

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
| 262 144 | CRASH at moving cycle 2 398 | FAIL later, different defect — below |

On the fixed binary at 262 144, the three `[tlab-audit]` counters,
`[deadref-pin]`, `[deadref-capture]`, `[deadref-singleton]`, `[heap-stale]` and
`[rset-verify]`'s missing-edge report all read **zero**. Before the fixes they
read 3 560, 13 921, and one apiece.

`cargo test -p cratonvm-types --lib`: 604 passed, 0 failed — including
`a_dbg_knob_declared_since_the_horizon_has_a_live_consumer`, which is what keeps
the two new flags from being registry entries nothing reads.

## The residual: arguments held across a collection in the invoke prologue

At `<= 262144` the class still fails, and `[deadref-arg]` names the mechanism
with no ambiguity left in it:

```text
[deadref-arg] INACTIVE-SEMISPACE push_args_to_locals: arg[5] = 0x20012000228 is
  already dead as it is laid into the callee's locals
    at frame::push_args_to_locals
    at frame::Frame::new_pooled
    at interpreter::invoke::try_stackless_invoke:4779
    at interpreter::invoke::execute_invoke_kind
```

This is defect 4's shape at the GENERIC invoke path rather than the lambda one.
`try_stackless_invoke` receives `args: &[Value]` — a slice the caller popped off
the operand stack into a Rust `Vec`, which is neither scanned as a root nor
remapped — and builds the callee frame from it after a prologue that can complete
a moving young collection.

The fix is the same idiom, but this is the hottest path in the VM, so it must not
become an unconditional pin per argument per invoke. The next step is to bracket
only the GC-capable steps of that prologue — class initialisation and
`monitor_enter_synchronized_method` are the two candidates — and confirm by
`[deadref-arg]` going silent. `--nojit` reproduces identically and names the same
sites, which is what says the JIT is not involved.

**Do not re-open the collector for this.** Each row of the ruled-out table is a
measurement on this exact workload, and the residual's own report names a Rust
slice, not a heap address.

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
