# JIT drops a constructor's side effects when the object does not escape

**Status:** **FIXED 2026-08-12** on `fix/jit-ctor-side-effects-20260812`
(see "Resolution" at the bottom). Found while triaging
[investigate-batch-12.md](investigate-batch-12.md)
(`io.netty.util.concurrent.FastThreadLocalTest`), but this is **not a netty
bug and not netty-specific** — it is a general JIT correctness defect that
silently produces wrong results in any compiled method.

## Symptom

A `new X()` whose result is never used, but whose **constructor writes global
state**, has that write silently discarded once the enclosing method is
JIT-compiled. No exception, no diagnostic — the program simply computes the
wrong answer.

## Minimal reproducer

No netty, no dependencies. `probe/EA.java`:

```java
import java.util.concurrent.atomic.AtomicInteger;

public class EA {
    static final AtomicInteger ATOMIC = new AtomicInteger();
    static int plain;
    static class WithAtomic { final int id; WithAtomic() { id = ATOMIC.getAndIncrement(); } }
    static class WithPlain  { final int id; WithPlain()  { id = ++plain; } }

    public static void main(String[] a) {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 1000000;
        for (int i = 0; i < n; i++) { new WithAtomic(); }
        for (int i = 0; i < n; i++) { new WithPlain(); }
        System.out.println("atomic ctor = " + ATOMIC.get());
        System.out.println("plain  ctor = " + plain);
    }
}
```

```bash
javac EA.java
/data/toolchain/jdk-25/bin/java -cp . EA                                    # HotSpot
<cratonvm> --java-home /data/toolchain/jdk-25 -cp . EA                      # JIT on
<cratonvm> --java-home /data/toolchain/jdk-25 --nojit -cp . EA              # interpreter
```

| run | `ATOMIC.get()` | `plain` |
|---|---|---|
| HotSpot JDK 25 | 1000000 | 1000000 |
| CratonVM, JIT on | **3000** | **0** |
| CratonVM, `--nojit` | 1000000 | 1000000 |

Both counters must be `1000000`. The JIT-on numbers are not stable across
runs (an earlier run of the netty-backed variant lost 989 000 of 1 000 000,
a rerun lost 998 000) — they track the point at which the loop is compiled,
which is exactly the tell: **the interpreter executes the constructor, the
compiled code does not.** The plain-`static int` case loses *every*
increment; the `AtomicInteger` case keeps only the pre-compilation ones.

Note also the wall time: the JIT-on run finished the 1 000 000-iteration loop
in 6–33 ms versus HotSpot's 137 ms. Being *faster than C2* on an allocation
loop is itself the signal that the work is not being done.

## Root cause

`jit/src/x64/driver.rs`, building the shape map the single-pass backend's
escape analysis consumes:

```rust
is_trivial_void_init: info.method_name == "<init>"
    && info.descriptor == "()V",
```

The flag is named *trivial* but the predicate only inspects the **signature**.
Any no-argument `()V` constructor is classified trivial regardless of what its
body does.

`jit/src/x64/escape_analysis.rs` then treats such a call as a no-op — it pops
only the receiver and does **not** mark it escaped:

```rust
Some(shape) if shape.is_trivial_void_init => {
    // `<init>()V`: pop only `this`; receiver does not escape.
    abs_stack.pop();
}
```

The receiver is therefore scalar-replaceable, the allocation is removed, and
the `<init>` call goes with it — taking the constructor's writes to static
state along with it. `WithPlain(){ id = ++plain; }` is a no-arg `()V`
constructor, so it qualifies.

## The correct predicate already exists in this tree

The **IR / tier-2 pipeline does this correctly**. It elides an `<init>` only
when a resolver has proven the constructor body is genuinely empty —
`vm/src/runtime/interpreter/jit_bridge.rs::is_elidable_construction`, reached
through `resolve_jit_elidable_init`, which `jit/src/ir.rs` describes as
admitting "only a 5-byte `aload_0; invokespecial Object.<init>()V; return`
body". `jit/src/lib.rs` feeds those proven pcs to the IR builder as
`trivial_init_pcs` via the `cp_elidable_init_resolver` closure.

So the defect is that the **single-pass x64 backend uses a signature check
where the IR backend uses a body check** — and per
`jit/src/escape_analysis.rs`'s own module docs, the single-pass one "runs for
the overwhelming majority of compiled methods".

## Fix (as applied)

Thread the elidability decision into the single-pass backend rather than
re-deriving it from the descriptor:

- `cp_elidable_init_resolver: Option<&dyn Fn(u16) -> bool>` is already a
  parameter of `jit/src/lib.rs::try_compile`, and the `scan.invoke_ops` list
  already carries `(pc, cp_idx, opcode)` — the same two inputs the IR path
  uses at `lib.rs:15471` to build `trivial_init_pcs`.
- Compute the elidable pc set once and pass it down to
  `x64::compile`/`driver.rs` alongside `non_escaping_new`, and set
  `is_trivial_void_init` from that set instead of from `info.descriptor`.
- When no resolver is supplied, `is_trivial_void_init` must default to
  **false** (keep the call), which is the safe direction — the IR path already
  reasons this way: "Calling an `<init>` runs every side effect the elision
  path was allowed to skip, which is the safe direction."

A cheap interim tightening (correctness-only, no plumbing) is to additionally
require `info.class_name == "java/lang/Object"`. Tightening this predicate can
only ever remove an elision, so it cannot introduce a miscompile; the cost is
losing scalar replacement for genuinely-empty user constructors until the
resolver is wired, which is a **performance** regression that needs measuring
before it is chosen over the real fix.

## Impact

Severity is high and not limited to tests: any hot method that constructs a
short-lived object whose constructor registers it, counts it, assigns it an
id, or otherwise mutates shared state will silently skip that work once
compiled. Common shapes that match:

- id/sequence assignment in a constructor (`id = NEXT.getAndIncrement()`),
- self-registration (`REGISTRY.add(this)` — though `this` escaping there
  should force an escape, an `<init>` classified trivial is never examined),
- instance counters and metrics.

### The netty class it was found through

`io.netty.util.concurrent.FastThreadLocalTest.testConstructionWithIndex`
loops until a counter advanced *by the constructor* reaches its limit:

```java
while (nextIndex.get() < ARRAY_LIST_CAPACITY_MAX_SIZE) {
    new FastThreadLocal<Boolean>();
}
```

`FastThreadLocal()` is a no-arg `()V` constructor whose body is
`index = InternalThreadLocalMap.nextVariableIndex()`. Once the loop is
compiled the increments stop, `nextIndex` stops advancing, and the loop
**never terminates** — the class is recorded as a HANG (rc=124) rather than
as a wrong answer. Measured directly: 1 000 000 `new FastThreadLocal()` calls
advanced `nextIndex` by 11 000 with the JIT on and by exactly 1 000 000 with
`--nojit`.

**Fixing this bug alone will probably not turn that class green.** The loop is
~2.1 billion iterations by construction (`Integer.MAX_VALUE - 8`); at the
`--nojit` rate measured on this host (389 k ctor/s) it needs ~5500 s, and even
the partially-warmed HotSpot rate (7.3 M ctor/s) implies ~294 s. HotSpot runs
the whole class in ~50 s, so C2 is optimising the loop far harder once warm.
Treat `testConstructionWithIndex` as needing *both* this correctness fix and
a throughput story; the other 12 tests in the class are unaffected and pass.

## Cross-check

HotSpot JDK 25 on the identical classpath gets both counters right, and
CratonVM `--nojit` gets both right — so this is a CratonVM JIT defect, not a
test bug and not a JDK-behaviour difference.

## Related

- `docs/known-issues/netty/jni-native-codec-sigsegv-20260812.md` — the other
  open netty finding (unrelated mechanism).
- Fixed in the same session, for the same two batch pages:
  the `StackWalker$Option` enum defect and the synthetic `CyclicBarrier`
  lost-release defect (see the branch's commit messages).


## Resolution (2026-08-12)

Implemented as described above: the resolver-proven pc set is now threaded into
the single-pass backend instead of being re-derived from the descriptor.

* `jit/src/x64/driver.rs` — `compile_with_param_slots` takes a new trailing
  `elidable_init_pcs: Option<HashSet<usize>>`, and `is_trivial_void_init` is set
  from it. `None` means "the caller proved nothing", and nothing may be elided.
* `jit/src/lib.rs` — `try_compile_inner` builds the set from
  `cp_elidable_init_resolver` over `scan.invoke_ops`, the same two inputs the IR
  builder already uses for its `trivial_init_pcs`.
* `vm/src/runtime/interpreter.rs` and
  `vm/src/runtime/interpreter/jit_bridge.rs` — **the other two production
  doors**. Both already called `is_elidable_construction` per `<init>()V` site
  (to rewrite the emitted class to `java/lang/Object`); they now also collect
  those pcs and pass them down, so these paths keep scalar replacement for
  genuinely-empty constructors rather than losing it.

Only that ONE predicate needed correcting. `plan_scalar_replacement`'s matching
`<init>()V` check (which populates `init_skips`, the set
`bytecode_walk.rs` consults to actually skip emitting the call) fires only for
receivers already in `non_escaping_new`, so fixing the single source of truth
keeps the allocation and its constructor call consistent by construction — no
second patch, and no change to the scalar-replacement tests.

**A grep would have missed two of the three doors.** `cargo check -p
cratonvm-jit` passed while the VM crate still failed to compile; the two extra
call sites were found only because the new parameter made them a type error.
That is the `compile_gate` "three doors" hazard the driver's own comments warn
about, working as intended.

### Verification

| check | before | after |
|---|---|---|
| `EA` ctor → `static AtomicInteger` (n=1 000 000) | 3 000 | **1 000 000** |
| `EA` ctor → plain `static int` (n=1 000 000) | **0** | **1 000 000** |
| `FastThreadLocal` ctor → `nextIndex` (n=1 000 000) | 11 000 | **1 000 000** |
| HotSpot / `--nojit` on the same probes | 1 000 000 | 1 000 000 (unchanged) |

The optimisation is **not** disabled — only narrowed to what is provable.
`CRATONVM_DBG_SCALAR_DEOPT=1` over a method that allocates in a loop shows a
constructor writing a static reported as `non_escaping_new=[]` (allocation and
call both kept) while a genuinely empty constructor in the same binary is still
`non_escaping_new=[7]` (scalar-replaced). The empty-ctor loop still runs ~11x
faster with the JIT than interpreted.

Regressions: `cargo test -p cratonvm-jit --lib` — 1990 passed, 0 failed. The
19 netty classes of batch-12/13 are unchanged at 13/19.

### Still open: `FastThreadLocalTest`

As predicted above, this fix alone does **not** turn that class green, and it
was never expected to. `testConstructionWithIndex` loops
`Integer.MAX_VALUE - 8` (~2.1 billion) times by construction; the counter now
advances correctly, so the loop terminates in principle, but not inside the
suite's wall cap. That remains a throughput item, tracked with the class in
[investigate-batch-12.md](investigate-batch-12.md) — not a correctness one.
