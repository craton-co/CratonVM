# `--jdk-only` could not start a thread: §7 step 3 fell through to `UnsatisfiedLinkError`, not to bytecode

**Status:** FIXED 2026-08-04 on `fix/jdk-only-wave2-retire-20260804`.
Pre-existing on `dev`; reproduced identically on an unmodified `dev` binary
before attributing.

## The symptom

Under `--jdk-only` against a real JDK 25 image, this VM could not run a single
`java.lang.Thread`:

```
WARN cratonvm_vm::vm::vm_exec: Missing native method in real-JDK mode
     method=java/lang/Thread.run()V
Thread Thread-2 terminated with error:
     InternalError(Runtime(UnsatisfiedLinkError { message: "java/lang/Thread.run()V" }))
```

`java/lang/Thread` is `boot-image` (`--dump-class-origins` confirms it, with
`real_bytes_found: true`), and the class-path image declares `run()V` with a
`Code` attribute and **no** `ACC_NATIVE` flag. There was nothing missing.

Consequences, all silent-to-hanging rather than diagnostic:

* every `new Thread(…)` died on entry, so every `ExecutorService` had zero live
  workers;
* a workload that then `join()`ed or `Future.get()`s **hung** rather than
  failed — which reads as a slow run, not a broken one;
* the same fall-through produced *wrong values* elsewhere:
  `FileChannel.size()` after `FileChannelImpl.open(…)` returned `0` where
  compatible mode and HotSpot both returned `32`.

The last one is the reason this was worth chasing past the thread failure: the
defect's usual presentation is a wrong number, not an exception.

## Why it happened

`invoke_on_class_shared_inner`'s `is_native` is **not** `method.is_native()`.
The `check_override` chain also sets it for concrete methods whose real bytecode
the VM deliberately shadows — that is the chain's entire purpose, and
`Thread.run()V` is one of its entries.

The `if is_native` arm then resolves the native through
`resolve_native_dispatch_wave1`. Under `JdkOnly`, §7 step 3 says concrete
bytecode beats a shadowing native, so the resolver correctly returned `None`.
The code's own comment described what happened next:

> JdkOnly, §7 step 3: this "native" is a bridge standing in front of concrete
> bytecode. Fall through to the JNI chain, **and past it to the bytecode path**,
> exactly as an unregistered native would have.

It could not. The `if is_native` arm has exactly three outcomes — registry
native, JNI function pointer, `UnsatisfiedLinkError` — and no path to the
bytecode, which lives in the *outer* `else`. Declining the native inside the arm
falls through to the link error, not to the `Code` attribute.

This is worth naming as a shape, because the comment is the kind that stops
review: it states the correct policy, cites the right clause, and describes a
control flow the surrounding code does not have. Nothing about it reads as
wrong until you follow the `else` chain to its end.

## The fix

Hoist the §7 resolution **above** the `if is_native`, and clear `is_native` when
a shadowing native lost to bytecode, so the bytecode branch becomes reachable:

```rust
let (class_name, native_shadows_bytecode, registry_native) = if is_native {
    … the existing prelude, unmoved …
} else {
    (String::new(), false, None)
};

let is_native = is_native && !(registry_native.is_none() && native_shadows_bytecode);
```

Two properties make this narrow rather than sweeping:

* **`Compatible` is bit-for-bit unchanged.** `native_shadows_bytecode` is
  `strict && …`, so under `Compatible` the added term is `false && …` and folds
  away. Verified as well as argued — see below.
* **Genuinely unimplemented natives still raise.** If `is_native` came from
  `method.is_native()`, then `native_shadows_bytecode` is `false` by
  construction, since `!m.is_native()` is one of its conjuncts. An `ACC_NATIVE`
  method with nothing behind it takes the error arm exactly as before, including
  one a host library would have served over JNI — a `RegisterNatives` target is
  `ACC_NATIVE` too.

## Evidence

`probes/JdkOnlyCensusLoadProbe.java`, nine sections across collections,
interfaces, streams, `Properties`, io, nio, net, executors and text. Run
interleaved (`--jdk-only`, `--real-jdk`, `--jdk-only`) with a HotSpot 25
control, JDK 25 image, 2026-08-04:

| section | HotSpot 25 | `--real-jdk` | `--jdk-only` before | `--jdk-only` after |
|---|---|---|---|---|
| net | `read=4` | `read=4` | **thread dead → hang** | `read=4` |
| concurrent | `sum=4324` | `sum=4324` | **timeout** | `sum=4324` |
| nio | `size=32` | `size=32` | **`size=0`** | `size=32` |
| collections / interfaces / streams / io / text | pass | pass | pass | pass |
| sections failed | 0 | 0 | **2** | 0 |

The remaining divergence is `sysprops=45` against HotSpot's `49`, which is
present in **both** CratonVM modes and is therefore not a strict-mode defect.

`Compatible`-mode regression: all ten `test_classes` entries run under the old
(`dev`) and new binaries, stdout+stderr and exit status compared. Three differed
on ISO timestamps only; with timestamps and numerals normalised, **0 of 10**
differ.

### Repeated, because a single pass hid a second defect

Eight runs per arm rather than one, after a lone verification run came back with
a truncated transcript that a first reading took for a regression:

| binary | mode | completed 9/9 | hung | other |
|---|---:|---:|---:|---:|
| this branch | `--jdk-only` | 6 | 1 | 1 |
| this branch | `--real-jdk` | 6 | 2 | 0 |
| `dev` | `--jdk-only` | **0** | 0 | **8** |
| `dev` | `--real-jdk` | 6 | 2 | 0 |

The `dev` / `--jdk-only` row is this defect at full strength: eight runs, none
of them clean, 16 `UnsatisfiedLinkError` lines apiece, `net` and `concurrent`
both dead, `nio` returning `0`, and four system properties missing. **0/8 → 6/8**
is what the fix buys.

The residual 2/8 is a **different, pre-existing, mode-independent** defect —
the `dev` binary hangs at the same rate under `--real-jdk`, which no
`JdkOnly`-gated change can cause. Filed separately as
[bounded socket operations hang about one run in five](fixed-suite-bugs/net/bounded-socket-operations-hang-FIXED-20260805.md) (fixed 2026-08-05),
with a HotSpot control (12/12 clean at the same host load) that is what
distinguishes it from contention.

Worth stating plainly: a *single* verification run would have reported this fix
as a regression, because the one sample it drew happened to be a hang.

### The probe had to be made hang-proof first

The first two attempts at the strict census produced no census at all: the run
hung, `timeout` sent `SIGKILL`, and the exit hook never wrote the file. An
unbounded `accept()` / `Future.get()` turns "the VM cannot start threads" into
"the harness produced nothing", which is indistinguishable from a
still-running job. The probe now bounds every blocking call
(`ServerSocket.setSoTimeout`, `Socket.connect(…, timeout)`,
`Future.get(n, SECONDS)`) and reports `SECTION-FAILED` per section with a
`sections=/failed=` tally on the last line, so a truncated run cannot be read as
a clean one.

## How this was found

Not by looking for it. It fell out of the schema-3 census
(`docs/known-issues/jdk-only/native-kind-is-ambient-and-defaults-to-syntheticstub.md`),
which adjudicates every registration against the bytes on the class path. The
three `java/lang/Thread` rows read:

| row | kind | image says |
|---|---|---|
| `start0()V` | `bridge` | `acc_native: true` — a genuine bridge |
| `start()V` | `bridge` | `acc_native: false, has_code: true` — a shadow |
| `run()V` | `bridge` | `acc_native: false, has_code: true` — a shadow |

The instrument was built to size a reclassification wave. It named the failing
dispatch on its first run, before anyone asked it to.

## Related

* `docs/feature-designs/jdk-only-mode.md` §1.4, §7 step 3 — the policy this
  implements.
* `docs/known-issues/jdk-only/native-kind-is-ambient-and-defaults-to-syntheticstub.md`
  — item 1, the census that surfaced it. The 4,796 `Bridge` registrations that
  shadow concrete bytecode are all candidates to reach this same path.
