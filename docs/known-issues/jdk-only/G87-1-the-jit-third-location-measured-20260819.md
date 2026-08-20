# G87-1 — the JIT "third location", measured under strict mode

**Status:** MEASURED. No code changed. Closes `G86-1` N2 as a measurement; does
NOT close the P0 row.
**Provenance:** `--jdk-only --jdk-only-report` over the three JIT-exercising
vectors (`RJitGc`, `RSyncMethodJit`, `RJitStringLayout`), CratonVM
`C:/craton/target-nolto`, 2026-08-19.

---

## 0. Why this was the row's real open question

`G86-1` established that the P0 *Duplicate dispatch* row's "two lists that
disagree" premise is stale — one predicate since 2026-08-04, and unreachable
under `--jdk-only` because its guard needs a `SyntheticStub` and strict mode
registers none. What it explicitly did NOT establish was the row's third
location: seven "thin direct call" ladders in `jit/src/lib.rs` that bake
VM-side reimplementations of registered natives into emitted code, plus five
JIT-reachable paths that bypass `resolve_dispatch` and `record_invocation`.

`G86-1` N2 said so: *"that is where duplicate dispatch still literally exists,
and it is not measured by anything this session ran."* This is that
measurement.

## 1. The counters, under JIT-heavy vectors in strict mode

| vector | `jit_direct_native_binds` | `jit_inline_cache_natives` | `jit_fastpath_admissions` |
| --- | ---: | ---: | ---: |
| `RJitGc` | 0 | 0 | 2 |
| `RSyncMethodJit` | 0 | 0 | 0 |
| `RJitStringLayout` | 0 | 0 | **38** |

**Read the third column carefully — I nearly did not.** It sits in the report
under `refusals`, and its accessor is
`jdk_only_jit_fastpath_refusals()`, documented as *"Number of JIT by-name
native fast-path admissions REFUSED under `JdkOnly`."* So 38 is not 38
shortcuts taken; it is 38 shortcuts **blocked**. The field name in the JSON
(`jit_fastpath_admissions`) reads as the opposite of what it counts.

So all three columns say the same thing: under `--jdk-only` the JIT's by-name
native shortcuts either never happen (0 direct binds, 0 inline-cache natives)
or are actively refused (38).

`RJitStringLayout` is a fair test of this: 1,750,874 intrinsic invocations in
that run, so the JIT was thoroughly exercised.

## 2. What that means for the row

**The third location is live in `--real-jdk`, not in `--jdk-only`.** That is the
same shape `G86-1` found for the allow-list: the mechanism the row is worried
about exists, and strict mode already neutralises it — the allow-list because
no `SyntheticStub` registers, the JIT shortcuts because they are refused.

The row therefore has a much narrower strict-mode surface than its text
suggests. What remains genuinely open in it:

* the **default mode**, where 1330 stubs register and the JIT shortcuts are
  admitted. That is where "duplicate dispatch implementations" still literally
  exists;
* `bytecode_available: false` in `resolve_step1_native`, still open by the
  row's own text and untouched here;
* the required resolution — ONE policy-aware resolver used by interpreter,
  JIT, reflection, JNI and method handles, with a lint forbidding direct
  registry lookup outside it. That does not exist, and no measurement makes it
  exist.

**This does not close the row**, and the distinction matters: strict mode being
clean is not the same as the duplication being gone. The row is about there
being several implementations of one decision. There still are — they are just
all refusing, in the one mode I measured.

## 3. NOMINATIONS

**N1 — rename `jit_fastpath_admissions` in the report, or document it at the
field.** It counts REFUSALS and lives under `refusals`, but the name reads as
admissions, and a reader checking whether strict mode is leaking JIT shortcuts
would draw exactly the wrong conclusion from a non-zero value. §1 nearly caught
me.

**N2 — measure the same three counters under `--real-jdk`.** That is where the
row's remaining content lives, and this session measured only strict. The
numbers would give the default-mode half of the row an evidence base it does
not currently have.

**N3 — 39 `native-shadows-bytecode` violations in a single JIT vector.** Up
from 104 crate-wide in `G84-1`'s `RJdkStrict` run, 39 of them appear in
`RJitStringLayout` alone. Whatever the JIT is doing to string layout is
shadowing real bytecode nearly forty times in one run, and that IS the row's
subject even in strict mode. Worth its own probe.
