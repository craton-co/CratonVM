# A lambda callee's deopt is orphaned by an identity check that compares the SAM's name, and its side effect runs twice

| | |
|---|---|
| **Status** | OPEN. Reproducible **12 of 12**, with a control in the same process and the same run that is correct 12 of 12. |
| **Severity** | Silent wrong answer — a side effect committed by a compiled lambda body runs a second time. No exception, no log line, no crash. |
| **Opened** | 2026-09-08 |
| **Witness** | `vm/tests/jit_lambda_door_deopt_resumes.rs` (`#[ignore]`d — it fails, and it is meant to) + `vm/tests/resources/cratonvm/DeoptLambdaRerunCount.java` |
| **Family** | The 2026-09-07 deopt-sink set: `deopt-sink-refused-a-frame-its-sibling-resumes-FIXED-20260907.md` and `jit-bridge-sinks-re-ran-a-side-effecting-body-FIXED-20260907.md`. **This is not one of those and is not fixed by them.** |

## The measurement

One fixture, two arms, identical shape, same process, same run — a COMPILED
caller invoking a COMPILED callee whose body commits a side effect
(`SIDE_EFFECTS[0]++`, an `iastore`) and then traps (`i / d` with `d == 0`, which
the IR tier lowers to a deopt guard rather than a throw). The only difference is
how the callee is reached.

| arm | callee reached by | side effect ran | |
|---|---|---:|---|
| `stepStatic` → `implStatic` | `invokestatic` | **1** | correct |
| `step` → `OP.apply` → `lambda$static$0` | SAM call on a lambda | **2** | **the store ran twice** |

**12 runs, 12 times the same split.** The control is the point: it is not a
timing artefact, a threshold, or the acceptance gate — those would move both
arms together.

```
lambda site: calls=0 direct=0 no_code=0 refused=0 deopted=0 |
             resumed=0 unresumable=0 | delta=2 delta_static=1
```

## The cause, from the VM's own trace

`CRATONVM_DBG_DEOPT=1`, the two arms side by side:

```text
# static arm — correct
helper precise-resume of trapped callee
    cratonvm/DeoptLambdaRerunCount.implStatic:(II)I at bci=14

# lambda arm — orphaned
callee-resume refused (stash is not this call site's callee):
    stash=cratonvm/DeoptLambdaRerunCount.lambda$static$0:(II)I  site=apply(II)I
ORPHANED at the serviced call site apply(II)I:
    stash=cratonvm/DeoptLambdaRerunCount.lambda$static$0:(II)I bci=14
sink=jit-callsite-a    running= stash=…lambda$static$0:(II)I bci=14
sink=lambda-oneshot    running= stash=…lambda$static$0:(II)I bci=14
```

`try_resume_trapped_callee` (`vm/src/jit/helpers.rs`) decides whether a stashed
frame belongs to the call site being serviced by comparing names:

```rust
if key_method != info.method_name || !descriptors_match_modulo_return(key_desc, info.descriptor)
{
    trc("stash is not this call site's callee", …);
    return None;
}
```

For an ordinary call those two ARE the same method. **For a lambda they never
are**: `info.method_name` is the SAM — `apply` — while the frame the compiled
body stashed names the synthetic impl, `lambda$static$0`. The check therefore
refuses **every** lambda callee's deopt, unconditionally and by construction,
and the frame is dropped as an orphan. Control then falls through to
`jit-callsite-a` and `lambda-oneshot`, neither of which can claim a frame that
has already been taken, and the body is re-entered from bci 0 — running the
`iastore` it had already committed.

The refusal is not wrong to exist. Its own comment records why: resuming a
foreign frame is what produced the Groovy "duplicate `main` method" failures
that forced the `5ceb880f` revert. It is asking the right question with the
wrong pair of names.

## Why the 2026-09-07 fixes do not cover it

They widened the *admission* at four sinks that were declining a resume they
could perform. This one never reaches those sinks with a claimable frame: the
frame is orphaned one level earlier, in the dispatch helper, before any sink
sees it. `lambda_site_*` confirms the lambda-site door is not involved either —
`calls=0` across every run, so the counters that
`jit_bridge_sink_resumes_instead_of_rerunning.rs` asserts on are untouched here
too.

## The fix, specified but not taken

The check needs the identity of the method the call site actually RESOLVED to,
not the name it was written with. At a lambda site that is the impl, and
`try_lambda_site_direct_call` already knows it — it resolved it to make the
direct call. Threading that through to the resume check (or comparing against
the resolved callee recorded on the site rather than against `info`) is the
shape of the fix.

**Not taken here, deliberately.** This is a hot dispatch helper and the check it
touches is load-bearing for a documented silent-corruption regression, so it
needs the whole gate set — including `regression-suite` on both collectors —
and the build host could not produce a release binary at the time this was
found. The witness is checked in and `#[ignore]`d so the next person starts from
a reproduction rather than from this page.

## Reproducing

```bash
cargo test -p cratonvm-vm --test jit_lambda_door_deopt_resumes -- --ignored --nocapture
```

Prints the `lambda site:` line above. `delta=2 delta_static=1` is the defect;
`delta=1 delta_static=1` is the fix.

**One ordering caveat, and it cost an hour.** The lambda arm must run BEFORE the
control's warm-up. With the control's 12 800 extra warm calls in front of it the
lambda arm reported `delta=1` — so a version of this probe that warms everything
up front measures nothing. The test runs the arms in the order that keeps the
lambda arm cold, and says so at the call site.

## Related

- `deopt-sink-refused-a-frame-its-sibling-resumes-FIXED-20260907.md` — the
  tier-up sink, and where the family's argument about `can_deopt_resume` is set
  out.
- `jit-bridge-sinks-re-ran-a-side-effecting-body-FIXED-20260907.md` — the three
  sinks that re-ran from entry, and the fixture (`DeoptRerunCount`) this one's
  static control is modelled on.
- `lambda-sam-dispatch-bypasses-the-cached-invoke-path-20260817.md` — the
  earlier finding that lambda dispatch takes a path of its own, which is the
  same structural reason this check sees the wrong name.
