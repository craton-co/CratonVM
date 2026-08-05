# Retired 2026-08-04: CHM `get()` missed a key the same VM had just stored

Branch `fix/chm-inprocess-get-miss-20260804`. Retires
`docs/known-issues/vm/chm-get-misses-stored-key-in-process-20260803.md` and,
with it, the sibling `WP4.6-FOLLOWUP-A` label ("CHM `transfer()` data-loss
after resize past 16 buckets"). Both named `ConcurrentHashMap`. Neither was a
`ConcurrentHashMap` defect.

**The defect was in String equality, one call below the map:**
`NativeContextImpl::java_strings_equal` answered `Some(false)` — an
authoritative *"these are different strings"* — for two Strings whose
character storage it had not managed to read.

## What the known-issue file asked for, and why that was the wrong next step

> **The next step is to diff the two configurations**, not to change CHM. Boot
> the in-process VM with the CLI's exact config and bisect the difference until
> the result flips to 1.

The configuration difference is real and it is the trigger, but chasing it
would have produced a config knob, not a fix. The configuration only decides
*which `java/lang/String` gets loaded*; the bug is that one of this VM's own
readers treats a layout it does not recognise as a comparison it has performed.
That reader is wrong under any configuration that can produce such a layout.

## The three data points, re-measured

Run on Azure (16-core Linux), `cargo test --release -p cratonvm-vm --test
wp4_6_chm_basic`, `origin/dev` at `98f00873c`:

| arm | doc said | measured 2026-08-04 |
|---|---|---|
| in-process `test_vm()`, no `JAVA_HOME` | 0 | **0** |
| in-process `test_vm()`, `JAVA_HOME=jdk-25.0.3+9` | -100 | **0** |
| `test_chm_resize_path` (Integer keys, 64 entries, `#[ignore]`d for `WP4.6-FOLLOWUP-A`) | — | **passes** |

That third row is the one that mattered and nobody had run it: a probe ignored
for a *resize* defect passes, while its String-keyed siblings fail **below** the
resize threshold. Whatever this is, it is not resize, and it is not shared by
both key types.

`JAVA_HOME` no longer moves the answer, so the doc's most-quoted observation —
"pointing `test_vm()` at a real JDK changes the failure mode rather than
removing it" — does not reproduce. `VmConfig::new()` keeps
`use_synthetic_jdk = true`, and `with_java_home` deliberately does not change
that (`vm/src/config.rs`), so the JDK image was never in play on that arm.

## The `0` that "the method cannot return" was a stale `.class` file

The doc's sharpest argument was arithmetic:

> The method cannot return 0: it returns `1`, `-1`, `-100 - i`, `-200 - i`,
> `-991..-994`, or throws. […] 0 is unreachable from the Java source, so the
> *return value itself* was lost. That is the K1 signature.

It was not. `javap -c` on the **committed** `vm/tests/resources/cratonvm/
ChmBasicProbe.class` shows a `testChmPreResizePutGet` that is an older revision
of the method beside it — no staged codes, no `throws Throwable`, just
`iconst_0; ireturn` on every failure and one `Class java/lang/Throwable`
handler over the whole body. `0` was that method's ordinary failure return.

`vm/build.rs` compiles `tests/resources/**.java` into `OUT_DIR` and the harness
puts `CRATONVM_TEST_CLASSES_DIR` **first** on the classpath, so the committed
`.class` only runs when `javac` was unavailable — and then it runs a *different
method body than the source next to it*. `docs/internal/fixed-suite-bugs/
wp4-6-chm-boxed-values.md` had already recorded this exact trap ("The committed
fallback `ChmBasicProbe.class` hid the real failure by catching the VM
exception and returning `0`") in a previous investigation of the same file. It
hid the next one too.

Both passes are compiled in ONE `javac` invocation each, so a single
uncompilable fixture takes the whole batch down and every test silently falls
back to committed classes. That is how it happened here: the first attempt
resolved a `javac` that could not compile `PatternComplete.java`.

`ChmBasicProbe.class` has been regenerated from its own source so the two
agree.

## Root cause

`ConcurrentHashMap.get` on a String key reaches
`native_chm_get` → `chm_seg_get` → `map_keys_equal` →
`NativeContext::java_strings_equal`, and the VM's implementation was:

```rust
fn java_strings_equal(&self, a: ObjectRef, b: ObjectRef) -> Option<bool> {
    if !is_real_java_string(self.shared, a) || !is_real_java_string(self.shared, b) {
        return None;
    }
    Some(compact_java_strings_equal(self.shared, a, b))   // <- `bool`, not `Option<bool>`
}
```

`compact_java_strings_equal` read slot 0 as `value:[B` and slot 1 as `coder:B`
— the JDK-9+ compact layout — and `return false` on anything else. The
in-process VM does not boot that String. Its `java/lang/String` is a fabricated
stub (`is_synthetic_stub = true`, two `Ljava/lang/Object;` slots named `_f0`
and `_f1`) whose value array is a `char[]` and whose **slot 1 holds the cached
hash**. Traced on the failing run:

```text
[chm] seg_get seg=0x… hash=0xd25 cap=4 idx=1
      chain=[("0xd25", Some("k0"), …), ("0xd29", Some("k4"), …), ("0xd2d", Some("k8"), …)]
[chm]   cmp key=0x…(Some("k0")) node_key=0x…(Some("k0"))
        strings_equal=Some(false) cid=(ClassId(30),ClassId(30)) eq=Ok(false)
[chm]   key slots 0=Object(…array…) 1=Int(3365) | node_key slots 0=Object(…array…) 1=Int(3365)
```

Everything upstream is correct: same hash (`0xd25` = `"k0".hashCode()` spread),
same segment, right bucket, and the node with key `"k0"` is *in the chain the
walk examined*. The comparison then said "not equal" about two objects it had
declined to read — `3365` is not a valid coder, so the reader bailed, and
bailing was spelled `false`.

`map_keys_equal` treats `Some(_)` as the answer and only falls through to
`String.equals` on `None`, so the miss was final. `containsKey` reported
`false` for the same reason; `keySet()` walks chains without comparing keys,
which is why the map could list all eleven keys it could not find.

### Why the CLI passed and why HashMap looked healthy

* **CLI** (`--java-home <jdk-25>`): real `java.lang.String`, `byte[]` + valid
  `coder`, so the positional reader was right and the comparison was real. The
  known-issue file read that as evidence that "real CHM bytecode is correct for
  this workload"; in fact `ConcurrentHashMap` is a registered Rust native in
  both modes (`register_concurrent_hashmap_natives`) and the same native code
  ran on the passing arm.
* **`HashMap`** on the identical keys in the identical VM: green throughout.
  Its String fast path (`native_hashmap_get_string_fast`) compares
  `ctx.read_string(node_key) == key_text` — decoded text, layout-agnostic — and
  never asks `java_strings_equal` at all. CHM's fast path
  (`native_chm_get_string_fast`) instead opens with
  `ctx.java_string_hash_code(key)?`, which returned `None` on this layout and
  dropped the lookup into the general path, where the lying comparator was.
  Two collections, one heap, one key: the divergence was always in the
  comparator, not the container.

## Fix

`vm/src/vm/vm_exec.rs`:

* New `java_string_storage` locates a String's character array and its
  encoding (`LATIN1` / `UTF16` bytes / `char[]` units). It probes slots 0 and 1
  first — unchanged cost for every String in a real-JDK run — and, only when
  that does not describe a String, resolves `value` and `coder` **by name** off
  the receiver's own class.
* `compact_java_string_hash` and `compact_java_strings_equal` are both built on
  it. `compact_java_strings_equal` now returns `Option<bool>`, and
  `java_strings_equal` propagates that: `None` means *"not compared — dispatch
  `String.equals`"*. Every caller already handled `None` that way
  (`map_keys_equal`, `native_chm_get_string_chain`,
  `native-builtins/src/intrinsics/record.rs`).
* The `NativeContext` trait docs for `java_strings_equal` /
  `java_string_hash_code` now state the contract that was violated: an
  implementation must never report `false` for a pair it did not read.

Because the same resolver backs the hash, the CHM String fast path now engages
on the fabricated layout too instead of falling into the general path.

## Verification

* `cargo test --release -p cratonvm-vm --test wp4_6_chm_basic --
  --include-ignored`: **6 passed, 0 failed**, both with committed fixtures and
  with `build.rs`-compiled ones (`JAVA_HOME=jdk-25.0.3+9`). All four
  `#[ignore]`s are removed.
* Three new unit tests in `vm_exec.rs` pin the contract directly against the
  embedded String layout: equal strings compare equal, different strings
  compare unequal, and a String whose storage cannot be located answers `None`
  rather than `Some(false)`.
* Full `cargo test --release -p cratonvm-vm` compared against an
  `origin/dev` worktree — see "Suite delta" below.

## Suite delta

`cargo test -p cratonvm-vm --no-fail-fast`, debug profile, Windows, both arms
at merge base `d81e220b3`, `JAVA_HOME` = jdk-25.0.3.9 so `build.rs` staged
fresh fixtures:

| | passed | failed |
|---|---|---|
| `origin/dev` | 3992 | 9 |
| with fix | 4001 | 8 |

`+9` passing is exactly the four un-`#[ignore]`d CHM tests plus the four new
`vm_exec` unit tests, plus one net swap in the failing set.

**Seven failures are shared by both arms** and none is touched by this change:
`t9b_inline_constant_native_census` (328 registrations vs a ceiling of 327, all
in `native-builtins`/`native-io`), `custom_loader_metadata_is_reclaimed_{with,
without}_jit`, `arena_segment_allocator_default_methods_dispatch_to_native`,
`ir_exception_stub_stamps_this_methods_throw_bci`, `real_fjp_path`,
`stackwalker_log4j_deep_repeated_walks_finish_under_jit`.

**The three that differ are all `cratonvm`-binary-spawning probes, and the
difference is the binary, not the code.** `vthread_probe_regression`'s
`cratonvm_binary()` prefers `target/release` over `target/debug`: the baseline
worktree still held a **release** binary from a previous session, the fix
worktree held only the debug binary this session built. Re-run with
`CRATONVM_BIN` pinned to each arm's *debug* binary, `vthread_probe_10000_all_
increment` **times out at 60s on both arms** — 3/3 each. The two that failed
only on the baseline (`cov06_array_allocation_end_to_end`,
`ir_athrow_method_is_entered_through_dispatch`) spawn a binary the same way and
are the same artefact in the other direction. Pin `CRATONVM_BIN` before reading
any arm-to-arm delta in these four suites.

Also green: `cargo test -p cratonvm-native-collections -p cratonvm-native-api
--lib` (273 + 94), and the real-JDK CLI arm — `cratonvm --java-home <jdk-25>
-cp … cratonvm.ChmMain` prints `pre-resize=1 basic=1 resize=1 mutation=1
clear=1`, i.e. the arm that already passed is unchanged.

Earlier, on Azure release builds at the branch point,
`wp4_6_chm_basic --include-ignored` was 6/6 with committed fixtures and 6/6
with `build.rs`-compiled ones.

**Not confirmed: the release profile on the FINAL code.** The Azure box became
unusable partway through (sshd timing out during banner exchange under load,
then dropping every session long enough to build; with no other login session
open, `systemd-logind` SIGTERMs any detached build the moment ssh exits). The
6/6 release runs above predate the two follow-up corrections to this change —
the element-vs-unit span and the `ObjectKind::Array` guard — so everything
measured on the final tree is debug-profile. Nothing here is profile-sensitive
in principle (no JIT surface, no timing), but that is an argument, not a
measurement. Re-run `cargo test --release -p cratonvm-vm --lib --test
wp4_6_chm_basic` on a working Linux host to close it.

## What to carry forward

* **A comparator that cannot read its operands must say so.** `Option<bool>`
  where `None` means "not compared" is the shape; `false` is an answer and
  costs a present key its identity. The same trap is live in any
  `fn(..) -> bool` that opens with a layout guess.
* **A positional field read is a claim about a class you did not look at.**
  `value@0` / `coder@1` is true of the JDK's String and of nothing else this VM
  boots. This tree already carries the `CHM_FIELD_SEGMENT_MASK` version of the
  same lesson.
* **Run the ignored control before believing the ignored suspects.**
  `test_chm_resize_path` was ignored for `WP4.6-FOLLOWUP-A` and passed on
  first run; that single fact separated "String keys" from
  "ConcurrentHashMap" and made the rest mechanical.
* **A committed fallback `.class` is a second, unreviewed copy of the test.**
  When it disagrees with the `.java` beside it, every conclusion drawn from a
  run without `javac` is about a program nobody read.
* **A binary-spawning probe compares worktrees, not commits.** Four suites here
  pick `target/release` over `target/debug` and will happily measure a binary
  from a different session and a different branch. Pin `CRATONVM_BIN`, or the
  A/B is between two build profiles.
