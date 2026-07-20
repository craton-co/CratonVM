# ES CRASH — `GenerationalHeap::get_field` stack overflow, CONFIRMED pre-existing on `origin/dev` (unrelated to JIT/Lucene)

Status: OPEN — confirmed on clean, unmodified `origin/dev`

## Symptom

`EXCEPTION_STACK_OVERFLOW` (`0xC00000FD`) very early in process startup
(within ~1s of the last clinit-fixup log line, before any real test method
runs). The crashing PC symbolizes to
`cratonvm_gc::gen_heap::GenerationalHeap::get_field` (`gc/src/gen_heap.rs`,
seen at both line 2013 and line 2026 across different runs/builds — a
hyper-hot generic field-access function) and once to a sibling function,
`GenerationalHeap::read_slot` (`gc/src/gen_heap.rs:10149`). The raw native
frame walker gave up entirely (`Native frames (most recent call first)
[raw]:` followed by nothing), consistent with a stack too
deeply/repeatedly corrupted to walk — the shape strongly suggests
**recursion**, not a legitimately deep call chain (a real deep Lucene call
chain would not crash ~1s into startup, before any indexing/searching has
happened).

## THIS IS NOT specific to JIT-compiling Lucene — confirmed, see below

**Reproduces on a clean, completely unmodified `origin/dev` checkout**
(commit `54003fb83`, no local changes of any kind, built fresh in the main
worktree `C:\craton\CratonVM`) with the *default* production config — no
`CRATONVM_JIT_ALLOW_PACKAGES` env var, `LUCENE-POSTINGS.1`'s blanket
Lucene-JIT ban fully in place as normal. This is a genuine, currently
undocumented, pre-existing regression already on `dev`, discovered only by
coincidence while investigating something unrelated — **not** caused by,
or specific to, lifting the Lucene JIT ban, despite how it was first
noticed (see Discovery below for why the initial attribution was wrong).

## Discovery context (and a correction)

Found 2026-07-19 while investigating whether `LUCENE-POSTINGS.1`'s original
justification (a different bug — AIOOBE-then-SIGSEGV postings corruption,
2026-07-05) still held on current `dev`
(`vm/src/jit/skip_list.rs`'s `LUCENE-POSTINGS.1` comment has the full
history). The original repro passed cleanly across several verification
runs with the ban lifted, against a `dev` snapshot from earlier in the day.
The ban was removed and merged with `origin/dev` (60 same-day commits
pulled in) — the very next verification run, immediately post-merge, hit
this stack overflow instead, reproducibly (2/2 runs), which was initially
(incorrectly) attributed to lifting the ban. The ban was reverted back to
banned as a precaution — but the crash **reproduced again anyway**, in the
now-reverted (default, ban-in-place) config, on the same post-merge build.
Isolated definitively by building a byte-for-byte clean `origin/dev` tip in
the main worktree with zero local changes: **still crashes**, same
signature. So the ban-lift was a red herring — this bug was already
sitting on `dev`, unrelated to the whole Lucene-JIT investigation, and just
happened to get noticed during it.

## Not yet investigated

- Which of the 60 same-day `origin/dev` commits (merged in mid-investigation)
  introduced this — a `git bisect` restricted to that range, or simply
  checking whether it reproduces on the same test class/method against an
  earlier `dev` tip from before that merge, would narrow it quickly.
  Candidates worth checking first: anything touching
  `vm/src/runtime/interpreter.rs`, `vm/src/jit/helpers.rs`, or
  `gc/src/gen_heap.rs` that day.
- Whether `get_field`/`read_slot` genuinely recurse into themselves (direct
  or mutual recursion — e.g. a getfield triggering a GC-safepoint check,
  triggering a root scan, triggering another field read, in a cycle) or
  whether the crash is misattributed (the symbolized PC is the *crashing*
  frame, not necessarily the frame that started the runaway recursion — the
  raw frame walker failing means the actual recursive chain was never
  captured).
- How broadly this reproduces — only checked against
  `ES812PostingsFormatTests#testDocsAndFreqsAndPositionsAndPayloads`
  (seed `B17AC9D3E1F2A0C4`) so far. Given it crashes ~1s into a fresh
  process, before the target test method itself even starts, this smells
  like a **classloading/bootstrap-path** bug that could affect any
  sufficiently-shaped class hierarchy, not something specific to this one
  test — worth checking against other ES/Lucene classes, and non-ES
  workloads, to gauge real blast radius.

## Repro

```powershell
$JDK = "C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot"
$ES  = "C:\craton\CratonVM\apps\elasticsearch"
$CP  = (Get-Content "$ES\server\build\craton-testcp.txt" | ForEach-Object { $_.Trim() } | Where-Object { $_ }) -join ';'
& $EXE --java-home $JDK --stack-dump-on-timeout 350 --Xmx 2g `
  -Dtests.seed=B17AC9D3E1F2A0C4 -Des.path.home=$ES `
  -Dtests.testfeatures.enabled=true -Dtests.security.manager=false -Dtests.asserts=false `
  -Dtests.timeoutSuite=580000! -Dtests.method=testDocsAndFreqsAndPositionsAndPayloads `
  <standard ES --add-opens set, see run-elasticsearch-suite.ps1 Get-EsJavaArgs> `
  -cp $CP org.junit.runner.JUnitCore `
  org.elasticsearch.index.codec.postings.ES812PostingsFormatTests
```

No special env vars needed — reproduces with the plain default config.
Crashes within ~1s, 3/3 attempts so far (ban-lifted, ban-restored, and
clean unmodified `origin/dev`). To symbolize a fresh crash's RVA against
the same binary: `CRATONVM_SYMBOLIZE=<exe+0x RVA from the crash report>
cratonvm X`.

## Impact

**Higher urgency than initially scoped** — this is not gated behind an
opt-in JIT flag; it can affect any workload that happens to hit whatever
bootstrap/classloading condition triggers it, in the *default* production
configuration. Given it fires ~1s into process startup for this specific
test, it may already be silently contributing to unexplained crashes
elsewhere in the ES suite (or beyond) that haven't been traced back to this
root cause yet. Recommend treating this as a priority investigation, not a
low-urgency residual.
