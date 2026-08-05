# `CRATONVM_BG_COMPILE=0` poisoned `ConstantDescs` — the first-call compile door ran a `<clinit>` — FIXED

**Status: FIXED (2026-08-05).** The documented `CRATONVM_BG_COMPILE=0` opt-out
failed **100 % of runs** on any classpath that touches `java.lang.invoke`, with
an `ExceptionInInitializerError` that named a class nobody had asked for. Root
cause is not in the background pipeline and not in codegen: entering a
first-call-compiled artifact hoists **every** class-initialization trigger in
the body to method entry, and the historical inline path applied that to
`<clinit>` bodies — inverting a JDK circular-init cycle whose whole
correctness is its order.

Found while reducing the `OffsetDateTimeTest` discovery crash
(`../hibernate/offsetdatetimetest-junit-discovery-nullptr-sigsegv-20260805-FIXED.md`),
where `CRATONVM_BG_COMPILE=0` was reached for as a control arm and failed
20/20 — briefly looking like a 100 % reproduction of that bug. It is a
different defect; that page records it as unusable and points here.

## Symptom

```
<clinit> failed — wrapping in ExceptionInInitializerError
  class=jdk/internal/constant/PrimitiveClassDescImpl
  cause=java/lang/NullPointerException
    at java/lang/constant/DynamicConstantDesc.<init>(DynamicConstantDesc.java:89)
    at sun/invoke/util/Wrapper.<clinit>(Wrapper.java:37)
    at java/lang/invoke/MethodTypeForm.<init>(MethodTypeForm.java:176)
    ...
    at java/lang/invoke/VarHandles.makeArrayElementHandle(VarHandles.java:221)
```

## Minimal reproduction

Four lines, no Hibernate, no H2, no JUnit. Deterministic — 0/3 with the flag,
3/3 without, 3/3 with `--nojit`:

```java
VarHandle vh = MethodHandles.arrayElementVarHandle(int[].class);
```

```
CRATONVM_BG_COMPILE=0 cratonvm --java-home <jdk25> -cp . BgClinitProbe
```

| arm | result |
|---|---|
| default (background pipeline) | OK 3/3 |
| `CRATONVM_BG_COMPILE=0` | **FAIL 3/3** |
| `CRATONVM_BG_COMPILE=false` | **FAIL 3/3** |
| `CRATONVM_BG_COMPILE=0` + `--nojit` | OK 3/3 |
| `--nojit` | OK 3/3 |

`java -Xshare:off` on stock HotSpot 25 does **not** reproduce it, contrary to
the claim in `ensure_class_initialized_shared`'s existing
`PrimitiveClassDescImpl` guard comment. That comment describes a *different*
entry into the same cycle (Spring's ClassFile `MethodTypeDescImpl.ofDescriptor`
route, which reaches `PrimitiveClassDescImpl` first); this one enters through
`ConstantDescs`, which is the benign edge, so the guard is not at fault and was
never consulted here.

## The cycle, and why the order is the whole story

`java/lang/constant/ConstantDescs.<clinit>` is correct only when executed in
source order:

```java
line 198:  BSM_PRIMITIVE_CLASS = ofConstantBootstrap(CD_ConstantBootstraps, "primitiveClass", CD_Class);
   ...
line 249:  CD_int = PrimitiveClassDescImpl.CD_int;   // first touch of PrimitiveClassDescImpl
```

and `PrimitiveClassDescImpl.<clinit>` reads back the field assigned at 198:

```java
private PrimitiveClassDescImpl(String descriptor) {
    super(ConstantDescs.BSM_PRIMITIVE_CLASS, requireNonNull(descriptor), ConstantDescs.CD_Class);
```

`DynamicConstantDesc.<init>:89` is `requireNonNull(bootstrapMethod)`. Reach
line 249's trigger before line 198 has run and that `requireNonNull` throws.

## What the evidence said, in the order it said it

Three levers were tried before anything was read, and two of them proved the
first hypothesis wrong rather than confirming it:

* Raising `CRATONVM_TIER_OSR_THRESHOLD` / `TIER_OSR_BACKEDGE` /
  `JIT_THRESHOLD` / `TIER_C1|C2_THRESHOLD` to a billion: **all still FAIL**. So
  not invocation-counted tier-up and not OSR — the two things
  `CRATONVM_BG_COMPILE=0` is documented to re-enable.
* `CRATONVM_JIT_DENY=java/lang/String.`: compiled-method count drops to **zero**
  and it **still FAILs**. So no compiled body is executing the damage.
* `CRATONVM_DISABLE_JIT=1`: **OK**. So the JIT is nevertheless required.

`CRATONVM_DBG_CLINIT_ORDER` (added by this fix — see below) then diffed the
two arms:

```
bg=ON    > ConstantDescs   from depth=12 [Wrapper.<clinit>@46 <- ...]
           > ClassOrInterfaceDescImpl   from depth=13 [ConstantDescs.<clinit>@2   <- ...]
           > ArrayClassDescImpl         from depth=14 [ClassOrInterfaceDescImpl.arrayType@2]
           > ConstantUtils              from depth=13 [ConstantDescs.<clinit>@304 <- ...]
           > DirectMethodHandleDesc$Kind from depth=14 [ConstantDescs.ofConstantBootstrap@39]
           > MethodTypeDescImpl          from depth=14 [ConstantDescs.ofConstantBootstrap@47]
           > DirectMethodHandleDescImpl  from depth=15 [MethodHandleDesc.ofMethod@80]
           > PrimitiveClassDescImpl      from depth=13 [ConstantDescs.<clinit>@579 <- ...]

bg=OFF   > ConstantDescs   from depth=11 [Wrapper.<clinit>@46 <- ...]
           > ConstantUtils              from depth=11 [Wrapper.<clinit>@46 <- ...]
           > ClassOrInterfaceDescImpl   from depth=11 [Wrapper.<clinit>@46 <- ...]
           > PrimitiveClassDescImpl     from depth=11 [Wrapper.<clinit>@46 <- ...]
```

Two facts, both decisive:

1. **The frame depth never moves** (11 throughout) and the trigger frame never
   advances past `Wrapper.<clinit>@46`. `ConstantDescs.<clinit>` is not
   executing its own statements — these initializations are being driven from
   Rust, before bytecode zero.
2. `MethodTypeDescImpl` and `DirectMethodHandleDescImpl` — the machinery line
   198 needs — are **never initialized at all** in the failing arm.

A `CRATONVM_DBG_STATIC_WATCH` on `ConstantDescs` (temporary, since removed)
closed it: **285** static operations in the passing arm, **2** in the failing
one — and both of those are *reads*, from `PrimitiveClassDescImpl`'s
constructor, both null. `ConstantDescs.<clinit>` executed **zero putstatics**.

`CRATONVM_DBG_CLINIT_FAIL=1`'s Rust backtrace then named the exact line:
`interpreter::execute` calling `ensure_class_initialized_shared` — before any
bytecode of the `<clinit>` it was about to run.

## Root cause

`vm/src/runtime/interpreter.rs`, the eager first-call compile door. After
compiling, and before entering the artifact:

```rust
// Ensure all classes referenced by static field ops are initialized.
// The JIT directly accesses static field memory, bypassing the
// interpreter's ensure_class_initialized_shared call.
for &cid_raw in &compiled.static_init_classes { ensure_class_initialized_shared(...)? }
```

`static_init_classes` is the declaring class of **every** getstatic/putstatic
site anywhere in the body (`x64/driver.rs`, from the already-resolved
`static_field_info`). For an ordinary method, hoisting those to method entry is
a sound approximation of JVMS §5.5 — it can only initialize a class the body
was going to initialize anyway. For a `<clinit>` it is not an approximation of
anything: it runs the triggers *in the one order the initializer exists to
prevent*.

For `ConstantDescs.<clinit>` (52 920 bytes of compiled code, first-call
compiled only under the opt-out) the list contains `PrimitiveClassDescImpl`
from line 249. So:

1. `ConstantDescs.<clinit>` is invoked and compiled.
2. The pre-walk initializes `PrimitiveClassDescImpl` — before bytecode zero.
3. Its constructor reads a still-null `ConstantDescs.BSM_PRIMITIVE_CLASS`
   → NPE → `ExceptionInInitializerError`.
4. `ConstantDescs.<clinit>` never runs a single statement; both classes are
   poisoned (`NoClassDefFoundError`) for the rest of the process.

22 of the 38 first-call compiles in the failing arm were `<clinit>` bodies.

## The fix

`<clinit>` no longer takes the eager first-call compile door
(`vm/src/runtime/interpreter.rs`), joining the existing static-policy /
ForkJoin / native-shadow skips and their seal.

Refusing costs nothing: a `<clinit>` runs at most once per class per loader, so
a method-entry compile can never amortize its own codegen. A `<clinit>` with a
genuinely hot loop still reaches the **OSR** door, which enters mid-body and
does not run this pre-walk.

**Measured blast radius — the default configuration is untouched.** The
background pipeline performs *zero* first-call compiles at this door, so the
change is a no-op unless the opt-out is set:

| binary / arm | `<clinit>` first-compiles | total first-compiles |
|---|---:|---:|
| pre-fix, default | 0 | 0 |
| pre-fix, `BG_COMPILE=0` | 22 | 38 |
| fixed, default | 0 | 0 |
| fixed, `BG_COMPILE=0` | **0** | 7 |

### Residual, deliberately not fixed here

The pre-walk remains a §5.5 hoist for **ordinary** methods: a method that
merely mentions a class's statics initializes it on entry, not at the
triggering instruction. That is only observable through a circular-init cycle
entered from a method that is itself a member of the cycle, which is what the
`<clinit>` case was. Making it strictly correct means *declining to enter* the
artifact when a `static_init_classes` entry is not yet initialized (interpret
this call, compile-enter later) rather than initializing it — a change that
risks stranding a method with a rarely-taken branch in the interpreter forever,
and which needs its own measurement. Recorded here rather than attempted.

## Verification

| binary | default | `CRATONVM_BG_COMPILE=0` |
|---|---|---|
| unmodified dev (`8aa7ca97a`) | OK | **FAIL** |
| fixed | OK | **OK** |

On the original Hibernate classpath (`DiscoveryProbe` on `OffsetDateTimeTest`,
`CRATONVM_BG_COMPILE=0`): **0/5 clean before, 5/5 clean after**.

Suite state, `apps/hib-suite-runner`, `passed.txt[0..60)`, real JDK, JIT on:

| arm | result | wall |
|---|---|---|
| default | **60/60 PASS** | 3m35s |
| `CRATONVM_BG_COMPILE=0` | **60/60 PASS** | 3m50s |

The second row is the point of the fix: the opt-out could not previously
complete a single class on this classpath. The ~7 % wall difference between the
arms is the inline-tier-up path doing its own codegen on the mutator and is not
attributable to this change (which only *removes* compiles).

`cargo test -p cratonvm-vm --lib` — 2401 passed, the same 2 pre-existing
`native::jni` `/OPT:ICF` failures as on unmodified `dev`, documented in
`../../native-call-funnel-per-call-floor-item2-20260805.md`.

`vm/tests/clinit_first_call_compile_order.rs` pins both flag arms — the default
arm too, so a future change that moves the first-call door under the background
pipeline cannot reintroduce this silently. **Verified RED on the pre-fix
binary** (`missing 'varHandle' line — the JDK constant-descriptor <clinit> cycle
did not complete`) and green on the fixed one. It asserts the descriptor text
of `CD_int`/`CD_boolean`, not merely non-null, so the same defect surviving as
a wrong value rather than a throw still fails it.

`CRATONVM_DBG_CLINIT_ORDER` is kept and registered as a `DBG` flag token. Its
sibling `CRATONVM_DBG_CLINIT_FAIL` reports a backtrace for an initializer that
*threw*; this reports the ORDER, which is the only question a circular-init
cycle can be debugged by, and this family has now bitten at least twice.
