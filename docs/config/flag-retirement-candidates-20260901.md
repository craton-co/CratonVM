# Flag retirement candidates — 2026-09-01

*A measured backlog, not a delete script. Every number below was re-derived on
this tree on 2026-09-01; the commands are printed so the next person can run
them instead of quoting this file.*

Nothing in this repository has ever removed a `CRATONVM_*` flag. The census that
motivated the grouping recorded the growth rate plainly — flags accumulate "at
roughly one per fixed bug with no retirement path" — and the grouping that
followed was a **renaming**: 995 declared knobs reached through 15 environment
variables is still 995 knobs. `CRATONVM_DBG` alone carries 510 tokens.

This document is the other half of the two things landed on 2026-09-01 to make
removal possible at all:

1. every row in `types/src/flag_groups.rs::INVENTORY` now carries a `since:`
   date taken from `git log`, and that file's own `mod tests` enforces a
   retirement horizon on it — a `DBG` knob declared on or after **2026-08-01**
   must be referenced somewhere outside `types/` and the generated flag
   documents;
2. this list, which is the population the horizon deliberately grandfathers.

---

## Contents

1. [Method, and the exact commands](#method-and-the-exact-commands)
2. [What the numbers came out at](#what-the-numbers-came-out-at)
3. [The 63 candidates](#the-63-candidates)
4. [Why this set is the safe place to start](#why-this-set-is-the-safe-place-to-start)
5. [What this set is not](#what-this-set-is-not)

---

## Method, and the exact commands

Run from the repository root. Nothing here builds anything.

**Step 0 — the declared surface.** `types/tests/flag_surface.rs` asserts that
`flag-surface.txt` and `INVENTORY` name exactly the same set, so the fixture is
a safe stand-in for "declared":

```bash
sort -u types/tests/flag-surface.txt > /tmp/declared.txt
wc -l < /tmp/declared.txt                                    # declared names
grep -c '^    E { group: Group::' types/src/flag_groups.rs   # INVENTORY rows
```

**Step 1 — every mention of every name, with the file it is in.** `-I` skips
binaries; `.agent-requests/` is a transient inter-agent scratch directory and is
not documentation, so it is excluded — leaving it in credits five flags with a
"doc mention" that will not exist tomorrow.

```bash
grep -rIo --exclude-dir=target --exclude-dir=.git --exclude-dir=apps \
     --exclude-dir=node_modules --exclude-dir=.agent-requests \
     -E 'CRATONVM_[A-Z0-9_]+' . | sed 's|^\./||' > /tmp/mentions.txt
```

**Step 2 — which names anybody outside the registry can actually find.** Five
things are not evidence that a knob is wanted, and all five are cut here:

| Cut | Why |
|---|---|
| `types/**` | the declaration itself — a row citing its own registry proves nothing |
| `docs/config/flag-inventory.md`, `docs/flag-tokens.md` | generated *from* that registry, so every declared name is in them by construction |
| **this file** | it names a knob in order to propose deleting it, which is the opposite of a consumer — and it is self-referential: leaving it in took the candidate set from 63 to **0**, because the table below "documents" every row in it |
| everything under the `internal` subtree of `docs` | historical bug write-ups, stripped from published history: a name whose only mention is there has no reader outside a working copy |
| `*.rs` | a read site is counted separately in step 3; this step asks whether the flag is **operator-facing or referenced by CI** |

```bash
awk -F: '$1 !~ /^types\// && $1 !~ /^docs\/internal\// \
      && $1 != "docs/config/flag-inventory.md" \
      && $1 != "docs/flag-tokens.md" \
      && $1 != "docs/config/flag-retirement-candidates-20260901.md" \
      && $1 !~ /\.rs$/ {print $2}' /tmp/mentions.txt | sort -u > /tmp/documented.txt
comm -23 /tmp/declared.txt /tmp/documented.txt > /tmp/undocumented.txt
wc -l < /tmp/undocumented.txt
```

**Step 3 — read sites.** The counting rule that matters: a name's read count is
its `--include='*.rs'` hits across the member crates **minus** its hits in
`types/src/flag_groups.rs`, because the registry names every variable and that
is a declaration, not a read. Omitting that subtraction gives every flag a
floor of one and makes the whole exercise return nothing.

```bash
CRATES="reader types native-api native-collections native-io native-builtins \
native-builtins-crypto native-builtins-security native-awt jit-api jit jit-cuda \
cuda-bridge classloading craton-gpu gc vm vm-cli jfr libcratonvm cratonvm-embed difftest"

grep -rIon --include='*.rs' -E 'CRATONVM_[A-Z0-9_]+' $CRATES \
  | grep -v '^types/src/flag_groups.rs:' | sort -u > /tmp/readsites.txt

awk -F: '{print $3}' /tmp/readsites.txt | sort | uniq -c \
  | awk '{print $2, $1}' | sort > /tmp/readcounts.txt

join -a1 -e 0 -o '0,2.2' /tmp/undocumented.txt /tmp/readcounts.txt \
  | awk '$2 <= 1' > /tmp/candidates.txt
wc -l < /tmp/candidates.txt
```

`/tmp/readsites.txt` is `path:line:NAME`, so the single read site quoted in the
table below is a `grep` line, not a claim about semantics.

---

## What the numbers came out at

| | 2026-09-01 | the figure the audit brief carried |
|---|---|---|
| declared knobs (`INVENTORY` keys + 5 scalars + 10 group variables) | **995** | 986 |
| … of which `INVENTORY` rows (tokens) | 970 | — |
| declared names with **no** operator- or CI-facing mention (step 2) | **488** | 491 |
| … of those, with **at most one** Rust read site (step 3) | **63** | 65 |

The brief's 986 / 491 / 65 and these 995 / 488 / 63 are the same measurement
taken hours apart on a branch several agents are editing concurrently; eight of
the nine extra declared names are the kill switches registered during this same
pass, all on the same day. That is the point of printing the commands: the
numbers move — they moved twice while this file was being written — and only the
method is quotable.

Distribution of the 488 across groups:

| Group | names |
|---|---|
| `DBG` | 271 |
| `JIT` | 127 |
| `GC` | 35 |
| `REAL` | 16 |
| `THREADS` | 9 |
| `COMPAT` | 8 |
| `TEST` | 7 |
| `IO` / `LOADER` / `SECURITY` | 5 each |

Read-site distribution across those same 488 — the reason the cut is drawn at
one and not at two:

| Rust read sites | names |
|---|---|
| 1 | 63 |
| 2 | 169 |
| 3 | 126 |
| 4 | 52 |
| 5 | 31 |
| 6 or more | 47 |

**Zero** of the 488 have no read site at all. There is no already-dead flag in
this tree to sweep up; every removal is a real edit to real code, which is
exactly why the one-read-site set is where to begin.

---

## The 63 candidates

Each row is one *variable*. The 63 variables are 62 tokens: `CRATONVM_JIT=stack-bang`
appears twice because it is the one candidate whose on-key and off-key are both
spelled out (`CRATONVM_JIT_STACK_BANG` / `CRATONVM_JIT_NO_STACK_BANG`), and they
must be retired together or not at all.

`since:` is the date from the `INVENTORY` row — the earlier of "the row appeared
in `flag_groups.rs`" and "the name appeared anywhere in the tree", both from
`git log`. 32 of the 63 predate the 2026-08-01 retirement horizon and 31 do not;
the horizon governs whether a *new* row may look like this, not whether an
existing one must go.

| Variable | Group | Canonical spelling | `since:` | Its one read site |
|---|---|---|---|---|
| `CRATONVM_VH_STRICT_REFERENCE_RETURN` | `COMPAT` | `CRATONVM_COMPAT=vh-strict-reference-return` | 2026-08-07 | `vm/src/vm/vm_exec.rs:2573` |
| `CRATONVM_DBG_CALLEE_DEOPT` | `DBG` | `CRATONVM_DBG=callee-deopt` | 2026-08-05 | `vm/src/runtime/env_cache.rs:1531` |
| `CRATONVM_DBG_CHECKCAST_INLINE` | `DBG` | `CRATONVM_DBG=checkcast-inline` | 2026-08-28 | `jit/src/x64/bytecode_walk.rs:12649` |
| `CRATONVM_DBG_DEFINE_STACK_FILTER` | `DBG` | `CRATONVM_DBG=define-stack-filter` | 2026-08-13 | `native-builtins/src/lang_system.rs:6338` |
| `CRATONVM_DBG_DEOPTSLOT` | `DBG` | `CRATONVM_DBG=deoptslot` | 2026-07-31 | `vm/src/runtime/interpreter/deopt_resume.rs:910` |
| `CRATONVM_DBG_DUPX_TRACE` | `DBG` | `CRATONVM_DBG=dupx-trace` | 2026-08-05 | `jit/src/x64/bytecode_compat.rs:70` |
| `CRATONVM_DBG_FIELD_SITE` | `DBG` | `CRATONVM_DBG=field-site` | 2026-08-04 | `vm/src/runtime/interpreter/site_cache.rs:436` |
| `CRATONVM_DBG_GC_OVERHEAD` | `DBG` | `CRATONVM_DBG=gc-overhead` | 2026-06-21 | `vm/src/runtime/interpreter/gc_and_alloc.rs:1702` |
| `CRATONVM_DBG_INVSPECIAL` | `DBG` | `CRATONVM_DBG=invspecial` | 2026-07-25 | `vm/src/runtime/interpreter/invoke.rs:2461` |
| `CRATONVM_DBG_IRSLOT` | `DBG` | `CRATONVM_DBG=irslot` | 2026-07-10 | `jit/src/ir_lower.rs:1519` |
| `CRATONVM_DBG_IR_BAILOUT` | `DBG` | `CRATONVM_DBG=ir-bailout` | 2026-07-31 | `jit/src/ir_lower.rs:9723` |
| `CRATONVM_DBG_IR_LONG` | `DBG` | `CRATONVM_DBG=ir-long` | 2026-06-21 | `jit/src/lib.rs:23301` |
| `CRATONVM_DBG_IR_RELOC` | `DBG` | `CRATONVM_DBG=ir-reloc` | 2026-07-31 | `jit/src/ir_lower.rs:1645` |
| `CRATONVM_DBG_JIT_SAFEPOINTS` | `DBG` | `CRATONVM_DBG=jit-safepoints` | 2026-07-24 | `vm/src/jit/helpers.rs:24648` |
| `CRATONVM_DBG_JNI_LOCALREF` | `DBG` | `CRATONVM_DBG=jni-localref` | 2026-08-26 | `vm/src/native/jni.rs:2170` |
| `CRATONVM_DBG_LAMBDA_PROF` | `DBG` | `CRATONVM_DBG=lambda-prof` | 2026-08-10 | `vm/src/runtime/interpreter/lambda.rs:59` |
| `CRATONVM_DBG_LHM_EVICT` | `DBG` | `CRATONVM_DBG=lhm-evict` | 2026-07-06 | `native-collections/src/lib.rs:44955` |
| `CRATONVM_DBG_LICM` | `DBG` | `CRATONVM_DBG=licm` | 2026-06-22 | `jit/src/ir_optimize.rs:1329` |
| `CRATONVM_DBG_LOAD_TRANSFORM_NO_MEMO` | `DBG` | `CRATONVM_DBG=load-transform-no-memo` | 2026-08-10 | `vm/src/runtime/instrument.rs:1631` |
| `CRATONVM_DBG_LOOP_WORK` | `DBG` | `CRATONVM_DBG=loop-work` | 2026-08-04 | `vm/src/runtime/interpreter.rs:4874` |
| `CRATONVM_DBG_MONITOR_NOTIFY` | `DBG` | `CRATONVM_DBG=monitor-notify` | 2026-08-24 | `vm/src/threading/monitor.rs:255` |
| `CRATONVM_DBG_NATIVE_SHADOW` | `DBG` | `CRATONVM_DBG=native-shadow` | 2026-08-30 | `vm/src/runtime/env_cache.rs:1625` |
| `CRATONVM_DBG_OOM_BT` | `DBG` | `CRATONVM_DBG=oom-bt` | 2026-07-31 | `gc/src/gen_heap.rs:13907` |
| `CRATONVM_DBG_OSR_META` | `DBG` | `CRATONVM_DBG=osr-meta` | 2026-07-10 | `jit/src/x64/osr.rs:568` |
| `CRATONVM_DBG_OSR_SEED_COLLISION` | `DBG` | `CRATONVM_DBG=osr-seed-collision` | 2026-08-06 | `jit/src/x64/osr.rs:480` |
| `CRATONVM_DBG_OVERLAY_BT` | `DBG` | `CRATONVM_DBG=overlay-bt` | 2026-08-04 | `vm/src/vm/vm_exec.rs:5584` |
| `CRATONVM_DBG_OVERLAY_NODEDUP` | `DBG` | `CRATONVM_DBG=overlay-nodedup` | 2026-08-05 | `vm/src/vm/vm_exec.rs:5611` |
| `CRATONVM_DBG_QUARKUS_STATICINIT` | `DBG` | `CRATONVM_DBG=quarkus-staticinit` | 2026-07-26 | `types/src/flags.rs:2043` |
| `CRATONVM_DBG_RVAS` | `DBG` | `CRATONVM_DBG=rvas` | 2026-06-21 | `libcratonvm/src/lib.rs:4104` |
| `CRATONVM_DBG_SHADOW2_FILTER` | `DBG` | `CRATONVM_DBG=shadow2-filter` | 2026-07-01 | `jit/src/x64/licm.rs:1460` |
| `CRATONVM_DBG_SP_IC_SITES` | `DBG` | `CRATONVM_DBG=sp-ic-sites` | 2026-08-05 | `jit/src/x64/bytecode_compat.rs:193` |
| `CRATONVM_DBG_STW_NATIVE_RING` | `DBG` | `CRATONVM_DBG=stw-native-ring` | 2026-07-05 | `vm/src/runtime/interpreter/gc_and_alloc.rs:510` |
| `CRATONVM_DBG_UCLTRACE` | `DBG` | `CRATONVM_DBG=ucltrace` | 2026-07-26 | `native-builtins/src/classloader.rs:7995` |
| `CRATONVM_DBG_VIEW_COMOD` | `DBG` | `CRATONVM_DBG=view-comod` | 2026-08-24 | `native-collections/src/lib.rs:20836` |
| `CRATONVM_DBG_WATCHADDR` | `DBG` | `CRATONVM_DBG=watchaddr` | 2026-07-25 | `vm/src/memory/gc.rs:560` |
| `CRATONVM_DBG_ZGC_HIGH` | `DBG` | `CRATONVM_DBG=zgc-high` | 2026-08-29 | `gc/src/zgc.rs:6181` |
| `CRATONVM_INVOKESTATIC_LOADER_TRACE` | `DBG` | `CRATONVM_DBG=invokestatic-loader-trace` | 2026-07-22 | `vm/src/runtime/interpreter/dispatch_static.rs:259` |
| `CRATONVM_MOVING_YOUNG_COVERAGE_DBG` | `DBG` | `CRATONVM_DBG=moving-young-coverage-dbg` | 2026-07-01 | `vm/src/jit/conservative_roots.rs:3496` |
| `CRATONVM_NEEDS_EXACT_TRACE` | `DBG` | `CRATONVM_DBG=needs-exact-trace` | 2026-07-25 | `vm/src/vm/vm_exec.rs:11484` |
| `CRATONVM_JIT_C1_VECTOR_VETO` | `JIT` | `CRATONVM_JIT=c1-vector-veto` | 2026-08-04 | `jit/src/x64/single_pass_only.rs:263` |
| `CRATONVM_JIT_CACHED_ENTRY_OWNER_REUSE` | `JIT` | `CRATONVM_JIT=cached-entry-owner-reuse` | 2026-08-17 | `vm/src/jit/helpers.rs:1805` |
| `CRATONVM_JIT_CENSUS_DIRECT_HELPERS` | `JIT` | `CRATONVM_JIT=census-direct-helpers` | 2026-08-17 | `jit/src/lib.rs:11476` |
| `CRATONVM_JIT_FIELD_SITE_CACHE_LOADER` | `JIT` | `CRATONVM_JIT=field-site-cache-loader` | 2026-08-04 | `vm/src/runtime/interpreter/field_access.rs:216` |
| `CRATONVM_JIT_FULL_SELF_CALL_SPILL` | `JIT` | `CRATONVM_JIT=full-self-call-spill` | 2026-07-14 | `jit/src/x64/licm.rs:2048` |
| `CRATONVM_JIT_GC_INERT_SELFREC` | `JIT` | `CRATONVM_JIT=gc-inert-selfrec` | 2026-07-30 | `jit/src/x64/licm.rs:1787` |
| `CRATONVM_JIT_LOOP_WORK_TIERUP` | `JIT` | `CRATONVM_JIT=loop-work-tierup` | 2026-08-04 | `vm/src/runtime/interpreter.rs:4866` |
| `CRATONVM_JIT_METHOD_SITE_CACHE` | `JIT` | `CRATONVM_JIT=method-site-cache` | 2026-08-04 | `vm/src/runtime/interpreter/field_access.rs:427` |
| `CRATONVM_JIT_NATIVE_SHADOW_INTERFACE_BLIND` | `JIT` | `CRATONVM_JIT=native-shadow-interface-blind` | 2026-08-10 | `vm/src/runtime/env_cache.rs:2015` |
| `CRATONVM_JIT_NO_DUP2_X2` | `JIT` | `CRATONVM_JIT=dup2-x2` | 2026-08-17 | `jit/src/x64/bytecode_compat.rs:46` |
| `CRATONVM_JIT_NO_INLINE_LIVE_SLOT_CLAMP` | `JIT` | `CRATONVM_JIT=inline-live-slot-clamp` | 2026-08-24 | `jit/src/x64/inlining.rs:45` |
| `CRATONVM_JIT_NO_MIC_RUST_ENTRY_CACHE` | `JIT` | `CRATONVM_JIT=mic-rust-entry-cache` | 2026-07-31 | `vm/src/jit/helpers.rs:2600` |
| `CRATONVM_JIT_NO_NEW_CLASS_INIT_MEMO` | `JIT` | `CRATONVM_JIT=new-class-init-memo` | 2026-08-26 | `vm/src/jit/helpers.rs:4796` |
| `CRATONVM_JIT_NO_STACK_BANG` | `JIT` | `CRATONVM_JIT=stack-bang` | 2026-07-01 | `jit/src/x64/reg_encoding.rs:112` |
| `CRATONVM_JIT_SP_IC_DENY` | `JIT` | `CRATONVM_JIT=sp-ic-deny` | 2026-08-05 | `jit/src/x64/bytecode_compat.rs:123` |
| `CRATONVM_JIT_STACK_BANG` | `JIT` | `CRATONVM_JIT=stack-bang` | 2026-07-01 | `jit/src/x64/reg_encoding.rs:115` |
| `CRATONVM_JIT_SYNC_METHODS` | `JIT` | `CRATONVM_JIT=sync-methods` | 2026-08-04 | `vm/src/runtime/interpreter/jit_bridge.rs:4616` |
| `CRATONVM_JIT_UNREG_ACCEPT_RESIDUE` | `JIT` | `CRATONVM_JIT=unreg-accept-residue` | 2026-08-07 | `vm/src/jit/conservative_roots.rs:2176` |
| `CRATONVM_SOAK_ITERS` | `TEST` | `CRATONVM_TEST=soak-iters` | 2026-06-21 | `libcratonvm/src/lib.rs:4265` |
| `CRATONVM_SOAK_K` | `TEST` | `CRATONVM_TEST=soak-k` | 2026-06-21 | `libcratonvm/src/lib.rs:4264` |
| `CRATONVM_SOAK_METHOD` | `TEST` | `CRATONVM_TEST=soak-method` | 2026-06-21 | `libcratonvm/src/lib.rs:4188` |
| `CRATONVM_SOAK_TIMEOUT_SECS` | `TEST` | `CRATONVM_TEST=soak-timeout-secs` | 2026-06-21 | `libcratonvm/src/lib.rs:4358` |
| `CRATONVM_SOAK_XMX` | `TEST` | `CRATONVM_TEST=soak-xmx` | 2026-06-21 | `libcratonvm/src/lib.rs:4142` |
| `CRATONVM_THREAD_START_GRACE_MS` | `THREADS` | `CRATONVM_THREADS=thread-start-grace-ms` | 2026-07-04 | `vm/src/vm/vm_exec.rs:2027` |

---

## Why this set is the safe place to start

**The edit is mechanical.** A flag with one read site is one `if` to delete.
Remove the branch, keep whichever side is the default, delete the four-file
registration (the `INVENTORY` row and the `flag-surface.txt` line, then re-run
`tools/flag-census/render-tokens.sh` and `render-inventory.sh`, which regenerate
the other two). There is no second call site to reason about, no interaction
with another knob to think through, and the compiler finds the mistake if the
branch was load-bearing.

**Nothing operator-facing breaks.** By construction these names appear in no
runbook, no CI script, no `docs/` page outside the generated set and the
historical archive. A name nobody has written down is a name nobody exports on
purpose — and a stale export of a deleted knob is inert anyway, because
`resolve` reports an unknown token on stderr rather than failing.

**It establishes that a flag can be removed.** That is the actual deliverable.
The surface grew to 995 because there was no worked example of removal, no test
that could ever say "this one is unused", and therefore no cost to adding one
more. The `since:` field and the horizon test give the *next* flag a deadline;
this list gives the *first* deletion a starting point. One row retired end to
end — code, registry, both generated documents, green `cargo test -p
cratonvm-types` — is worth more than all sixty-three proposed at once.

**Suggested order.** The five `CRATONVM_SOAK_*` rows are the cleanest: one
crate, adjacent lines in `libcratonvm/src/lib.rs`, a `TEST`-group harness knob
family with no production reader. `CRATONVM_DBG_QUARKUS_STATICINIT` is the
strangest and probably next — its only read site is inside `types/` itself
(`types/src/flags.rs`), so no crate outside the flag machinery consults it.

---

## What this set is not

**It is not a delete script.** It is a candidate list requiring one judgement
per row. Each of the following is a real way a row on this list is still
load-bearing:

* **A flag with no doc mention may be carrying a live investigation.** These are
  overwhelmingly diagnostics — 38 of the 63 are `DBG` — and a diagnostic added
  last week for a bug still open has exactly this profile: one read site, no
  prose, no CI. Twelve of the 63 were declared in the three weeks before this
  list was taken. Ask whoever added it before deleting it; the `since:` date and
  `git log -S'"THE_NAME"'` name them.
* **The internal write-up archive was deliberately not counted, and it is where
  the reasoning lives.** A knob cited only in an internal record is not
  *operator*-facing, which is what step 2 measures — that is not the same claim
  as "nothing explains why this exists". Read the internal page before deciding.
* **One read site is not one behaviour.** A single `if` can gate a whole
  subsystem: `CRATONVM_JIT_NO_STACK_BANG` reads once in
  `jit/src/x64/reg_encoding.rs`, and that read decides whether every compiled
  frame gets a stack banger. Deleting the flag is fine; deleting the *branch it
  guards* is not, and the two are one keystroke apart.
* **Paired names must move together.** `CRATONVM_JIT_STACK_BANG` and
  `CRATONVM_JIT_NO_STACK_BANG` are one token with two spellings. Retiring one
  leaves `types/tests/flag_surface.rs` green and the token half-reachable.
* **The scan is textual.** A name assembled at runtime
  (`format!("CRATONVM_REAL_{sub}")`) is invisible to every step above — the same
  blind spot `flag_declaration_guard.rs` states for itself.
  `cuda-bridge/src/critical.rs` is the known instance in this tree.

**No flag was deleted to produce this document.** The read sites are scattered
across files several concurrent branches are editing; the deletion is separate
work, done one row at a time, and each one should say in its commit message
which line it removed and which side of the branch it kept.
