# `DefaultCatalogAndSchemaTest` HANGs again — the per-class 3600s/`--nojit` runner accommodation from 2026-07-22 does not exist in the current `run-hib.sh`

| | |
|---|---|
| **Status** | 🟡 OPEN — harness/tooling gap, **not** a VM defect. Corrects [`docs/internal/fixed-suite-bugs/hibernate/qualfiedtablenaming-hang-cluster-20260721-FIXED.md`](../../internal/fixed-suite-bugs/hibernate/qualfiedtablenaming-hang-cluster-20260721-FIXED.md), whose "Resolved 2026-07-22" section is false as of this writing. |
| **Class** | `org.hibernate.orm.test.boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest` |
| **Discovered (recurrence)** | 2026-07-31, auditing a fresh full 4548-class suite run for still-hanging classes. |

## What the retired doc claims

[`qualfiedtablenaming-hang-cluster-20260721-FIXED.md`](../../internal/fixed-suite-bugs/hibernate/qualfiedtablenaming-hang-cluster-20260721-FIXED.md)
correctly diagnoses this class's HANG as a known, accepted tradeoff: the real
`MutableBigInteger` JIT-corruption AIOOBE (11-session saga in
[`hib-misc-residuals-20260716-FIXED.md`](../../internal/fixed-suite-bugs/hibernate/hib-misc-residuals-20260716-FIXED.md))
is quarantined by forcing `java/math/MutableBigInteger` to stay interpreted
(`41cdfdf94`), which is correct but makes this 132-`@ParameterizedClass`-method
class slow enough (620s–1083s+ historically) to blow through the suite
runner's flat 300-second per-class timeout every time, with or without any
other defect.

The doc's own "Resolved 2026-07-22" section then claims this was fixed at the
**harness level**: `run-hib.sh` was changed to give this one class a
3600-second timeout floor and to force `--nojit` for it (after the extended-JIT
run exposed a separate, narrower `UnknownEntityTypeException` metadata-corruption
residual), so that a real-JDK run completed cleanly:
`found=132 started=123 ok=123 failed=0` in 1,804,813 ms.

## Fresh evidence: it HANGs again, unchanged from the original diagnosis

Source: `apps/hib-suite-runner/runs/categorize-20260730-225515` — a fresh full
4548-class run, 8 shards, binary from worktree `CratonVM-hib-local-0712-v3`
(tip `8e8a7b8cd`), `dev` merged with the real ByteBuddy fix.

```
$ grep DefaultCatalogAndSchemaTest apps/hib-suite-runner/runs/categorize-20260730-225515/results.tsv
70	org.hibernate.orm.test.boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest	HANG	0	0	0	0	0	0	process-died rc=124
```

`rc=124` is the flat `timeout "$TIMEOUT"` wrapper's own kill code, at
`ms=0`/zero `@@RESULT` ever printed — exactly the original "never gets a
chance to finish" signature, not a new symptom.

**Solo repro this session** (`CratonRunner` invoked directly, same binary,
same `--java-home`/`--Xmx 1500m`/`common.args` as the suite uses): the process
ran genuinely, continuously CPU-bound — CPU-time samples tracked essentially
1:1 with wall-clock time across a 6+ minute sampled window (e.g. 253s → 370s
CPU over a ~4:16 → 6:13 elapsed window) — with zero stdout progress past the
JAXB/DDL-bootstrap stage the entire time. This is the same "genuinely
computing, not parked" signature the original doc already established for
this class; it is not stalled, it is just slower than the harness's default
timeout allows.

## Root cause: the 2026-07-22 runner accommodation is not in the current script

`apps/hib-suite-runner/run-hib.sh` — the only test-suite runner script in this
repo — applies exactly **one flat timeout to every class**, with no per-class
exceptions of any kind:

```bash
TIMEOUT="${TIMEOUT:-300}"          # per-class wall cap (s) -> HANG
...
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 timeout "$TIMEOUT" "$CV_BIN" "${VMFLAGS[@]}" \
    -Dcraton.batch=1 "$RUNNER_CLASS" "$cls" >"$tmp" 2>>"$RAW"; rc=$?
```

There is no per-class timeout table, no `--nojit` override for any specific
class, and no reference to `DefaultCatalogAndSchemaTest` (or
`qualfiedTableNaming` at all) anywhere in the file — confirmed by reading the
full script and by `grep -n "DefaultCatalogAndSchemaTest\|3600\|nojit"
run-hib.sh`, which matches nothing relevant. Whatever accommodation the
2026-07-22 session implemented and validated, it is not present in the copy
of `run-hib.sh` used to produce the 2026-07-31 categorize run, nor in the
copy in the main worktree today.

**Why it could vanish without anyone noticing:** `apps/` is wholly gitignored
(`.gitignore:12: apps/`), so `run-hib.sh` carries **no commit history** in
this repository — there is no diff, no blame, no way to see when or how the
2026-07-22 change was lost. `docs/known-issues/hibernate/README.md`'s own
"Data-loss note" records a precedent for exactly this failure mode: on
2026-07-16, `apps/hib-suite-runner`'s driver files (`rerun.sh`, `common.args`,
`CratonRunner.java/.class`, `testlist.txt`, etc.) were found silently
truncated to zero bytes by local-disk-exhaustion corruption, and were
recovered from a `hib-suite-runner.tar` backup dated **2026-06-28/29** — three
to four weeks *before* the 2026-07-22 fix this doc is about. Restoring
`run-hib.sh` from that (or any other pre-07-22) backup would silently revert
this exact accommodation while leaving everything else looking normal, which
is consistent with what's observed: the script works fine for every other
class, it simply never had this one class's special case (or had it and lost
it) with nothing to flag the gap.

## Not a correctness regression

The doc's actual subject — the `MutableBigInteger` AIOOBE and its interpreter
quarantine (`41cdfdf94`) — is untouched and not in question here. This is
purely a test-harness bookkeeping gap: one already-known-slow class has no
timeout accommodation in the current runner, so it predictably reports `HANG`
on every fresh full-suite run that uses the default 300s timeout, exactly as
it did before the 2026-07-22 fix was (apparently) written.

## Recommendation

1. Re-implement a per-class timeout/mode override in `run-hib.sh` — e.g. an
   associative array such as `CLASS_TIMEOUT_OVERRIDE["org.hibernate.orm.test.boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest"]=3600`
   plus a `CLASS_NOJIT_OVERRIDE` set, checked in `run_shard()` before choosing
   the `timeout`/`VMFLAGS` for a given class.
2. Since `apps/` is gitignored wholesale and has already lost driver-file
   state twice (2026-07-16 truncation, now this), consider carving out just
   the override table (or all of `run-hib.sh`) into a tracked location (e.g.
   `docs/internal/` or a small tracked config under `apps/hib-suite-runner/`
   specifically excluded from the blanket ignore) so this class of "silent
   runner regression" stops recurring invisibly.
3. Until either lands, treat a `HANG` on this exact class in any fresh
   full-suite run as expected, not a new regression — cross-reference this
   doc and the original
   [`qualfiedtablenaming-hang-cluster-20260721-FIXED.md`](../../internal/fixed-suite-bugs/hibernate/qualfiedtablenaming-hang-cluster-20260721-FIXED.md)
   rather than re-investigating it as new.
