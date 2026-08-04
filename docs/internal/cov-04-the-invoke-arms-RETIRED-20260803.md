# COV-04 — RETIRED 2026-08-03. Every invoke refusal was a constructor

Was `docs/known-issues/c2/cov-04-the-invoke-arms.md`. Owned the `0xb7` / `0xb9`
arms and the `<init>`-elision path of `IrBuilder::build`, and the invoke
eligibility loop in `jit/src/lib.rs` that feeds them.

The brief opened by refusing to size itself — *"this doc deliberately does not
guess the split"* — and made the first increment a grouping rather than code.
That was the right call, and the grouping it asked for contradicted both of the
two cases it offered.

## What the lane shipped

| | |
|---|---|
| increment 0 | the grouping the brief demanded, plus the diagnostic that makes it a `grep` |
| increment 1 | a compiled constructor's `super(...)` / `this(...)` chain call lowers to a real `Op::Call` |
| increment 2 | a method containing a `new` may lower its calls at all — the `call_eligible` term that forbade it had outlived its reason |
| tests | three in `jit/tests/ir_vs_singlepass.rs`: one per shape, plus the transform that must not be lost |

## Increment 0 — the grouping

`CRATONVM_DBG=ir-compiles` over the three Spring Boot workloads that carry 1,947
of the survey's 1,954 compile requests (`ConditionalOnPropertyTests`,
`AutoConfigurationSorterTests`, `ConditionalOnClassTests`, module
`core/spring-boot-autoconfigure`, default configuration, one run each). It
reproduces the survey's bail histogram to within one event, so it is measuring
the same thing:

| site (survey line no.) | survey | this run |
|---|---:|---:|
| `ir.rs:5204` `invokespecial`, neither resolvable call nor trivial `<init>` | 53 | 52 |
| `ir.rs:5264` `0xb6`/`0xb8` with no `invoke_info` | 13 | 13 |
| `ir.rs:5314` `0xb9` with no `invoke_info` | 2 | 2 |
| `ir.rs:5219` elidable `<init>` whose receiver is not a fresh `Op::New` | 1 | 1 |

### Every one of the 52 is an `<init>`

The brief asked which of two things the 53 were:

> * a superclass or private `invokespecial` that *should* lower to an ordinary
>   call and the arm simply does not have that path; or
> * a constructor the elision analysis correctly declines, where the right
>   answer is a real call to `<init>` and the builder has no way to emit one.

**The first case has zero events.** Grouped by callee, all 52 name an `<init>`;
not one is a private or `super.m()` `invokespecial`. That case was already
handled — inc 24 gave the arm an ordinary-call path for a non-`<init>`
`invokespecial`, and on this corpus it never fails.

The second case is closer, but the split inside it is the finding. Joining each
bail to the reason its method lost `invoke_info` — keyed on the compiling
method's own name, which the diagnostic now prints on the bail line, **not** on
log adjacency — gives:

| group | why `invoke_info` was absent | events | sites |
|---|---|---:|---|
| **A** — a `super(...)` / `this(...)` chain call in a compiled **constructor** | `is_special && mn == "<init>"` discarded `invoke_info` for the whole method | **29** | all at `5204` |
| **B** — the method contains a `new` | `call_eligible = scan.new_ops.is_empty()` discarded `invoke_info` for the whole method | **39** | 23 @ `5204`, 13 @ `5264`, 2 @ `5314`, 1 @ `5219` |

68 events, none unmatched. (The same split falls out of the cruder
nearest-preceding-log-line join, to the event — but that agreement is the check,
not the method.)

Group A is a case the brief did not have. Its receiver is `this`, a parameter;
there is no `new` anywhere in the method, so eliding was never one of two
options — it was structurally impossible, and the term was refusing the only
lowering there was. 35 compiles were disabled that way; 79 were disabled by
group B's term.

Worth noting because it is the sort of thing a coarser join gets wrong: 33 of
the 52 have a constructor as the *caller*, but only 29 are group A. The other
four are constructors that also contain a `new`, so their cause is group B's
term, not group A's.

### `ir.rs:5264` is not what it looked like

The brief read the 13 as *"the caller supplied no `invoke_info` for that pc, so
the site was never resolved at compile time"*, and asked whether a deferred path
was worth building. It is not, because that is not what happened. The callees at
those 13 sites are `StringBuilder.append`, `Class.getName`,
`CollectionUtils.newLinkedHashSet`, `RootBeanDefinition.hasMethodOverrides` —
ordinary, resolvable, shape-compatible methods. Every one is in a method that
lost `invoke_info` wholesale to group B's term, and the builder reached the
virtual/static invoke before it reached the constructor.

Across every invoke-bearing admitted compile in the corpus there are exactly
**two** reasons `invoke_info` is ever absent — group A's term and group B's —
and **zero** events for any of the other four (`cp_invoke_resolver` declining a
CP index, `static_call_shape` refusing a descriptor, a gate being off, a
self-recursive wide return). So the two backends do *not* "already agree here":
the IR tier was losing sites the single-pass backend compiles without comment.

## Increment 1 — group A: call the constructor

`is_special && mn == "<init>"` is gone from the eligibility loop. Its comment
justified itself with *"a `<init>`-bearing method also has a `new`, so
`call_eligible` is already false"* — false for 35 of the compiles it disabled,
which were constructors with no `new` at all.

A `<init>` now takes `invoke_kind == 1`, the statically-bound non-virtual
dispatch, which is **exactly what the single-pass backend already emits** for
every constructor call its own elision rewrite declines (`jit/src/lib.rs`, the
`cp_elidable_init_resolver` rewrite's `else` arm). No new call shape and no new
runtime path: `jit_invoke_dispatch` kind 1 → `invoke_special_shared`, the
interpreter's own invokespecial semantics.

A `<init>` is deliberately excluded from the IR **direct**-call path. That path
bakes a callee entry this compile resolved by running `callee_compiler`, and
making every constructor site eagerly compile its callee is a compile-time and
recursion-cycle change this lane did not measure. Constructors keep helper
dispatch.

The `0xb7` arm is now ordered *elision first, call second*, and the
elidable-but-receiver-is-not-a-fresh-`New` case falls through to the call
instead of bailing (that was the single `5219` event). The ordering matters: an
`Op::Call` arg-escapes its reference inputs, so taking the call where elision
applies would pin an allocation that scalar replacement was about to remove.

The brief's hazard — *"an `<init>` that this lane starts calling rather than
eliding must still run every side effect the elision path was allowed to skip"* —
points the safe way round. Calling runs everything; only eliding can skip.

## Increment 2 — group B: a method with a `new` may have calls

```rust
let call_eligible = scan.new_ops.is_empty() && scan.anewarray_ops.is_empty();
```

The `new_ops` term's stated reason was *"a surviving `New` would need the
allocation path the lowerer lacks"*. `ir_lower` has that path: its `Op::New` arm
goes through the shared compact-layout / TLAB-aware `jit_new_object` stub — the
same helper the single-pass `0xbb` uses — and `ir_lower` already refuses the
graph when `helpers.new_object == 0` rather than calling through address zero.
The premise expired; the term did not.

It is the single largest cause of an invoke refusal in the corpus: 79 compiles,
39 of the 68 bails at that baseline, including *all* of `5264` and `5314`.

`anewarray` stays. `IrBuilder::build` has no `0xbd` arm at all and
`ir_compatible` refuses such methods a stage earlier anyway — that conjunct is
`cov-06`'s, and this lane did not touch it.

Soundness of the newly-admitted shape rests on one existing rule: an allocation
that reaches an `Op::Call` as an argument is `ArgEscape` in
`build_connection_graph`, so it is really allocated rather than scalar-replaced.
The `new`-plus-non-elidable-`<init>` test asserts exactly that, by counting
allocations.

## The verification

`CRATONVM_DBG=ir-compiles`, the same three workloads, two rounds, both arms
every round with the arm order flipped between them. Every number below is a
count, so the host's load does not enter into it.

**The base arm is `origin/dev` at `95152daea`, which already carries `cov-01`
and `cov-02`.** Both landed while this lane was open and both edit the same
match statement, so measuring against the tree this lane branched from would
have credited it with two neighbours' bodies. It also matters in the other
direction, and this is the part worth carrying: **every one of them made this
lane bigger.** A method blocked on `ldc` never reached its `invokespecial`, so
`cov-01`'s landing roughly doubled the `0xb7` site. Measured three times against
three different baselines, this lane removed 69, then 69, then **106** invoke
refusals — same code, same corpus, different neighbours.

| | base r1 | base r2 | fix r1 | fix r2 |
|---|---:|---:|---:|---:|
| compile requests | 1,938 | 1,944 | 1,933 | 1,935 |
| admitted to the optimizing pipeline | 974 | 977 | 971 | 970 |
| **bodies the optimizing backend produced** | **778** | **781** | **850** | **849** |
| builder refusals, all sites | 159 | 159 | 85 | 85 |
| **invoke refusals** | **106** | **106** | **0** | **0** |
| opcode-gap events | 13 | 13 | 13 | 13 |

**The invoke arms refuse nothing on this corpus any more** — `5555`, `5597` and
`5647` are all zero in both fixed rounds — and **bodies rose by 70**, 778 → 849
(+9%). All twelve runs pass (`failed=0 aborted=0 containersFailed=0`) in both
arms.

The other half, which the brief insists be quoted: of the 106 methods that
stopped failing here, ~70 became a body and the rest moved to the next gap they
meet — which, with `cov-01` and `cov-02` closed, is almost entirely `cov-03`:

| where the rest went | base | fix |
|---|---:|---:|
| `putfield` of a non-`I/Z/B/C/S` tag (`cov-03`; `ir.rs:5337`→`5387`) | 45 | **78** |
| `getfield` of a `long`/`float`/`double` (`cov-03`) | 6 | 4 |
| a `new` whose site is `JitNewSite::Deferred` | 2 | 3 |
| the opcode gap (`0xbc`, `0x53`, `0xb3`, `0x5c`) | 13 | 13 |

Which is the ranking-shift the directory's own re-run rule predicts. `cov-03`'s
`putfield` row grew by 33 and now accounts for **78 of the 85** builder refusals
that remain — because the methods that were hiding behind the invoke terms are
constructors, and constructors write reference fields. Run-to-run variation on
these counts is ±1–3 events.

Bail line numbers move with the edit. In the fixed binary the invoke bails are
`ir.rs:5555` (`0xb7` — the old `5204`/`5219` pair merged, since a receiver that
is not a fresh `Op::New` is now a call rather than a refusal), `ir.rs:5597`
(`0xb6`/`0xb8`) and `ir.rs:5647` (`0xb9`).

The brief asks for both halves to be quoted, and warns that a method which stops
failing here and immediately fails on the next unlowered opcode is a real
outcome and is not a body. That did happen — 36 of the 106 — but with `cov-01`
and `cov-02` closed the opcode gap is down to **13 events on this whole corpus**,
so nearly all of the shortfall lands on `cov-03` rather than dispersing.

### Correctness across a wider corpus

The change admits a whole new population — every method containing a `new` —
to the optimizing tier, so the three-workload A/B is not enough on its own. The
**79-class Spring Boot regression list** (`/data/data/regr-list.tsv`) was run on
both arms, interleaved per class with the arm order flipped between classes.
Run twice, once per baseline, the second time after `cov-01` and `cov-02` had
raised the fixed arm to 850 bodies — because "more C2 code executes" is exactly
the condition under which a miscompile would show:

| verdict | sweep 1 base | sweep 1 fix | sweep 2 base | sweep 2 fix |
|---|---:|---:|---:|---:|
| PASS | 65 | 65 | 64 | 65 |
| FAIL (pre-existing) | 12 | 12 | 13 | 12 |
| NOSUMMARY | 1 | 1 | 1 | 1 |
| no classpath built | 1 | 1 | 1 | 1 |

**Sweep 1: zero mismatches across all 79.** Sweep 2: one, and it is worth
spelling out because it points the flattering way and is still not a result.
`DevToolsEmbeddedDataSourceAutoConfigurationTests` failed on the **base** arm
and passed on the fixed one — i.e. the change appeared to *fix* something. It
did not: re-run three times per arm, it is **6/6 PASS**, and the base failure is
a `NoSuchBeanDefinitionException` from the known Spring bean-attribute flake
family. A mismatch in your favour is a mismatch; re-run it before it becomes a
claim.

Net: **no class changes state in either direction** that survives repetition,
across two independent sweeps at two different baselines — the second with the
fixed arm producing 850 bodies rather than 683.

Unit coverage, `jit/tests/ir_vs_singlepass.rs`:

* `ir_invokespecial_super_constructor_chain_is_called` — group A. The dispatch
  stub writes `n * 3` into the receiver's field; the method reads it back. `0`
  means the call was never emitted; anything else means the receiver/argument
  pair is wrong.
* `ir_new_with_non_elidable_constructor_allocates_and_calls_init` — group B.
  `new Corpus(n)` then `sink(c)`; the constructor writes `n * 5` and `sink`
  reads it back, and the allocation counter must advance once per invocation.
* `ir_elidable_trivial_init_on_fresh_new_is_still_elided` — the transform the
  lane must not lose, as a two-armed comparison over identical bytecode. With
  the site elidable the dispatch helper must never run (it `panic!`s rather than
  returning something plausible, so an un-elided `<init>` cannot slip past);
  with the identical bytecode declined by the elision analysis it must run
  exactly once per invocation. The second arm exists because the first alone
  would also pass on a builder that silently dropped every `<init>`.

Each test names the exact edit that trips it, per this directory's rule 5. On
the merged tree `cargo test --release -p cratonvm-jit --lib` is 1,868 / 0 and
`--test ir_vs_singlepass` is 121 / 0 (this lane's three plus `cov-01`'s and
`cov-02`'s).

### One trap worth naming

`ir_lower::tests::the_op_representatives_cover_every_declared_variant` failed
during this lane with an `ir::Op` set of **90** names — `Absent`, `Acquire`,
`EDGE`, `THREAD` — instead of the real 53. Nothing to do with the change. The
edits were made in a **Windows** worktree, where `core.autocrlf=true` puts CRLF
on disk (the `.gitattributes` header says so), and the file was `scp`'d to the
Linux build host. `include_str!("ir.rs")` then read CRLF, so that scanner's
`"\n}\n"` block terminator never matched and its "enum body" ran to the end of
the file.

Two things follow. Sync a remote build worktree with
`git fetch && git reset --hard FETCH_HEAD`, never `scp` — that also proves the
host is building the pushed content. And when checking whether a blob really
carries CRLF, use `git cat-file -p`: `git show HEAD:<path>` applies the
working-tree EOL filter and reports the opposite. Release binaries are
unaffected — every `include_str!` in the crate is under `#[cfg(test)]` — so the
A/B above stands; only the test run had to be repeated from a clean checkout.
This is a sibling of the six text-scanning gates `seam-01` found, from a
direction that has nothing to do with moving code.

## Residuals

* **`<init>` sites keep helper dispatch**, by choice (increment 1). Binding them
  directly is a measurable follow-up, not a gap.
* **A constructor with a `long` / `double` / `float` parameter** is still
  refused by `static_call_shape`, and one refused site would still discard
  `invoke_info` for the whole method. **Measured at zero** across 669
  invoke-bearing compiles on the merged tree — as are all four other
  non-emittable reasons, and `call_eligible` itself. There is nothing left to
  size this from: the census now prints no `NO invoke_info` line at all.
* **The all-or-nothing discard itself.** One non-emittable invoke would cost the
  method every other invoke's lowering. It is not *wrong* (the builder bails on
  any invoke it cannot lower, so the method was lost either way), and it now
  never fires, but it is why the two terms this lane removed cost 106 events
  rather than being confined to the sites that caused them.

### The one residual this lane found, and the two wrong answers on the way

**Escape analysis offers a scalar replacement that the emitted body does not
take.** For `Corpus c = new Corpus(); c.f0 = n; return c.f0` with an elidable
`<init>`, `CRATONVM_DBG_SCALAR_NEW=1` reports **`scalar-replaced 1/1`** — and
the emitted code still calls `jit_new_object` once per invocation. The control
arm, with the constructor declined and therefore really called, reports `0/1`
and also allocates, which is correct. So the discrepancy is not in
`analyze_escapes`' *analysis*; it is between what it offers and what survives to
the emitted body. Not this lane's to fix — recorded here with an exact repro
(`ir_elidable_trivial_init_on_fresh_new_is_still_elided`, arm 1, which asserts
the current count and says in the assertion message that 0 would be an
improvement).

Getting there took two wrong answers, both worth naming:

1. The counter that produced the first report was **a single `static` shared by
   two `#[test]` functions, which `cargo test` runs on concurrent threads** —
   the other test's allocations were being counted against this one. Flaky,
   which rule 5 rates worse than no assertion.
2. Having found that, the obvious conclusion — *"so the residual was an
   artefact"* — was **also wrong**, and `scalar-replaced 1/1` looked like proof
   of it. It is not: that line reports what the analysis *offered*. Replacing
   the stub with one that `panic!`s if it is ever reached settled it in one run;
   the allocation is real.

Both tests now own their counter and their `extern "C"` wrapper, declared inside
the test body. And the general lesson is the one already in
`reference_compare_what_the_consumer_reads_not_the_value`: a diagnostic that
reports a decision is not evidence about the code that was emitted. Assert
against the observable, not against the log line.

## Reproducing

```bash
CRATONVM_DBG=ir-compiles <cratonvm> ... SbRunner org.springframework.boot.autoconfigure.condition.ConditionalOnPropertyTests
```

Three line kinds carry everything above:

* `[ir] invoke-plan <method>: sites=N new_ops=N anewarray_ops=N gates(...) call_eligible=B`
  — one per invoke-bearing compile, before the builder runs.
* `[ir] invoke-plan <method>: NO invoke_info — <reason>` — which condition
  discarded the map, and at which pc. Five remain after this lane:
  `call_eligible=false`, a gate being off, `cp_invoke_resolver` declining the CP
  index, `static_call_shape` refusing the descriptor, and a self-recursive wide
  return. All five measured zero except `call_eligible`, which measures zero too
  now that only `anewarray` is left in it.
* `[ir] IrBuilder::build refused at ir.rs:NNNN (bytecode pc N) in <caller> callee <0xNN cn.mn desc>`
  — the bail, now naming both ends. The caller is what makes the join to the
  reason line exact rather than an assumption about log interleaving.

All three are diagnostic-only and cost nothing with the flag off: the invoke
label map is not built, the constant-pool resolver is not called for it, and
`IrBuilder`'s two extra fields stay empty.
