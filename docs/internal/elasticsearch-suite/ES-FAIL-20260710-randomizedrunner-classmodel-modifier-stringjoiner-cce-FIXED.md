# ES suite-wide bootstrap failure: `ClassCastException` in `Modifier.toString`/`StringJoiner.add` blocks every `RandomizedRunner`-based test class

Status: FIXED

Found: 2026-07-10, while verifying
`docs/known-issues/elasticsearch-suite/ES-FAIL-20260710-executors-factory-synthetic-mainlock-npe-FIXED.md`
against a full ES suite run. Confirmed **unrelated** to that fix (reproduced identically with and
without it).

## Symptom (before fix)

Every ES test class using `com.carrotsearch.randomizedtesting.RandomizedRunner` (i.e. essentially
the entire suite — confirmed across `client/rest`, `action.admin.cluster.storedscripts`, and other
unrelated modules in a 60-class spot check) failed immediately with `initializationError`, before
any test method ran:

```text
java.lang.ClassCastException: java.util.ArrayList cannot be cast to [Ljava.lang.String;
	at java.util.StringJoiner.add(StringJoiner.java:191)
	at java.lang.reflect.Modifier.toString(Modifier.java:233)
	at java.lang.reflect.Field.toGenericString(Field.java:381)
	at com.carrotsearch.randomizedtesting.ClassModel$2.compare(ClassModel.java:34)
	...
	at com.carrotsearch.randomizedtesting.RandomizedRunner.<init>(RandomizedRunner.java:321)
```

## Root cause

Bisected to dev commit `aa21e334` ("Fix Spring SpEL evaluation edge cases"). That commit added
`vm/src/runtime/interpreter.rs::synthetic_stub_should_yield_to_real_bytecode`, a new check applied
at the interpreter's own per-frame dispatch loop (`try_stackless_invoke`) that makes a
`SyntheticStub`-registered native yield to real JDK bytecode whenever the receiver's class is
loaded as a genuinely real (non-`is_synthetic_stub`) class with real bytecode for the method —
mirroring an allowlist that `vm/src/vm/vm_exec.rs::invoke_or_native` (a *different*, pre-existing
dispatch path) had used safely for a while. The new function's allowlist mirrored that older one
verbatim, including `java/util/StringJoiner`.

`native-collections/src/lib.rs` registers a `SyntheticStub` fallback implementation of
`StringJoiner` (`register_string_joiner_stub_natives`) using a legacy 5-field layout (`delimiter`,
`prefix`, `suffix`, an internal `ArrayList` of elements, `emptyValue`) that predates the modern
real JDK `StringJoiner`'s actual field layout (`prefix`, `delimiter`, `suffix`,
`elts: String[]`, `size`, `len`, `emptyValue`). Before `aa21e334`, every `StringJoiner` method call
— `<init>`, `add`, `toString`, etc. — consistently used this native, so despite the field-index
mismatch relative to the *real* class's layout, execution was internally self-consistent (the
native always read back exactly what it itself had written) and never crashed.

`aa21e334` made **path 1** (the interpreter's own dispatch loop) additionally prefer real bytecode
for `StringJoiner` whenever the loaded class is real (which it always is in real-JDK mode) — this
was verified, via targeted debug instrumentation, to apply *consistently* to every `StringJoiner`
method (`<init>`, `add`, `toString` all correctly "yield" to real bytecode). The problem is **not**
dispatch inconsistency between construction and method calls, as first suspected — a standalone
repro confirmed real bytecode runs consistently throughout construction and use.

Instead, running real `StringJoiner` bytecode for the **first time ever** on CratonVM (it was
always shadowed by the native before `aa21e334`) exposed a **separate, deeper, deterministic
heap-reference-integrity defect**: the `gen_heap::read_slot` "corrupt Value cell (out-of-range
discriminant)" guard (see `HIB-CV-32`) fires reading `StringJoiner`'s own `size`/`elts` fields back
after a `putfield`, specifically from the second `add()` call onward (`elts[size++] = elt`'s
`dup_x1`/`iadd`/`putfield`/`aastore` sequence). This does **not** reproduce for an equivalent
user-defined class with the identical bytecode shape and field count/layout — confirmed via two
standalone `MicroProbe`/`MicroProbe2` repros (one matching the bytecode pattern, one additionally
matching `StringJoiner`'s exact 7-field layout) that both ran correctly with zero corruption. The
defect is specific to `java.util.StringJoiner` being a natively-registered bootstrap class, not a
general interpreter bug in the bytecode pattern itself — the exact mechanism was not further
pinned down (filed separately, see below).

## Fix

`vm/src/runtime/interpreter.rs`: excluded `java/util/StringJoiner` from
`synthetic_stub_should_yield_to_real_bytecode`'s allowlist, reverting **only this one class, only
at this one (new, `aa21e334`-added) dispatch point** back to the proven-safe pre-`aa21e334`
behavior — the `SyntheticStub` native wins uniformly for `StringJoiner` at the interpreter's own
dispatch loop, avoiding the newly-exposed heap corruption entirely. `vm/src/vm/vm_exec.rs`'s
separate, older `invoke_or_native` allowlist (which also lists `StringJoiner`, and which the
`aa21e334` commit did not touch) is unaffected by this change.

This is a knowingly narrow, minimal-blast-radius fix: the other 8 classes `aa21e334` added to this
same allowlist (`ReentrantLock`, `LinkedBlockingDeque`, `AtomicBoolean`, `EnumSet`,
`FileInputStream`, `Cleaner`, `Cleaner$Cleanable`, `ManagementFactory`) are untouched, since
whatever specific Spring SpEL edge case `aa21e334` fixed for those classes is presumed to still
need this treatment and no equivalent corruption was found for them in this session.
`StringJoiner`'s own `SyntheticStub` native was independently confirmed (via reflection-based field
dumps) to *already* silently produce incorrect `toString()`/`add()` results in real-JDK mode
**identically both before and after `aa21e334`** — i.e. this fix restores exactly the prior status
quo bit-for-bit, introducing no new regression; it does not newly break or newly fix
`StringJoiner`'s own content-correctness, which was already broken. See the new, separate doc filed
for that: `docs/known-issues/stringjoiner-synthetic-native-real-jdk-field-mismatch.md`.

## Verification

- Standalone probes (`SJProbe.java`, `SJProbe2.java` — direct `StringJoiner` usage,
  `Modifier.toString`, `Field.toGenericString`) all run to completion with no exception and no
  `gen_heap::read_slot`/`GC-ARRAY-GUARD` diagnostics, both with and without `--nojit`.
- ES suite: the exact repro from this doc (`-Start 1 -Count 6`, `client/rest` module — all 6
  classes previously failed with this `ClassCastException`) now shows 6/6 `PASS`.
- A 60-class broader sweep (`-Start 1 -Count 60`) shows the identical PASS/FAIL/HANG pattern
  recorded in a prior, independent session's pre-`aa21e334` baseline sweep for the same range —
  confirming the fix returns the suite to its exact pre-regression state, zero new failures
  introduced.

## Reproduction (historical, now fixed)

```powershell
pwsh -NoProfile -ExecutionPolicy Bypass -File apps/elasticsearch-suite-runner/run-elasticsearch-suite.ps1 -Category all -Jit on -Vm craton -ElasticsearchRoot "<es-checkout>" -Exe <cratonvm-binary-built-from-dev-tip> -JdkHome /usr/lib/jvm/java-21-openjdk-amd64 -TimeoutSec 60 -RunName repro-cce -ModeName repro-cce -Start 1 -Count 1
```

Collection context:
- Host: `victor@20.83.144.174`
- Worktree: `/data/data/cratonvm-worktrees/20260710-144656-fix-stringjoiner-cce`, branch
  `fix/stringjoiner-cce-modifier-tostring-20260710`
- Standalone probes: `/tmp/sjprobe/{SJProbe,SJProbe2,MicroProbe,MicroProbe2}.java` on the
  collection host
