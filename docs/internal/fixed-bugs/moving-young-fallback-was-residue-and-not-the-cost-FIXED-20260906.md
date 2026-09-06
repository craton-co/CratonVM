# The `[moving-young]` fallback was 100% residue — and it was not what the Kafka test was dying of

| | |
|---|---|
| **Status** | CLOSED 2026-09-06. The named mechanism is FIXED; the cost attribution is REFUTED by the arm the page never ran. |
| **Was** | `known-issues/springboot/moving-young-jit-frame-fallback-costs-3-10x-20260906.md` (OPEN, "throughput defect, 3-10x") |
| **Successor** | the Kafka method's real failure is a different defect, filed separately — see "What is actually wrong with that test" below. |

## What the page claimed

That `KafkaAutoConfigurationIntegrationTests#testEndToEndWithRetryTopics` fails
under `--XX:UseGc Generational` because the young collector keeps falling back
to the non-moving sweep with
`reason=unregistered-jit-frame-on-stack`, that this costs 3-10x, and that it is
a throughput defect and not a correctness one. Its evidence was one ablation:
`--nojit` removes both the warnings and the failure.

## Two of those are wrong, and one arm settles it

`CRATONVM_NO_MOVING_YOUNG=1` holds the young collector permanently in EXACTLY
the state a `[moving-young]` fallback puts it in — non-moving sweep, free-list
allocation — and removes the probe that produces the warning. If the fallbacks
were the cost, that arm is the worst case. It is not:

| arm | rep 1 | rep 2 | `[moving-young]` warnings |
|---|---|---|---|
| Generational, default | FAIL 164 s | FAIL 125 s | 11 / 10 |
| Generational `CRATONVM_NO_MOVING_YOUNG=1` | FAIL 195 s | FAIL 140 s | **0** / **0** |

Same failure, same order of magnitude, zero fallbacks. The fallback is a
co-symptom of running compiled code on this workload, not the cause of anything
the test measures.

The page reached the opposite conclusion because `--nojit` moves BOTH variables
at once — it removes the compiled frames the probe trips on AND everything else
the JIT does. `NO_MOVING_YOUNG` moves only the first.

## And the failure is not a throughput failure

The page describes the assertion as `latch.await(30, SECONDS)` returning false.
That is not what fails now:

```text
org.springframework.kafka.KafkaException: Send failed
Caused by: UnknownTopicOrPartitionException: This server does not host this topic-partition
...
Caused by: java.io.IOException: FileChannel.map: invalid fd
  at AbstractIndex.createMappedBuffer
Caused by: TimeoutException: Topic testRetryTopic not present in metadata after 60000 ms
```

The embedded KRaft broker never gets its topics into metadata, and in about half
the runs its index mmap fails outright because `FileChannel.map` is handed a
receiver whose `fd` field does not read back as a non-negative int. A test that
cannot publish is not a slow test.

## The named mechanism, measured and fixed

The refusal the page is named for comes from the A5 raw-word probe in
`refresh_moving_young_coverage_for_current_thread`. That probe's own doc comment
has named cutting its false-positive rate as the designated follow-up since it
was written, and `scan_active_jit_frames` — the OTHER call site of the same scan
— has carried the screen for it since the H2 `FileNioMapped.unMap`
investigation: a compiled method that has returned leaves a return address into
JIT code at every depth below its own `entry_sp`, and the one live guardless
frame the probe exists for (the process entry point) sits ABOVE every JIT entry
the run ever makes.

The coverage probe did not have that screen. It now does, and the population it
converts was never in doubt once counted:

```text
[a5-fallback] #1 slot=0x72faaa7edb18 word=0x72fab0dca3c1
              residue_hi=0x72faaa7f11bf is_residue=true shapeless=true
              filtered=false band=[0x72faaa7ea540,0x72faaa7fd000) chain_len=0
```

**Every sampled hit on this workload is residue** — 26 of 26 with the filter on,
25 of 25 with it off. With the screen armed the
`reason=unregistered-jit-frame-on-stack` refusal disappears from the coverage
probe entirely and the next reason (`cross-thread-jit-peer`) becomes the binding
one.

That is the fix, and it is worth having on its own terms — a false positive
costs the young generation its copying collector for that cycle — but note what
it does NOT do: it does not make this test pass, because the fallback was never
why the test failed.

The screen is applied at the coverage probe only. `scan_active_jit_frames` still
marks `UNREGISTERED_JIT_FRAME` on its own terms (it accepts every hit while the
entry chain is non-empty, because there the fail-safe direction is to MARK), so
the reason can still appear from that site.

### Flags

| flag | default | what it does |
|---|---|---|
| `CRATONVM_JIT_A5_RESIDUE_FILTER` | on | `0` accepts every hit again — restores the defect |
| `CRATONVM_JIT_A5_SHAPE_FILTER` | off | `1` also suppresses hits at a slot with no frame shape |
| `CRATONVM_DBG_A5_FALLBACK` | off | the per-hit lines above, plus a totals line on BOTH VM exit arms |

The frame-shape screen stays OFF. `a5_slot_has_frame_shape` was written as a
PRICING instrument whose own doc says it is "never [used] to suppress a hit that
passes", it costs the global code-range lock the probe's fast path exists to
avoid, and on this workload the residue screen already converts everything it
would.

## What is actually wrong with that test

Established with same-time controls, one process at a time, round-robin so a
drift in host load lands on every arm equally — which matters, because the
page's own `--nojit` control failed once at 414 s in an uncontrolled pass and
passed at 68 s in a controlled one:

| arm | result | wall |
|---|---|---|
| HotSpot `jdk-25` | PASS | 14 s, 17 s, 16 s, 14 s |
| CratonVM **ZGC** (shipped default) | PASS | 57 s, 60 s |
| CratonVM **Generational**, `--nojit` | PASS | 68 s |
| CratonVM **Generational** | **FAIL** | 302 s, 329 s, 108 s |

So: not the box, not CratonVM in general, and the page's ablation is real.

`CRATONVM_DBG_SWEEP_ZERO=1` then names the defect in one line:

```text
[sweep-zero] RECLAIMED-LIVE receiver ptr=0x77d695f6a090:
  original class=sun/nio/ch/NativeThreadSet (class_id=3299 kind=0x00),
  zeroed by non-moving sweep cycle 22; invoked as sun/nio/ch/NativeThreadSet.add
  — the live ref was a register/native-stack root the marker missed
```

One object — the `NativeThreadSet` a `FileChannelImpl` keeps its blocking-I/O
threads in — reclaimed while LIVE by the young sweep on cycle 22, then invoked
three times (`add`, `remove`, `signalAndWait`) against zeroed memory. Downstream
that is `FileChannel.map: invalid fd`, the broker's index mmap failing, topics
that never reach metadata, and `Send failed`.

**This inverts the page's verdict.** It closes with

> the young collector's refusal to move is the sound response to it — which is
> why this is filed as throughput and not as corruption.

The non-moving sweep is not the sound response here; it is where the live object
is destroyed. That is also exactly why `CRATONVM_NO_MOVING_YOUNG=1` does not
help — it does not avoid the fallback state, it makes the fallback state
permanent — and why the page's own triage rule ("single digits harmless,
thousands a death spiral") could never have found this: the count of fallbacks
was never the quantity that mattered.

The surviving defect is therefore a JIT root-coverage gap under the Generational
collector's non-moving young sweep, not a `[moving-young]` throughput problem.
It is filed as its own page with the reproducer, the controls and the probe
invocation above.
