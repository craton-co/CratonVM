# `class-overrides.tsv` (netty, hib, hibernate-reactive) — deleted by a "cleanup" commit, restored

**Status: FIXED 2026-09-06** for the three `class-overrides.tsv` files.
**A wider exposure is flagged below and NOT fixed here.**

## What happened

`e33f6d7e3` ("cleanup", 2026-08-29) deleted 368 files in one commit — the bulk
of it genuine scratch/experiment cruft (one-off `RESULTS-*.md` snapshots, old
probes, ad-hoc `.bat`/`.ps1` build scripts). Swept up in the same commit were
several files that are supposed to survive exactly this kind of cleanup,
because they were deliberately `git add -f`'d past `apps/`'s wholesale
`.gitignore` for that reason — each one's own header says so:

```
apps/hib-suite-runner/class-overrides.tsv                   -124
apps/hibernate-reactive-suite-runner/class-overrides.tsv     -50
apps/netty-suite-runner/class-overrides.tsv                  -86
apps/netty-suite-runner/module-scoped-classes.tsv            -60
apps/netty-suite-runner/run-netty-suite.sh                   -536
apps/netty-suite-runner/gen-module-args.sh                   -95
apps/netty-suite-runner/gen-openssl-args.sh                  -110
apps/netty-suite-runner/CratonRunner.java                    -76
apps/netty-suite-runner/known-benign-aborts.tsv              -69
apps/hib-suite-runner/run-hib.sh                             -712
apps/hib-suite-runner/known-benign-aborts.tsv                -123
... (README.md, MethodProgressRunner.java, and more)
```

No commit since has re-added any of them; `origin/dev` tip is still missing
all three `class-overrides.tsv` files as of this writing.

## Why this mattered today

The Azure host's live `/data/cratonvm/apps/netty-suite-runner/` had also lost
its `class-overrides.tsv` (not just git's copy). A fresh full 733-class
3-GC-arm suite run on 2026-09-06 printed the warning
(`per-class override table not found`) and silently ran the five
`SSLEngineTest`-family classes — which need documented 3 600–14 400s floors
(`ssl-parameterized-classes-exceed-180s-timeout-masking-real-failures-20260826.md`)
— at the flat 180s cap. All five reported `HANG`, exactly the failure mode the
table exists to prevent, and looked at first glance like a fresh regression.

## Fix (this page)

Restored all three `class-overrides.tsv` files verbatim from
`e33f6d7e3^` (the commit immediately before the deletion), to both the git
tree (force-added, `dev` commit `2b153ee13`) and the live Azure host
(`/data/cratonvm/apps/{netty-suite-runner,hib-suite-runner,hibernate-reactive-suite-runner}/`).
No content changes. `*.tsv` is already pinned to `eol=lf` in `.gitattributes`
via a blanket rule, so no further hardening was needed there.

Verified: `run-netty-suite.sh overrides` now reports `state: 6 (loaded)`
(previously `MISSING <path>`).

## What is NOT fixed here — a wider exposure

The other files the same commit deleted from git
(`module-scoped-classes.tsv`, `run-netty-suite.sh`, `gen-module-args.sh`,
`gen-openssl-args.sh`, `CratonRunner.java`, `known-benign-aborts.tsv`, the
hib-suite-runner equivalents, `README.md`s) are **currently fine in practice**
— they still physically exist on the shared Azure host at
`/data/cratonvm/apps/*-suite-runner/`, untouched by the git-level deletion
because that host's working directory was never re-checked-out against the
commit that removed them. Several have since accumulated real, live fixes
this session (the netty pre-flight build-verification check, the
`bootstrap.extensions`/BC-jar-order/OpenSSL `common.args` edits) that exist
**only** on that one host's disk.

That is a durability gap, not an active bug: **if that host's `/data` were
lost, all of this — the override tables just restored, the harness fixes from
earlier in this session, and everything `e33f6d7e3` removed that is still
silently relied upon — would be gone with no git history to recover from.**
Restoring it properly means reconciling each file's git-history version
against its current, further-modified state on the host (not a blind
git-checkout, since `run-netty-suite.sh` in particular has real fixes layered
on top of the pre-deletion version) — out of scope for this triage pass.
Flagged as follow-up work.

## Related

* `ssl-parameterized-classes-exceed-180s-timeout-masking-real-failures-20260826.md`
* `nativeimagehandlermetadatatest-harness-module-scope-FIXED-20260819.md`
