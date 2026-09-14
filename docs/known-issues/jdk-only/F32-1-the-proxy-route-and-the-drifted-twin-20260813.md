# F32-1 — Which route answers a proxy call, the drifted twin that decides it, and a degrade made loud

**2026-08-13, lane F32.** Settles the route question F19-1 §4.2 left open, and
finds that the deciding line is in **neither** of the two sites F19 named.
Edits exactly two files — `classloading/src/proxy_gen.rs` and
`vm/src/runtime/interpreter/typecheck.rs` (plus this record). **This lane did
not build or run CratonVM**: every CratonVM claim below is marked READ or
PREDICTED, and every HotSpot number is MEASURED on this host against
`openjdk 25.0.3 2026-04-21 LTS (25.0.3+9-LTS)` (Microsoft build) before it was
written down.

**Both file paths in the brief were correct** — `classloading/src/proxy_gen.rs`
and `vm/src/runtime/interpreter/typecheck.rs` both exist and both contain what
the brief said they contain (`emit_proxy_classfile`; `class_chain_reaches_
proxy_instance` at `:949` pre-edit). Verified by `find` and by
`grep -rn class_chain_reaches_proxy_instance --include=*.rs`, not assumed.

---

## 1. Verdict

| | |
|---|---|
| **the route, steady state** | the **generated `$ProxyN` bytecode** — the emitted `X.valueOf`. **READ, not measured.** |
| **the route, first call at a cold call site** | the **shim** (`proxy_box_value_for_desc`). READ. Both routes are live in one process, in that order. |
| **the deciding line** | `vm/src/runtime/interpreter/dispatch_virtual.rs:478` — `const PROXY_INSTANCE: &str = "java/lang/reflect/Proxy$Instance";` inside `execute_invokevirtual_vtable_fast`. It is an **inlined copy** of `class_chain_reaches_proxy_instance` that carries **one** of that function's two names. |
| F19's reading ("the shim wins, the emitted `valueOf` is never reached") | **wrong, and wrong for a reason F19 could not have seen from the two sites it read.** Both sites it named *do* fire — but a third, earlier door returns before either is consulted. |
| the brief's "both interception sites fire before the receiver's own bytecode runs" | **true of those two sites, false of the dispatch pipeline.** There are **three** doors, in a fixed order, and the first two are not the two F19 read. |
| `class_chain_reaches_proxy_instance` "over-matches `java/lang/reflect/Proxy`" | **not an over-match — the `Proxy` arm is the DEFAULT arm.** `real_proxy_super()` is `truthy_word_default_true`, so a shipped `$ProxyN` extends the real `java/lang/reflect/Proxy` and `Proxy$Instance` appears nowhere on its chain. Narrowing it away would recognise **no proxy at all**. (§4) |
| what `RJdkReflBox --only=proxy` will show | **PREDICTED green on all 8 rows, before and after F19's `vm_exec.rs` change**, because the emitted bytecode is byte-for-byte the same boxing HotSpot emits (§3, MEASURED). Under F19's three-way table that is outcome 1. |
| degrade made observable | `emit_proxy_classfile` now counts real-super / synthetic-super / failed emissions behind a reader, and logs an emission failure **unconditionally** with the consequence spelled out. Default **not** flipped to strict; §5 says what that would break. |
| NOMINATIONS raised | **4** |

**The one sentence not to skim.** F19 asked "which of the two boxing
implementations answers?" and the honest answer is that the question had a
hidden third term: a vtable fast path that was taught to recognise proxies in
July, before the superclass it recognises them by was changed underneath it.
The proxy guard that exists to force proxies onto the slow path has been inert
for the shipping configuration since the real-super gate landed default-on.

---

## 2. The three doors, in order, and why only the third was read

READ from `vm/src/runtime/interpreter.rs`. `invokevirtual` (`0xb6`,
`interpreter.rs:6976`) and `invokeinterface` (`0xb9`, `:7192`) both run the
same three-stage pipeline, and a proxy interface call is an **invokeinterface**:

```
  1. execute_invokevirtual_cached      (interpreter.rs:6991 / :7197)
        thread-local monomorphic inline cache. No dynamic-proxy guard at all
        — grep -ni proxy over dispatch_virtual.rs returns only lambda-proxy
        and AnnotationProxy arms in this function.
  2. execute_invokevirtual_vtable_fast (interpreter.rs:7017 / :7226)
        HAS a dynamic-proxy guard, at dispatch_virtual.rs:476-503.
        It installs CachedInvokeTarget::VirtualBytecode (:945) on success.
  3. execute_invoke / execute_invoke_kind (interpreter.rs:7040 / :7251)
        invoke.rs:1852's `is_proxy_dispatch` — the WIDE guard.
```

`vm_exec.rs:9855` — F19's other site — is the `NativeContext::invoke_virtual`
entry, i.e. the door from **Rust natives** (`Method.invoke`, annotation
machinery), not from bytecode. It is correct and it is wide. It simply is not
on the path a Java `iface.m(7)` takes.

So the pipeline has one guard on the bytecode path *between* the inline cache
and the slow path, and that guard is stage 2's.

### 2.1 What stage 2's guard actually asks

`dispatch_virtual.rs:476-503`, READ verbatim:

```rust
{
    const MAX_DEPTH: usize = 32;
    const PROXY_INSTANCE: &str = "java/lang/reflect/Proxy$Instance";
    let mut current = Some(receiver_class_id);
    let mut is_proxy = false;
    for _ in 0..MAX_DEPTH {
        …
        if &*class.name == PROXY_INSTANCE { is_proxy = true; break; }
        if &*class.name == "java/lang/Object" { break; }
        current = class.superclass;
    }
    if is_proxy { drop(cm); return Ok(CachedCallResult::CacheMiss); }
}
```

One name. `class_chain_reaches_proxy_instance` (`typecheck.rs`, pre-edit
`:965`) has **two**:

```rust
if &*class.name == PROXY_INSTANCE || &*class.name == REAL_PROXY_BASE
```

The comment above the inlined copy explains why it is inlined and not a call —
a nested second `class_manager.read()` while the outer guard is live
self-deadlocks under parking_lot's writer-preferring fairness. That reasoning
is sound. Copying the *body* rather than extracting a lock-free *predicate* is
what let the two drift.

### 2.2 The chain a shipped proxy actually has

READ, three links, each one a line:

1. `types/src/flags.rs:1907` — `real_proxy_super: truthy_word_default_true(src, "CRATONVM_REAL_PROXY_SUPER")`. **Default ON.**
2. `native-builtins/src/reflect_annotations.rs:3767` — `proxy_super_class_name()` returns `"java/lang/reflect/Proxy"` when `real_proxy_super()`, else `"java/lang/reflect/Proxy$Instance"`.
3. `native-builtins/src/reflect_annotations.rs:4303` — `build_proxy_spec_for` writes `super_class: proxy_super_class_name().to_string()` into the `ProxyClassSpec` handed to `emit_proxy_classfile`.

So the default chain of a generated proxy is
`jdk/proxyM/$ProxyN → java/lang/reflect/Proxy → java/lang/Object`.
Walk it with stage 2's guard: `$ProxyN` is not `Proxy$Instance`, not `Object`
→ step; `java/lang/reflect/Proxy` is not `Proxy$Instance`, not `Object` →
step; `java/lang/Object` → **break**. `is_proxy = false`.

**Stage 2 does not cede.** Nothing else in it bails for a `$ProxyN` either,
READ end to end:

* the native-shadow ancestor walk at `:611` is skipped entirely, because
  `receiver_has_own_bytecode` is **true** — the generated class declares a
  concrete body for every proxied method (`emit_proxy_method`, one per
  `ProxyMethod`);
* the `is_interface` receiver-rooted re-selection at `:716` agrees, because
  `find_method_recursive($ProxyN, m, desc)` resolves to `$ProxyN` itself,
  which is also the vtable slot's `declaring_class_id`;
* the vtable read at `:683` succeeds, because `define_class_full` →
  `define_class_with_options` installs a vtable for every class it defines;
* `entry.is_native` is false, so `:695` does not bail;
* `:945` installs `CachedInvokeTarget::VirtualBytecode`.

### 2.3 The predicted sequence at one call site

**PREDICTED**, from the above:

| call | stage 1 (IC) | stage 2 (vtable) | stage 3 (wide guard) | who boxes |
|---|---|---|---|---|
| 1 | miss (cold) | `CacheMiss` — `resolution_cache.get_method` is cold (`dispatch_virtual.rs:141`) | **fires** | the **shim** |
| 2 | miss (stage 3 returned `Handled` without populating the IC) | resolution cache now warm; proxy guard says not-a-proxy; installs `VirtualBytecode` | not reached | the **generated bytecode** |
| 3+ | **hit** → generated bytecode | — | — | the **generated bytecode** |

That is also, exactly, the symptom `dispatch_virtual.rs:451-465`'s own comment
describes for the bug this guard was added to fix in July —
*"the FIRST call … poisons the cache … a LATER call on a different proxy
instance dispatches straight to the default method's bytecode and skips the
handler entirely"* (`AspectJAutoProxyCreatorTests.twoAdviceAspectPrototype`).
The fix is intact; the population it recognises is empty.

### 2.4 Why this is not merely academic

The generated body does not call `h.invoke(...)` directly. It ends in
`INVOKESTATIC java/lang/reflect/Proxy$Dispatch.invokeProxy`, an owner name
that **is never defined as a class** — it exists only as a key in the native
registry (`proxy_gen.rs`'s own module doc, and
`reflect_annotations.rs:3320-3324`, which says the native is registered
"defensively so any path that bypasses the hook … still routes through
`InvocationHandler.invoke`"). So both routes reach the same handler and the
same answer *for dispatch*; they differ only in **who boxes the arguments**,
which is precisely what F19 was chasing. That is why nothing visibly broke and
why this went unnoticed: the drift changed the boxing implementation, not the
call graph.

### 2.5 Provenance

READ. `dispatch_virtual.rs`'s guard traces to `b6805d3df` (**2026-07-02**,
"Fix aop cluster: … proxy default-method dispatch"), moved verbatim into its
own file by the SEAM-02 split (`5523d6c73`). `proxy_super_class_name` /
`real_proxy_super` post-date it. `typecheck.rs` **was** migrated for the
real-super shape — its `proxy_instance_satisfies_target` carries three
paragraphs about the 1-field real-super layout (`:112`, `:131`, `:147`) — so
this is one member of a family fix that was missed, the shape
`a-no-op-native-whose-comment-explains-why-it-is-a-no-op` names.

---

## 3. The oracle: HotSpot's own generated proxy, disassembled

**MEASURED**, HotSpot 25.0.3+9-LTS. `saveGeneratedFiles=true` is broken on
this JDK (`NullPointerException: Cannot read the array length because "b" is
null` out of `Proxy$ProxyBuilder.defineProxyClass`), so the bytes were taken
straight from the generator with
`--add-opens java.base/java.lang.reflect=ALL-UNNAMED` and
`ProxyGenerator.generateProxyClass(ClassLoader, String, List, int)`
(`scratchpad/f32/Dump.java`, 3517 bytes), then `javap -p -c`.

Also MEASURED, from a live proxy on this JDK:

```
proxy class = jdk.proxy1.$Proxy0
super       = java.lang.reflect.Proxy      <- CratonVM's default matches
```

HotSpot's body for `int m(int, boolean, char, long, byte, short, float, double, String)`:

```
 15: iload_1   16: invokestatic java/lang/Integer.valueOf:(I)Ljava/lang/Integer;
 22: iload_2   23: invokestatic java/lang/Boolean.valueOf:(Z)Ljava/lang/Boolean;
 29: iload_3   30: invokestatic java/lang/Character.valueOf:(C)Ljava/lang/Character;
 36: lload 4   38: invokestatic java/lang/Long.valueOf:(J)Ljava/lang/Long;
 44: iload 6   46: invokestatic java/lang/Byte.valueOf:(B)Ljava/lang/Byte;
 52: iload 7   54: invokestatic java/lang/Short.valueOf:(S)Ljava/lang/Short;
 61: fload 8   63: invokestatic java/lang/Float.valueOf:(F)Ljava/lang/Float;
 70: dload 9   72: invokestatic java/lang/Double.valueOf:(D)Ljava/lang/Double;
 79: aload 11  81: aastore                       (reference: no boxing)
 87: checkcast java/lang/Integer ; 90: invokevirtual Integer.intValue:()I ; 93: ireturn
```

and for `boolean eq(Object)`: `checkcast java/lang/Boolean` +
`Boolean.booleanValue()`.

`proxy_gen.rs:1249-1281` and `:1312-1335` emit **the same eight `valueOf`
calls and the same checkcast+`xValue` return coercion**, picking the wrapper
from the actual descriptor byte at that parameter position
(`int_family_param_wrapper`) rather than from a `Value` variant. So the
emitted route is canonical by construction, as F19 said — and this is the
measurement F19 did not have.

**PREDICTION for `RJdkReflBox --only=proxy`, all 8 rows:** `proxy.int`,
`proxy.char`, `proxy.bool`, `proxy.boolTRUE`, `proxy.long`, `proxy.byte`,
`proxy.short` **green**, `proxyoob.int1000` **green** (fresh — `valueOf` above
the cache bound allocates on both VMs), **before** F19's `vm_exec.rs` change
and after it. That is outcome **1** in F19 §4.2's table: *the emitted bytecode
answers; `proxy_box_value_for_desc` is a correctness fix for the degrade route
only.* If instead the family is red before and green after, §2.3 is wrong at
step 2 and the first place to look is whether `execute_invokevirtual_cached`
populates the IC from the stage-3 `Handled` return.

**Caveat, stated because it is the one row §2.3 cannot resolve by reading:**
the very first call at each call site takes the shim. `RJdkReflBox`'s proxy
rows call each shape once, so *if* every row is a cold site, the shim answers
every row and the family is a shim measurement after all. Two calls per row —
one warm-up, then the asserted one — makes the vector decide the question
instead of sampling one end of it. That is NOMINATION N3.

### 3.1 A second, unrelated divergence the disassembly exposes

HotSpot's generated body has an **exception table**: `Error` and
`RuntimeException` rethrown, everything else wrapped in
`UndeclaredThrowableException`. CratonVM's emitted body has **none** — and
must not have one, because the whole no-`StackMapTable` argument in
`proxy_gen.rs`'s module doc (and the
`emitted_class_is_straight_line_no_handlers` test) depends on an empty
`exception_table`. Whether the `Proxy$Dispatch.invokeProxy` native performs
the `UndeclaredThrowableException` wrap instead was **not checked by this
lane** and is not this lane's file; `reflect_annotations.rs:3339` says it
"consults" something on `ExceptionThrown`, which is where to start. Noted, not
diagnosed.

---

## 4. Task 3 — the predicate is not over-matching, and the narrowing is the trap

The brief asked whether `class_chain_reaches_proxy_instance` matching
`java/lang/reflect/Proxy` **as well as** the synthetic super is intentional.

**It is required, and the framing is inverted.** §2.2 shows `Proxy` is the
name a shipped generated proxy carries; `Proxy$Instance` is the *conditional*
arm. Asking "what serves the class per mode" gives two different answers and
both are live:

| configuration | super of a generated proxy | which arm matches |
|---|---|---|
| default (`CRATONVM_REAL_PROXY_SUPER` unset) | `java/lang/reflect/Proxy` | **`REAL_PROXY_BASE` only** |
| `CRATONVM_REAL_PROXY_SUPER=0` | `java/lang/reflect/Proxy$Instance` | `PROXY_INSTANCE` only |
| `ProxyClassOutcome::Degrade` / `Failed` (either gate) | `Proxy$Instance`, allocated directly (`reflect_annotations.rs:3187`, `:3200`) | `PROXY_INSTANCE` only |
| synthetic-JDK mode | `java/lang/reflect/Proxy` is itself a fabricated stub | `REAL_PROXY_BASE` |

Deleting either arm silently disables proxy interception for a whole mode with
no build error and no failing test — the exact shape
`narrowing-a-registration-to-a-flag-drops-the-mode` records.

**What was done instead.** The two names are now one shared, **lock-free,
name-only** predicate in `typecheck.rs`:

```rust
pub(super) fn class_name_is_proxy_super(name: &str) -> bool
```

`class_chain_reaches_proxy_instance` calls it. Name-only is the load-bearing
choice: it is what makes the *inlined* caller in `dispatch_virtual.rs` able to
ask the same question **while still holding its own `class_manager` guard**,
which was the entire reason the body was copied rather than called. The
duplication was not laziness — it was a lock-order constraint that nobody had
factored out. Now it is factored out, and the fix in `dispatch_virtual.rs` is
a two-line call (N1).

The doc comment at the new predicate states the flag chain with file and line
for each link, names the drifted twin, and says what breaks if either arm is
removed — so the next reader does not re-derive §2.2.

---

## 5. Task 2 — the silent degradation, made loud and counted

`real_proxy_strict()` is `affirmative_word(src, "CRATONVM_REAL_PROXY_STRICT")`
— **OFF by default** (`types/src/flags.rs:1906`, and
`real_proxy_strict_defaults_off` asserts it). `define_or_get_proxy_class`
classifies three failure stages, each logging **only** under
`CRATONVM_DBG_PROXY` (`reflect_annotations.rs:3918`, `:3929`, `:4011`), and
`native_proxy_new_instance` then allocates the 3-slot shim instead
(`:3200-3204`). Nothing throws. Nothing is logged. The returned object works.
And it boxes its arguments with a different implementation.

### 5.1 What landed, in this lane's file

`classloading/src/proxy_gen.rs`:

* `PROXY_EMIT_OK_REAL_SUPER`, `PROXY_EMIT_OK_SYNTHETIC_SUPER`,
  `PROXY_EMIT_FAILED`, and — shipped in the same change, deliberately — a
  reader, `pub fn proxy_emit_counts() -> (u64, u64, u64)`. A `fetch_add` with
  no reader is a write-only counter, which is the defect shape
  `a-write-only-counter-already-holds-the-diagnosis` records; this pair does
  not repeat it.
* The **split by super is the instrument**, not decoration. Every "is this a
  proxy?" predicate in the VM is a name test against one or both of those two
  names, so `real=N, synthetic=0` beside a proxy-guard counter of `0` is the
  whole of §2 in two numbers.
* Classification is by the super the **spec** names, via a pure
  `emitted_super_is_the_synthetic_shim`, not by re-reading the flag: `classloading`
  cannot see `native-builtins`' flags, and a flag read would mislabel exactly
  the rows that matter, since the `Degrade`/`Failed` fallbacks allocate the
  shim regardless of what the gate says.
* An emission failure now prints **unconditionally** — it is a capability
  loss, not a debug event — naming the class, the super, the error, the
  running failure count, that the caller will silently fall back, that the two
  implementations disagree on boxing, and the two env vars that change the
  outcome. The branch is only taken on a genuine `Err`, so a healthy run pays
  nothing.
* One test, `the_emission_census_counts_both_outcomes_and_can_be_read`,
  asserting monotonic increase (`>=`, not `==` — the counters are
  process-global and sibling tests in the same module also emit, so `>=` is
  the only sound assertion) plus the classification, tested purely.

### 5.2 The default was NOT flipped to strict, and what that would break

Asked for by the brief, and the answer is that it is a real behaviour change,
not a switch. Under `CRATONVM_REAL_PROXY_STRICT=1` *all three* stages become
`IllegalArgumentException` out of `Proxy.newProxyInstance`, and two of them
are reachable for reasons that are not the caller's fault:

* `Failed("spec")` — `build_proxy_spec_for` returns `None` when any interface
  ClassId will not resolve to a name. Today those callers get a working
  proxy.
* `Failed("define")` — the site's own comment lists `VerifyError`,
  `NoClassDefFoundError` on the super or an interface,
  `IncompatibleClassChangeError` on a duplicate define, and
  `SecurityException("Prohibited package name")`. A duplicate define in
  particular is a *race*, not a defect, and would make proxy creation
  intermittently throw.

Note also that `Proxy.getProxyClass` (`:3309`) **already** throws for any
non-`Real` outcome, so the two entry points are already inconsistent — a
reason to look at this properly rather than to flip a default. Nominated (N2)
with the counters now in place to size it first: if `PROXY_EMIT_FAILED` is 0
across the corpus, strict is free and the argument is over.

---

## 6. Task 4 — `simple_name_has_word` had no in-tree guard at all

Picked from `W8-C10-1-typecheck-hatch-audit-and-aastore-precedence.md` (INDEX:
**OPEN**, "one residual measured and left"), whose §4 residual is
`synthetic_implements`' substring arm — `typecheck.rs`, this lane's file.

**The residual is closed**: `simple_name_has_word` (`typecheck.rs:1086`)
implements both the simple-name rule and the iterator refusal, so the eight
names §4 lists as still-over-admitted are refused today. Verified by
extracting the function verbatim and running the rows under plain `rustc`.

**But the guard for it was never in the repository.** Its doc comment cites a
20,772-cell measurement asserted by `scratchpad/c16/verify.rs` — a path that
does not exist here. `grep -rn simple_name_has_word --include=*.rs` returned
seven lines at `HEAD`: the definition, one prose mention, and **six** call
sites (`:1340`–`:1344` and the `has` closure at `:1357`). **Zero tests.**
A heuristic that
decides `instanceof` for six interface targets across the whole fabricated-class
population had no in-tree assertion.

Added: `f32_pure_predicate_tests`, two tests, every row's verdict **MEASURED**
on HotSpot (`X.class.isAssignableFrom(Y)`, with the nested class names
enumerated by `getDeclaredClasses` rather than recalled — which mattered).

**Mutation-checked**, the whole row set re-run against four hand-made variants
under `rustc` (`scratchpad/f32/snhw2.rs`). Failing-row counts:

```text
  PRISTINE                                    0
  drop the `ends_with("Iterator")` arm        5    (all cursor rows)
  `simple` := the full `obj_name`             1    (ConcurrentSkipListMap$Values)
  full name AND plain `contains` (historical) 7
  drop the uppercase/digit word test          2    (word rows)
```

Three things the mutation check caught in **this lane's own first draft**, all
of which would have shipped as green-but-inert rows:

1. **Two rows were vacuous.** `ArrayList$Itr` + `"List"` and
   `ArrayDeque$DeqSpliterator` + `"Deque"` were written as iterator-arm rows.
   Their simple names are `Itr` and `DeqSpliterator`, neither of which
   contains the term — so they pass with the iterator arm **deleted**. Every
   surviving cursor row now has the term inside its simple name, and the test
   says so at the site.
2. **The "full name" mutation caught nothing** as first written, because the
   word test alone already refuses `…Collections$` (the term is followed by a
   lower-case `s`). The historical defect needed the full name **and** plain
   `contains`; that is now a separate, fourth mutant, and one measured row
   (`ConcurrentSkipListMap$Values` + `"List"` — `List` sits in front of a
   capital `M` in `SkipListMap`) is what gives the simple-name rule coverage
   of its own. `1` is a small number and is left visible rather than padded.
3. **The function's own doc comment is wrong about the oracle.** It cites
   `WorkQueue` and `ListResourceBundle` as admissions the word rule takes care
   not to lose. MEASURED on HotSpot 25.0.3+9:
   `Queue.class.isAssignableFrom(java.util.concurrent.ForkJoinPool$WorkQueue)`
   is **false** and `List.class.isAssignableFrom(java.util.ListResourceBundle)`
   is **false**. Both are over-admissions this heuristic knowingly keeps (it
   can only ADMIT, so a `false` is "no opinion") — not correct admissions it
   preserves. They are deliberately **not** asserted, with the measurement
   recorded at the site, because pinning them green would pin the wrong claim.

---

## 7. GC

No new heap storage, no new roots, no new caches. The three counters are
plain `AtomicU64` statics holding integers. `class_name_is_proxy_super` is a
pure `&str` comparison and takes no lock — strictly less locking than the code
it replaced, since `class_chain_reaches_proxy_instance` still holds its single
`class_manager` read guard for the whole walk exactly as before.

---

## 8. NOMINATIONS

### N1 — `vm/src/runtime/interpreter/dispatch_virtual.rs`: the drifted twin

The headline fix. Two edits in one hunk, at `:476-503`.

**exact literal OLD text:**
```rust
            {
                const MAX_DEPTH: usize = 32;
                const PROXY_INSTANCE: &str = "java/lang/reflect/Proxy$Instance";
                let mut current = Some(receiver_class_id);
                let mut is_proxy = false;
                for _ in 0..MAX_DEPTH {
                    let cid = match current {
                        Some(c) => c,
                        None => break,
                    };
                    let class = match cm.get_class(cid) {
                        Some(c) => c,
                        None => break,
                    };
                    if &*class.name == PROXY_INSTANCE {
                        is_proxy = true;
                        break;
                    }
                    if &*class.name == "java/lang/Object" {
                        break;
                    }
                    current = class.superclass;
                }
```

**exact literal NEW text:**
```rust
            {
                const MAX_DEPTH: usize = 32;
                let mut current = Some(receiver_class_id);
                let mut is_proxy = false;
                for _ in 0..MAX_DEPTH {
                    let cid = match current {
                        Some(c) => c,
                        None => break,
                    };
                    let class = match cm.get_class(cid) {
                        Some(c) => c,
                        None => break,
                    };
                    // `class_name_is_proxy_super`, NOT a local
                    // `Proxy$Instance` literal. The super of a generated
                    // `$ProxyN` is `java/lang/reflect/Proxy` by default
                    // (`CRATONVM_REAL_PROXY_SUPER` is truthy-default-true),
                    // so a one-name test recognises no shipped proxy and
                    // this guard never fires — see
                    // docs/known-issues/jdk-only/F32-1-the-proxy-route-and-the-drifted-twin-20260813.md.
                    // The shared predicate is name-only and takes NO lock,
                    // which is what makes it callable under the `cm` guard
                    // this block holds (a nested second read self-deadlocks
                    // under parking_lot's writer-preferring fairness — the
                    // original reason this walk was inlined at all).
                    if class_name_is_proxy_super(&class.name) {
                        is_proxy = true;
                        break;
                    }
                    if &*class.name == "java/lang/Object" {
                        break;
                    }
                    current = class.superclass;
                }
```

No import is needed: `dispatch_virtual.rs:24` is `use super::*;` and
`interpreter.rs:7788` is `pub use typecheck::*;`, so the `pub(super)`
predicate is already in scope. **Verified by reading both lines**, not
assumed.

**Blast radius, stated because it is a route switch and not only a bug fix.**
Applying N1 moves steady-state proxy dispatch from the generated bytecode
*back* onto the shim — the route F19's `vm_exec.rs` fix corrects. The two
changes therefore belong together, and applying N1 **without** F19's
`proxy_box_value_for_desc` change would turn `proxy.boolTRUE` and the five
still-fresh arms red. The reverse is not true: F19's change alone is
inert-but-correct on this path. If a lane must pick one order, land F19 first.
`RJdkReflBox --only=proxy` before and after N1 is the discriminator, and with
N3's warm-up it is a clean one.

**Also worth measuring, not asserted here:** the July AOP symptom
(`AspectJAutoProxyCreatorTests.twoAdviceAspectPrototype` /
`twoAdviceAspectSingleton`, "advice silently not firing on the second proxy
instance") is what this guard was written to stop. If those tests are green
today, that is evidence the generated bytecode's `Proxy$Dispatch.invokeProxy`
route reaches the handler correctly — i.e. the drift is a boxing divergence
and not a dispatch one. If they are red, N1 is urgent.

### N2 — `native-builtins/src/reflect_annotations.rs`: count and log the other two degrade stages

This lane instrumented `Failed("emit")` because that stage lives in its file.
The other two, and the `Degrade` arm, are still silent by default. Smallest
shape, mirroring §5.1: a counter per outcome beside `PROXY_INSTANCES_CREATED`
(`:3253`) with a reader, bumped in `define_or_get_proxy_class`'s four exits
(`:3851`, `:3920`, `:3933`, `:4015`), and — at
`native_proxy_new_instance:3200`, the point where the shim is actually
substituted — one unconditional `eprintln!` per **distinct** interface set
(not per instance; a proxy-heavy app would flood). Do **not** flip
`real_proxy_strict`'s default as part of it; §5.2 lists what that changes, and
the counters are how to size it first.

Second, independent item in the same file: `Proxy.getProxyClass` throws
`IllegalArgumentException` for `Degrade` **and** `Failed` (`:3309`) while
`newProxyInstance` degrades silently for both. One of the two is wrong; they
are 30 lines apart.

### N4 — `docs/known-issues/jdk-only/INDEX.md`: add the row for this record

Not applied: another lane is mid-edit on `INDEX.md` in this worktree
(`git diff --stat` shows 161 changed lines there), and a blind append would
race it. Row for the **JIT / typecheck** table:

```
| F32-1-the-proxy-route-and-the-drifted-twin-20260813 | which route answers a proxy call; the one-name copy of a two-name predicate | OPEN | READ | fix nominated (N1), not applied — different file |
```

### N3 — `regression-suite/src/RJdkReflBox.java`: make the `proxy` family warm

Not this lane's file. Per §2.3 the first call at a cold call site takes the
shim and every later call takes the generated bytecode, so a family that calls
each shape **once** samples only the cold end and cannot distinguish F19's
three outcomes. Call each proxy shape twice and assert on the second:

```java
// exact shape, per row: discard a warm-up call, then assert.
// The interpreter's call-site inline cache is cold on the first
// invocation, which takes a different boxing route from every later
// one (F32-1 §2.3). One call per row measures the cold route only.
h.boxInt(7);                       // warm-up, result discarded
ck("proxy.int", h.boxInt(7) == Integer.valueOf(7));
```

Optionally add a `proxy.coldVsWarm` row that records whether the two calls
agree — on HotSpot they always do (MEASURED: identity is stable across calls
for every cached row), so a CratonVM disagreement is a one-row diagnosis of
the whole route split.

---

## 9. Residuals

* **Nothing in this record was observed on CratonVM.** §2 is a chain of source
  readings; §2.3 is a prediction; §3's HotSpot half is measured and its
  CratonVM half is a comparison of emitted bytecode, not of behaviour. One
  run of `RJdkReflBox --only=proxy` falsifies or confirms the lot.
* Both edited files parse-check clean under `rustfmt --emit stdout` on a
  **copy** in the scratchpad (rc=0 both), remain pure CRLF (2,978 / 1,648
  lines, zero bare LF, verified with `tr -cd '\r' | wc -c` — **not**
  `grep -c $'\r'`, which matches every line in this shell), and no added line
  exceeds 100 columns. Neither was compiled: `cargo` is the orchestrator's.
* §3.1's `UndeclaredThrowableException` gap is noted and **not diagnosed**.
  Whether `Proxy$Dispatch.invokeProxy` performs the wrap that HotSpot's
  emitted exception table performs was not checked.
* `class_chain_reaches_proxy_instance`'s `MAX_DEPTH = 32` is unchanged and
  unexamined. A proxy chain is two hops, so it is not load-bearing here.
* `vm/src/runtime/proxy.rs`'s `is_proxy_class_name` is a **third** one-name
  predicate (`PROXY_INSTANCE_CLASS` only). It was not touched: its callers
  (`Proxy.isProxyClass`, per its doc) ask a different question — "is this the
  shim class itself" — and folding it into `class_name_is_proxy_super` would
  make `Proxy.isProxyClass(Proxy.class)` answer true. Named here so the next
  lane does not "unify" the three by name.
* The `$Proxy` substring arm at `typecheck.rs:857` and
  `class_chain_reaches_proxy_instance` at `:915` are two fail-open hatches
  over the same population, the first firing first. `W8-C10-1` kept the name
  test deliberately (`interpreter/tests.rs:108` calls it a documented
  lenience), so it is left alone — but with real generated proxies named
  `jdk/proxyM/$ProxyN` (MEASURED on HotSpot; CratonVM uses the same scheme per
  `build_proxy_spec_for`), the name test now covers the population the chain
  walk was added for, and one of the two is redundant. Not adjudicated.
