# P4-B: `--synthetic-jdk` MODE requires its own binary

> **Correction, same day.** This document was first titled "why it has never
> been run", following the roadmap's "it has never been executed, ever". A lane
> auditing the corpora falsified that: `apps/h2database-suite-runner/RESULTS-20260721.md:91-95`
> records a synthetic-mode boot failing on a *missing synthetic stub*, and four
> runners pass the flag today. The defensible restatement is **the
> `--features synthetic-jdk` binary has never been run in that mode by this
> campaign, and no `RJdk*` vector ever has** — nothing gating
> (`regression-suite/`, CI, `scripts/`) launches the mode; CI runs
> `cargo check`/`test` and never the binary. The measurement below stands
> unchanged and is the more useful fact: the mode cannot be reached from a
> shipping binary at all.
>
> Also corrected by that lane: `--synthetic-jdk` **is** a runtime flag
> (`vm-cli/src/main.rs:312`); there are three runtime selections, `--jdk-only`
> implies `JdkMode::Real`, the launcher default is `Real` and the **embedded**
> default is `Synthetic`. The original "feature ≠ mode" note was right about the
> distinction and wrong to imply the mode had no flag.



**Measured 2026-08-12 on `cratonvm-merged-dev` (dev tip `210703b7a`).**

The roadmap lists P4-B as "run `--synthetic-jdk` MODE once — it has never been
executed, ever", with several records' residuals living only in that
configuration. That framing implies it is an unperformed chore. It is not.

## The answer: a shipping binary refuses the mode outright

```
$ cratonvm --synthetic-jdk -cp . FabReach
[cratonvm] main-vm run() returned Err: synthetic-JDK mode was selected but this
binary was built without the `synthetic-jdk` Cargo feature, so none of the
~5,200 synthetic stubs are compiled in.

Running in this state would give a VM with neither the synthetic class library
nor a real-JDK boot classpath (synthetic mode also suppresses boot-classpath
discovery), so it is rejected here rather than failing later as an unexplained
NoClassDefFoundError.

Fix by one of:
  * rebuild with `cargo build -p cratonvm-cli --features synthetic-jdk`; or
  * drop --synthetic-jdk and run the default real-JDK mode.
```

Exit code **1**. The refusal is explicit, correct, and arrives during argument
parsing before anything else can go wrong.

## What this corrects

The standing note in this project is **"feature ≠ mode"** — `--features
synthetic-jdk` is a *build* feature and `--synthetic-jdk` is a *runtime* mode,
and a registrar reachable only from `register_synthetic_overrides` ships in
neither binary. That is true and remains the important warning.

But it is incomplete in a way that matters for planning: **the mode *requires*
the feature.** They are not independent axes. The feature is necessary though
not sufficient — you still need the flag. So there is no such thing as
exercising synthetic-JDK mode "once" as a quick run; it needs its own build of
its own binary, and that is the real reason it has never happened, rather than
oversight.

Consequences worth stating plainly:

* **Any record whose residual lives only in `--synthetic-jdk` mode cannot be
  adjudicated by any run of a shipping binary** — not by the regression suite at
  any `SUITE=` value, not by a corpus run, not by a census. It needs the feature
  build, and a lane that reports such a residual as "unreproducible" has
  measured the wrong binary.
* Symmetrically, a defect that only exists in that configuration **cannot affect
  any shipped run**, so it is a documentation and dead-code concern, not a
  correctness one. That is a legitimate reason to rank these residuals low —
  but it should be said for that reason, not by leaving them silently untested.
* The `~5,200 synthetic stubs` figure in the refusal text is the VM's own count
  and is worth reconciling against the census numbers, which are drawn from a
  binary in which none of them are compiled in.

## Status

A `--features synthetic-jdk` release binary is being built from clean `HEAD`
(`2dbb9d451`) in a separate target directory, so the mode can be exercised for
the first time and the residuals that live only there can be adjudicated. Result
to be appended here.

Note the build must come from a clean tree: exporting `git archive HEAD` rather
than building the working copy, because a multi-agent campaign leaves the
working tree half-edited and a binary built from it would measure nothing
reproducible.
