# CHM `get()` misses a key the same VM just stored — in-process only

**Status: OPEN, characterised, not fixed. 2026-08-03.**
**Fails:** `test_chm_pre_resize_put_get`, `test_chm_boxed_cache_is_vm_scoped`
(`vm/tests/wp4_6_chm_basic.rs`).

## The shape

`ChmBasicProbe.testChmPreResizePutGet` does 11 `put`s into a
`ConcurrentHashMap<String,Integer>`, checks `size() == 11`, then `get`s each key
back. Eleven entries stays under the `0.75 × 16 = 12` load threshold, so
`transfer()` never runs — this is deliberately the *pre-resize* subset, the one
that is supposed to work even while the post-resize path is broken
(`test_chm_basic_put_get` is already `#[ignore]`d for that).

The method cannot return 0: it returns `1`, `-1`, `-100 - i`, `-200 - i`,
`-991..-994`, or throws.

## Three data points, and they do not agree

| how it is run | result | means |
|---|---|---|
| `cratonvm` CLI, `--java-home <jdk-25.0.3+9>` | **1** | passes |
| in-process `test_vm()`, no `JAVA_HOME` | **0** | a value the method cannot return |
| in-process `test_vm()`, `JAVA_HOME` set (and with an explicit `with_java_home`) | **-100** | `size()==11` but `get("k0")` is `null` |

Reproduce the passing arm:

```bash
javac --release 21 -d /tmp/chmcls vm/tests/resources/cratonvm/ChmBasicProbe.java ChmMain.java
./target/release/cratonvm --java-home $JDK25 -cp /tmp/chmcls cratonvm.ChmMain
# pre-resize=1  mutation=1  clear=1
```

## What each result says

* **CLI = 1.** Real `java.util.concurrent.ConcurrentHashMap` bytecode, executed
  by this VM, is correct for this workload. So this is not a CHM-semantics bug.
* **In-process, no `JAVA_HOME` = 0.** 0 is unreachable from the Java source, so
  the *return value itself* was lost. That is the K1 signature the probe's own
  header names — `Value::Object(None)` / `Uninitialized` reaching `pop_int` and
  becoming 0.
* **In-process with a real JDK = -100.** A different failure, and the more
  informative one: `put` × 11 then `size() == 11` (so the entries are there),
  but `get("k0")` returns `null`. A store that the map counts and cannot find
  again points at key hashing/equality or bucket indexing, not at storage.

The two in-process results being *different* is the useful part: pointing
`test_vm()` at a real JDK changes the failure mode rather than removing it, so
"the synthetic stand-in is wrong" does not explain it on its own.

## Why this is not simply "the in-process VM boots the synthetic JDK"

That was the first hypothesis and it is not sufficient. `VmConfig::new()` is not
the CLI's configuration — a known trap in this tree — but adding
`with_java_home(...)` to `test_vm()` was tried and moved the answer from 0 to
-100 without fixing it. Whatever the CLI does that the in-process VM does not is
still unidentified; the CLI also sets things like `CRATONVM_REAL=net-sockets,aqs`
in the suite runners, and its native-registration phases may not match.

**The next step is to diff the two configurations**, not to change CHM. Boot the
in-process VM with the CLI's exact config and bisect the difference until the
result flips to 1.

## What was done here

Both tests are `#[ignore]`d with a reason pointing at this file, matching how
`test_chm_basic_put_get` in the same file already handles the sibling defect.
They are not deleted and not weakened: un-ignore them the moment the
configuration difference is found.
