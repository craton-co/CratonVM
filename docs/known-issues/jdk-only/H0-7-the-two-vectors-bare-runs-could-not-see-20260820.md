# H0-7 — the two vectors bare runs could not see, and how far the `HashMap` defect reaches

*(Title corrected after §7. It first read "…and a time-zone registry nobody had in the blast radius", which §7 disproves. §3's wrong sentence is left standing per convention, but a TITLE is what the index and every citation carry, so a wrong one propagates rather than sits still.)*

**Status: OPEN — MEASURED.** Five runs of `regression-suite/run.sh` with
`ONLY="RJdkModule RJdkLogging"` on `C:/craton/target-jdkonly-h2/release/cratonvm.exe`
at `fe59bf9d9`. No source change.

Lane H0 (orchestrator), 2026-08-20. Answers `H0-5` §7 N4.

---

## 1. Why these two needed a different method

`H0-5` §2 established by bare unarmed/armed pairs that 20 of the 22
`HashMap`-armed failures are cleanly dial-caused, and that **two are not
diagnosable that way**: `RJdkModule` fails bare whether or not the dial is armed
(it needs `--module-path`/`--add-modules`, which `run.sh` supplies per vector),
and `RJdkLogging` passes bare in both configurations while failing under the arm.

The hook is `ONLY=`, which schedules a subset through the same per-vector
launcher the full arm uses:

```bash
CV=… TIMEOUT=420 CRATONVM_ARGS=--jdk-only ONLY="RJdkModule RJdkLogging" \
  bash regression-suite/run.sh
```

**Unarmed control first, every time** — `2 passed, 0 failed`. Both vectors are
healthy under the real launch args, so everything below is attributable to the
dial.

## 2. MEASURED — three configurations

| configuration | `RJdkModule` | `RJdkLogging` |
|---|---|---|
| unarmed control | **PASS** | **PASS** |
| `CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/HashMap` | FAIL — bare `AssertionError` | FAIL — runs to completion, 79 checks, **3 diverge** |
| `…=java/util/concurrent/ConcurrentHashMap` | FAIL — bare `AssertionError` | FAIL — bare `AssertionError`, **no output survives** |

Both `AssertionError`s carry an **empty message**, i.e. a bare `assert cond;` in
the vector rather than a VM-level exception. The runner does not print the
assertion's stack, so I have the failing *family* and not the failing *line*.
Stated rather than guessed at.

## 3. The finding — `java.time.zone` is in the blast radius, and nobody put it there

`RJdkLogging` under `java/util/HashMap` survives far enough to diff, and the
divergence is precise:

```text
                                    HotSpot          CratonVM
  CK RJdkLogging streamBytes=        177              179
  CK RJdkLogging handlerLevelGate=ok bytes= 88         89
  CK RJdkLogging defaultZoneRawOffsetMs=-10800000
                 zoneAgrees=         true             threw:ZoneRulesException
```

**`ZoneRulesException` when `HashMap` natives are refused.** The zone-rules
registry — `java.time.zone.ZoneRulesProvider`'s id-to-rules map — is a map whose
contents the VM owns, and with real bytecode reading a real, empty table the
lookup finds nothing and throws.

That is a family that appears in **no** blast-radius record, **no** P0 row, and
none of `H0-3`'s eleven or `H0-4`'s six. Every family found so far has been a
collection, a security/provider chain, a logger registry, a module graph, a proxy
cache or service loading. **`java.time` is new**, and it is reached through
`java.util.logging` — a path nobody would have predicted from the family name.

Note what *agrees*: `defaultZoneRawOffsetMs=-10800000` is identical on both VMs.
So the offset is right and only the **rules lookup** fails. Whether the two byte
counts (`177/179`, `88/89`) are downstream of the same failure or independent is
**NOT MEASURED** — differences of 2 and 1 bytes in a formatted log record are
consistent with either. I am not asserting a single cause for three lines when I
measured one.

## 4. CHM is worse than `HashMap` here, which the table does not say

`H0-4` priced the families in aggregate: `ConcurrentHashMap` 93/104,
`HashMap` **81**/104 — so `HashMap` is the more expensive family overall.

**For these two vectors the order reverses.** Under `HashMap`, `RJdkLogging`
runs to completion and gets 76 of 79 checks right; under `ConcurrentHashMap` it
dies before printing anything.

**The blast-radius table orders FAMILIES by aggregate cost. It does not order
what any individual vector suffers.** A reader planning a per-vector repair from
that table would infer the opposite of what these runs show. Worth stating
because the table is the most quoted artefact this wave produced, and this is
the first measurement that constrains how it may be read.

## 5. The harness errors are crash artifacts — checked, because the alternative was more interesting

The `HashMap`-armed run of `RJdkModule` printed:

```text
HARNESS ERROR [G2] RJdkModule: nothing survives extract() — the cross-VM diff
                   compares two empty strings
HARNESS ERROR [G3] RJdkModule: publishes no check count, and is not in
                   regression-suite/harness-uncounted.txt.
```

Read literally that says **a vector in the strict 104 asserts nothing and passes
by comparing two empty strings** — a green-forever vector, which would be the
most consequential thing in this record.

**It is not that.** `harness_guard_extract`'s own comment says it "runs on every
invocation", and the unarmed run of the same vector produces **zero** `HARNESS`
lines against the armed run's two. The guard is not failure-gated; it is silent
unarmed because the vector *does* publish observables unarmed. Armed, the VM
dies before printing them, so `extract()` yields nothing and G2/G3 fire on the
wreckage.

`RJdkModule` is genuinely not in `harness-uncounted.txt`, and does not need to
be.

I am recording the check rather than only the conclusion because the tempting
move was to publish the literal reading. This directory's most-repeated failure
is asserting a cause from an artefact of the run that produced it — `H0-5` §2
nearly did it with `RJdkModule`'s bare failure, and that was the same vector.

## 6. NOMINATIONS

* **N1 — get the assertion line.** Both `AssertionError`s are bare. The runner
  swallows the stack; a direct invocation with the launcher's own args would
  print it, and that is one command away from naming the failing check in each
  vector.
* **N2 — arm `java/time/` and `java/time/zone/` directly.** If the zone-rules
  registry is VM-owned, that prefix should have a blast radius of its own and it
  has never been measured. Cheap: one env var, one arm, no build.
* **N3 — separate the three `RJdkLogging` diffs.** Establish whether the two
  byte counts are downstream of `ZoneRulesException` or a second defect. If the
  latter, `java.util.logging`'s own stream handling is a third family here.
* **N4 — the blast-radius table needs the §4 caveat attached to it.** It is
  being quoted as a per-vector priority and it is not one. *(`H0-4` is my
  record; I am nominating the edit rather than making it silently, because the
  table has been cited by three other lanes and a quiet change would strand
  them.)*

---

## 7. CORRECTION to §3, same day — `java.time` is a VICTIM, not a new family

I wrote in §3 that *"`java.time` is new"* in the blast radius and nominated
arming it as N2. **I then ran N2 rather than publishing the nomination, and it
falsifies the framing:**

| armed prefix | `RJdkLogging` |
|---|---|
| `java/time/zone/` | **PASS** |
| `java/time/` | **PASS** |

**`java.time` has no native-shadow problem of its own.** Arming its whole
package changes nothing. The `ZoneRulesException` appears only when
`java/util/HashMap` is armed, which means the zone-rules registry is not a
VM-owned *service* — it is ordinary JDK code standing on a **VM-owned map**.

So the correct statement is narrower and more useful than the one I first wrote:

> **`java.time.zone` is a CONSUMER of the `HashMap` defect.** It is not a family
> to migrate. It is evidence of how far the `HashMap` defect reaches — into a
> package that has no natives in the picture at all.

That is still worth having, and it strengthens rather than weakens `H0-4` §3's
conclusion that `HashMap` is the floor: the reach extends past the collection
and security clusters into date/time, through a consumer that no census would
have associated with `java.util`.

**N2 is therefore ANSWERED and withdrawn.** I am leaving §3's wrong sentence
standing above rather than editing it away, per this directory's convention —
the error was calling a consumer a family, on one run, before running the one
command that distinguishes them.
