# L12 — Item 11 residuals (§2, §4, §6, §8, §9, §10, §11)

**Owns:** mixed — see the per-section table. **Claim sections, not the lane.**
**Gated on:** partly. §4 wants L5's kinds to be true first.
**Effort:** L in aggregate; each section is S–M and several are independently
claimable.
**Evidence:** [`additional-wave2-markers-not-in-the-original-inventory.md`](../../known-issues/jdk-only/additional-wave2-markers-not-in-the-original-inventory.md)

## Sections, and who can take them at once

| § | What | Owns | Gated | Effort |
|---|---|---|---|---|
| §2 | JIT compatibility policy is a **process global**, and so are the direct-helper addresses — contract §2 forbids it | `jit/src/lib.rs`, `vm/src/jit/` | — | M |
| §4 | Seven JIT "thin direct call" ladders bake a native reimplementation into emitted code | `jit/src/` | prefer after L5 | M |
| §6 | `JNI_NATIVE_METHODS` is a process global; JNI dispatches are **uncounted** | `vm/src/native/jni*` | — | M |
| §8 | The interpreter substitutes a **different class's** native for unresolvable interface calls | `vm/src/runtime/interpreter/` | — | M |
| §9 | The three `redefine_immune_*` predicates take a §1.4 decision outside `resolve_dispatch` | `vm/src/` | — | S |
| §10 | The wave-1 name-only dispatch adapter hard-codes `compat_native_wins = true` | `vm/src/vm/` | — | S |
| §11 | `check_override`'s **217-disjunct, ~2,650-line** chain | `vm/src/vm/vm_exec.rs` ⚠ | after L11 | L |

⚠ §11 collides with L4 and L11 in `vm_exec.rs`. Sequence it last.

## The standing rule: gate, do not remove

**Read `README.md`'s *The end state is two modes, and it is a rename* before
removing anything.** It governs every section below.

The three modes collapse to two by renaming, not by purging: today's
`--jdk-only` becomes `--real-jdk`, today's `--real-jdk` becomes
`--synthetic-jdk`. **A native that is load-bearing in either surviving mode
must survive.** Strict mode declines to *admit* it; nothing deletes it.

The distinction this lane keeps needing is between a *policy artefact* and an
*implementation*:

* **Policy artefacts** — hard-coded name lists, `matches!` chains, per-path
  copies of a decision, `compat_native_wins = true`, the
  `redefine_immune_*` predicates. These exist to answer "who wins dispatch".
  Delete them and let `resolve_dispatch` answer from `NativeKind` +
  `Method::code()`. That is the whole point of §9, §10 and §11.
* **Implementations** — the Rust natives themselves. Never deleted by this
  wave. If one is wrong in `Compatible` mode, fix it; if it must not run under
  strict policy, tag it `NativeKind::SyntheticStub` and let the registry refuse
  it. "It is unreachable in strict mode" is not a reason to delete it, because
  it is reachable in the other mode.

§11's 217-disjunct `check_override` chain is the sharpest case: it is a policy
artefact of 2,650 lines, and collapsing it must not take out a single native it
currently routes to.

The two axes are independent, and conflating them is how a correct change gets
argued about:

| Axis | Question | Mechanism | Changed by the rename? |
|---|---|---|---|
| **Correctness** | is real bytecode present for this class, and does the synthetic surface corrupt its layout? | `set_drop_real_layout_synthetic` | **No.** A synthetic surface that corrupts a real object is dropped wherever the real class is loaded, in every mode. |
| **Policy** | is synthetic behaviour admissible at all? | `CompatibilityMode` / `NativeKind` / `CRATONVM_NO_STUBS` | **Yes.** This is the axis the two surviving mode names describe. |

## Measure before rewriting — §4 especially

**§1 is closed as *answered*, not implemented**, and it is the cautionary tale
for this whole lane. Its record claimed the strict blanket refusal "costs
`JdkOnly` runs every inline-cached native call". The counter for that claim was
already in `--jdk-only-report`. Measured on three workloads including a
JIT-hot one:

| counter | IcHot | CensusLoad | Breadth |
|---|---:|---:|---:|
| `jit_inline_cache_natives` | **0** | **0** | **0** |
| `jit_direct_native_binds` | 0 | 0 | 0 |
| `jit_fastpath_admissions` | 0 | 0 | 0 |
| `interpreter_bytecode_preferred` | 3,344 | 3,254 | 532 |

The refusal never fires: every MIC/PIC publication takes its entry from
`try_jit_compile_callee`, which is owned, so `jit_entry_publishable` returns
early before the strict branch. A native trampoline never reaches it.

**`jit_direct_native_binds` is zero too — ask what §4's seven ladders actually
cost before rewriting them.** The one non-zero column is §7 step 3, three orders
of magnitude above everything else.

## §11 — the 217-disjunct chain

Of ~250 disjuncts in `check_override`, **exactly one** survives contract §7:
`method.is_abstract()` (§7 step 3b — no `Code`, so a registered native is the
only thing to run). Every other disjunct is a class-name exception saying "prefer
our native over the real JDK's concrete bytecode", which is what §1.4 forbids.

Under `--jdk-only` the replacement is *nothing*: `resolve_dispatch` step 3
returns `Bytecode` for all of them. Removing them under `Compatible` is a
separate, per-family exercise — **each entry is load-bearing for a real boot
today.** Do not treat this as one deletion.

## §2 and §6 — the process globals

Contract §2: no process globals for this feature's state. These are the two
existing violations; the constraint exists so a second VM in the same process
cannot inherit the first's policy. Note `jdk_only_refusal_counts()` already
guards against exactly that ("a `Compatible` report cannot inherit a sibling
VM's latched-strict counters") — copy the reasoning.

## Verification

Per section, but universally: measure the counter before rewriting anything that
claims a cost, both probes vs HotSpot in both modes, and `Compatible`
byte-for-byte.

## Done when

Each section is either fixed, or closed as *answered* with the measurement that
retired it — §1 and §5 are both precedents for the second outcome, and it is a
legitimate one.
