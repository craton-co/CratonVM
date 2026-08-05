# Flag declaration audit (`CRATONVM_*`)

> Status as of 2026-08-01, branch `feat/c2-review-remediation`.
> Scope: make the declarations for this wave's new JIT capabilities real, then
> census the whole workspace against the declared inventory.
>
> The prompt for this lane carried a claim from
> `docs/known-issues/c2/deep-research-vm-c2.md:19` — *"74 undeclared, including one
> that weakens a security control"*. That number is now **0 undeclared** under
> the workspace guard, and the security-relevant one is named below.

## What "declared" actually buys

Four mechanisms key off the inventory, and they are not the same mechanism.

| | declared | undeclared |
|---|---|---|
| source of the value | the latched `VmFlags` snapshot | live `getenv` |
| reachable from `CRATONVM_<GROUP>=token` | yes | **no** |
| reachable from a launcher's `flags::install` / `-XX:` | yes | **no** |
| visible to `flags::with_thread_overrides` | yes | **no** |
| listed in `docs/CONFIG.md` / `docs/flag-tokens.md` | yes | no |

The mechanism is `types/src/flags.rs:2281-2304`: `runtime_var` and
`runtime_var_os` test the key against `declared_flag_names()` (built from
`INVENTORY.on_key` + `INVENTORY.off_key` + `SCALARS`, at `flags.rs:1930`). A hit
is served from `flags().legacy_var_os(..)`, i.e. from the one immutable
snapshot. A miss falls through to `std::env::var[_os]` verbatim.

**The claim the other lanes were given — "an undeclared flag is invisible to
tests" — is right about the conclusion and wrong about the mechanism, in a way
that matters.** An undeclared flag is not invisible to the *environment*: a
`set_var` or an exported variable still reaches it, because the fall-through is
a live read. What it is invisible to is the *supported override hook*.
`with_thread_overrides` (`flags.rs:2261`) builds a `VmFlags` and installs it as
the snapshot; an undeclared name is never read from that snapshot, so the
override has no effect and the flag keeps whatever the developer's ambient
environment says. The failure is silent — the test passes or fails for a reason
unrelated to what it claims to check.

The inverse is equally true and is the trap in the other direction: once a flag
**is** declared, `set_var` stops working, because the snapshot latches on first
read (`flags.rs:2015`, `FLAGS.get_or_init`). `types/tests/flag_env_mutation_guard.rs`
exists to catch that.

That inversion is why every row added here was checked against its consumer's
test strategy first. All of them drive a `#[cfg(test)]` thread-local override
(`bce::RANGE_BCE_TEST_OVERRIDE`, `ir_lower::LS_FORCE`,
`VecEmitPolicy::from_flag_value` taking a plain `Option<&str>`) rather than the
process environment, so declaring them cannot make an existing test vacuous.

Three gates enforce the boundary. Two are `cargo test`, one is CI shell:

* `types/tests/flag_surface.rs` — `INVENTORY` and `types/tests/flag-surface.txt`
  must name exactly the same set. Both are hand-edited files, so this only
  catches a *declaration* that went missing.
* `types/tests/flag_declaration_guard.rs` — scans every `.rs` file in the
  workspace for exact `"CRATONVM_…"` literals and fails on any that is neither
  declared nor explicitly exempt. This is the one that catches a *read site*.
* `tools/flag-census/check-surface.sh` — four steps: literals in `<crate>/src`
  vs the fixture; doc-named tokens vs the inventory; the `cargo test` above;
  and `std::env::var[_os]` bypasses in the core runtime crates.

## The four requested rows: two landed, two refused

Each was re-verified at the consumer before anything was written.

| Requested | Verdict | Evidence |
|---|---|---|
| `CRATONVM_JIT_VECTORIZE` | **declared** | `jit/src/x64/vec_emit.rs:155` (`VECTORIZE_FLAG`), read at `:160` via `runtime_var_os`. |
| `CRATONVM_JIT_RANGE_BCE` | **declared** | `jit/src/x64/bce.rs:2038`, `runtime_var_os(..).is_some()`. |
| `CRATONVM_TIER_BROKER` | **refused — no consumer** | `jit/src/tiered.rs` reads `CRATONVM_TIER_C1_THRESHOLD`, `_C2_THRESHOLD`, `_OSR_THRESHOLD`, `_C2_MIN_INVOCATIONS`, `_ENABLED` (`:215-227`) and nothing else. The only occurrence of the name in the tree is a *proposal* at `docs/jit/compilation-broker.md:310`. |
| `CRATONVM_JIT_BYTECODE_UNROLL` | **refused — no consumer** | The rewriter is armed by a thread-local, `jit/src/x64.rs:23248` `set_bytecode_loop_rewriter_armed`. No environment read exists; `docs/jit/loop-rewriter-wiring.md:44` says a flag "must be added" if one is wanted. |

A declared flag with no read site is worse than an absent one: it reads as
coverage, `CRATONVM_JIT=<token>` stops reporting an unknown token, and
`docs/CONFIG.md` gains a knob that does nothing — which is precisely the defect
`flag_groups.rs::tests::the_documented_no_ops_now_work` exists to have fixed
once. `knobs_without_a_consumer_are_not_declared` now pins both absences.

On the bytecode-unroll question specifically: if a flag is wanted later, seeding
the thread-local **once per compiler worker** is the only correct wiring. The
thread-local is read per compile and the arming call is not idempotent across
threads, so a per-compile `runtime_var` would pay a snapshot lookup on every
compilation for a value that cannot change — the snapshot has already latched.

### Polarity: where the requested rows were wrong

Two of the four requests carried `off_word: Some("0")`. Both were dropped, one
of them for a correctness reason and one for a documentation reason.

* **`range-bce` — `off_word: Some("0")` would have been a live bug.** The gate
  is `runtime_var_os("CRATONVM_JIT_RANGE_BCE").is_some()`. It tests *presence*,
  so `CRATONVM_JIT_RANGE_BCE=0` enables the pass. With `off_word: Some("0")`,
  `apply()` (`flag_groups.rs:1256`) writes `"0"` for a negated token, so
  `CRATONVM_JIT=-range-bce` would have switched guard-dominated bounds-check
  elimination **on**. That pass deletes bounds checks and has no differential
  run behind it; a wrong elision is an out-of-bounds heap write. The spelling
  that means "off" must not be the one that arms it.
* **`vectorize` and `ir-linear-scan` — correct either way, dropped for
  honesty.** Both parsers do read `"0"` as false, so `Some("0")` would have
  behaved identically to `None` (unsetting is already the off state, because
  both default to OFF). It is left out because `off_word` is the field a reader
  uses to tell a default-ON knob from a default-OFF one — the invariant is
  stated in `flag_groups.rs::tests::every_entry_can_be_switched_both_ways`
  ("off_word only applies to a default-ON knob"), and the module's whole premise
  is that polarity is stated once, unambiguously.

`default_off_capabilities_are_switched_off_by_unsetting_them` pins this for
all four default-OFF capability rows, including the stale-parent-shell case
(`CRATONVM_JIT=-range-bce` with `CRATONVM_JIT_RANGE_BCE=1` already exported must
clear the key, not give it a value).

### The BCE family question

The lane was asked to check whether the neighbouring bounds-check switches were
themselves undeclared, on the theory that a gap would be a family-wide finding.
**They were not.** All three were already declared before this pass:
`CRATONVM_JIT_NO_BCE` (token `bce`, `flag_groups.rs:582`),
`CRATONVM_JIT_NO_SPEC_BCE` (`spec-bce`, `:725`) and
`CRATONVM_JIT_INCLUSIVE_BCE` (`inclusive-bce`, `:611`). `range-bce` was the only
member missing. `the_bounds_check_family_is_declared_in_full` now asserts all
four, plus the property that makes the narrow opt-in safe: `-bce` still kills
every reason including this one.

## Rows added

Twenty-six, all with a verified read site. The four capability rows above, plus
the extra one requested mid-lane, plus the twenty-one the census turned up.

| Group / token | Key(s) | Consumer | Polarity |
|---|---|---|---|
| `JIT/vectorize` | `CRATONVM_JIT_VECTORIZE` | `jit/src/x64/vec_emit.rs:160` | opt-in |
| `JIT/range-bce` | `CRATONVM_JIT_RANGE_BCE` | `jit/src/x64/bce.rs:2038` | opt-in, presence-parsed |
| `JIT/ir-linear-scan` | `CRATONVM_JIT_IR_LINEAR_SCAN` | `jit/src/ir_lower.rs:6041` | opt-in |
| `JIT/activation-global-mutex` | `CRATONVM_JIT_ACTIVATION_GLOBAL_MUTEX` | `types/src/jit_activation.rs:123` | opt-in, presence-parsed |
| `JIT/mic-exc-table-publish` | `CRATONVM_JIT_MIC_EXC_TABLE_PUBLISH` | `vm/src/jit/helpers.rs:1862` (`mic_publish_exception_table_callees`) | opt-in |
| `JIT/mic-rust-entry-cache` | `CRATONVM_JIT_NO_MIC_RUST_ENTRY_CACHE` | `vm/src/jit/helpers.rs:1871` (`mic_rust_entry_cache_enabled`) | default-ON, opt-out key |
| `JIT/precise-virtual-invokes` | `CRATONVM_JIT_NO_PRECISE_VIRTUAL_INVOKES` | `jit/src/lib.rs:11835` | default-ON, opt-out key |
| `JIT/osr-dead-locals` | `CRATONVM_JIT_OSR_DEAD_LOCALS` | `jit/src/lib.rs:3372` (`osr_dead_local_entry_allowed`) | default-ON, `off_word "0"` |
| `JIT/sp-inline-ic` | `CRATONVM_JIT_SP_INLINE_IC` | `jit/src/x64/licm.rs:1186` | default-ON, exact `"0"` |
| `JIT/sp-tailcall` | `CRATONVM_JIT_SP_TAILCALL` | `jit/src/x64/licm.rs:1202` | default-ON, exact `"0"` |
| `JIT/shadow-end-guard` | `CRATONVM_SHADOW_NO_END_GUARD` | `jit/src/lib.rs:11017` | default-ON, opt-out key |
| `JIT/shadow-overflow-diag` | `CRATONVM_SHADOW_OVERFLOW_DIAG` | `jit/src/x64/licm.rs:1216` | opt-in |
| `JIT/statics-index` | `CRATONVM_NO_STATICS_INDEX` | `vm/src/vm/vm_object.rs:1290` | default-ON, opt-out key |
| `JIT/xt-peer-deadline-ms` | `CRATONVM_XT_PEER_DEADLINE_MS` | `vm/src/jit/xt_root_scan.rs:107` | value, default 20 |
| `GC/exact-refproc-survival` | `CRATONVM_NO_EXACT_REFPROC_SURVIVAL` | `types/src/flags.rs:892` | default-ON, opt-out key |
| `GC/oldgen-coalesce` | `CRATONVM_NO_OLDGEN_COALESCE` | `types/src/flags.rs:893` | default-ON, opt-out key |
| `GC/lhm-root-all` | `CRATONVM_LHM_ROOT_ALL` | `native-collections/src/lib.rs:29250` | opt-in |
| `THREADS/striped-counters` | `CRATONVM_STRIPED_COUNTERS_OFF` | `types/src/striped_counter.rs:72` | default-ON, opt-out key |
| `DBG/callee-probe` | `CRATONVM_DBG_CALLEE_PROBE` | `vm/src/runtime/interpreter/invoke.rs:17855` | opt-in |
| `DBG/getstatic-prof` | `CRATONVM_DBG_GETSTATIC_PROF` | `vm/src/jit/helpers.rs:5691` (`getstatic_prof::enabled`), `vm/src/vm/vm_object.rs:1281` | opt-in |
| `DBG/hminit-purge` | `CRATONVM_DBG_HMINIT_PURGE` | `native-collections/src/lib.rs:8120` | opt-in |
| `DBG/ir-linear-scan` | `CRATONVM_DBG_IR_LINEAR_SCAN` | `jit/src/ir_lower.rs:6369` | opt-in |
| `DBG/map-miss-audit` | `CRATONVM_DBG_MAP_MISS_AUDIT` | `native-collections/src/lib.rs:8129` | opt-in |
| `DBG/objkey` | `CRATONVM_DBG_OBJKEY` | `native-collections/src/lib.rs:877` | opt-in |
| `DBG/oldsweep-owners` | `CRATONVM_DBG_OLDSWEEP_OWNERS` | `gc/src/gen_heap.rs:9102` | opt-in |
| `DBG/overlay-prune` | `CRATONVM_DBG_OVERLAY_PRUNE` | `vm/src/runtime/interpreter.rs:2290` | opt-in |

`ir-linear-scan` is one token name in two groups — the capability in `JIT`, its
trace in `DBG` — following `ir-long` and `xt-jit-root-scan`. They must stay two
distinct keys or `CRATONVM_DBG=all` would arm a register allocator;
`a_token_shared_between_groups_stays_two_keys` pins that.

## The census

Method: replicate `flag_declaration_guard.rs`'s scanner — every exact
`"CRATONVM_…"` literal (closing quote immediately after the name) on a
non-comment line, across all 876 `.rs` files outside `target/`, `.git/`,
`apps/`, `node_modules/` — and diff against `INVENTORY ∪ SCALARS ∪ groups`.

| | before | after |
|---|---|---|
| distinct `CRATONVM_*` literals in the workspace | 686 | 686 |
| declared names | 648 | 674 |
| guard offenders (undeclared, not exempt) | **27** | **0** |
| `INVENTORY` rows | 623 | 649 |

The 27 offenders by crate, before this pass:

| Crate | Count |
|---|---|
| `jit` | 10 |
| `vm` | 7 |
| `native-collections` | 4 |
| `types` | 4 |
| `gc` | 1 |
| `libcratonvm` | 1 |

Twenty-six became inventory rows. The twenty-seventh is `CRATONVM_` — the bare
prefix, not a variable. It is matched because a quote follows the underscore
directly, and it appears at `libcratonvm/src/lib.rs:2814`, `vm/src/config.rs:2250`
(two `with_env` test helpers that `debug_assert!` the key they are about to
`set_var` does *not* start with it — they are refusing to touch declared flags)
and `vm/src/vm/vm_init.rs:6947` (picking which of `std::env::vars()` to report as
VM configuration). It now has an `ALLOWED` row, and
`the_scanner_only_matches_whole_string_literals` gained an assertion pinning
that the matcher does match it, so nobody "fixes" the matcher into blindness.

**The 74 in the report is not reproducible as stated and the trend is the point.**
`flag_declaration_guard.rs`'s own module docs record 63 at the time that test was
written; this pass found 27 and closed them. The report's line has no
enumeration attached anywhere in the tree, so the 74 cannot be audited item by
item — what can be said is that the guard now passes with a twelve-row
allowlist, every row of which names a reason.

### The security-relevant one

`CRATONVM_SHADOW_NO_END_GUARD`, read at `jit/src/lib.rs:11017`:

```rust
pub fn shadow_end_guard_enabled() -> bool {
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_SHADOW_NO_END_GUARD").is_none()
    })
}
```

Setting it suppresses the `end` overflow guard that **both** backends emit ahead
of a shadow-stack push, "restoring the pre-guard behaviour where an overrunning
push stores straight on through the allocator arena" — i.e. an out-of-bounds
write, deliberately, as a bisection aid. Undeclared, it was served by a live
`getenv`: an inherited export in a parent shell disarmed a memory-safety bound
with no way for the launcher, for `-XX:`, or for a test override to see it or
say otherwise, and it appeared in no generated documentation. It is now
`CRATONVM_JIT=-shadow-end-guard`, inside the snapshot, and documented in
`docs/CONFIG.md` with what it costs.

Two more are safety-relevant without being security controls, and are called out
in their inventory comments rather than here: `GC/exact-refproc-survival` and
`GC/oldgen-coalesce` are opt-outs that *reinstate a known, fixed defect*
(stale-address writes in post-GC reference processing; monotonic old-gen
fragmentation). They exist for A/B isolation, which is exactly the argument for
having them inside a surface somebody can enumerate.

## Two pre-existing gate failures found and one fixed

* **`flag_surface.rs` was red on this branch.** `CRATONVM_DBG_SWEEP_LIVENESS`
  was added to `INVENTORY` by `6ce9be3ab` ("diag(gc): assert nothing live still
  points at a block the old-gen sweep frees") without the matching line in
  `types/tests/flag-surface.txt`, so
  `inventory_matches_the_checked_in_surface` failed on the `extra` assertion.
  Fixed.
* **`check-surface.sh` step 4 was, and still is, red.** Nineteen
  `std::env::var[_os]` call sites in the core runtime crates bypass
  `runtime_var`. One of them was in `types/` and is fixed (below); the other
  nineteen are in `vm`, `jit`, `gc`, `classloading` and `native-builtins` and
  belong to other lanes.

### `types/src/error.rs` bypassed the boundary, with an inverted rationale

`java_home()` (`types/src/error.rs:618`) read `CRATONVM_JAVA_HOME` and
`JAVA_HOME` through `std::env::var_os`, documented as deliberate: binding a
diagnostic to a latching snapshot would supposedly make its text depend on who
read a flag first.

That is the wrong way round. The snapshot is fixed for the life of the process;
`environ` is what gets rewritten in place, by
`flag_groups::expand_process_env()` (`flag_groups.rs:1336`). And
`CRATONVM_JAVA_HOME` is a declared scalar, so a launcher that supplies it via
`flags::install` never touches `environ` at all — the raw read would then name a
different JDK in the report than the one the VM booted. Switched to
`flags::runtime_var_os`, which changes nothing for `JAVA_HOME` (undeclared, so
still a live read through the same call).

## What remains

Nothing inside `types/`. Everything below needs a file this lane did not own.

1. **`check-surface.sh` step 1 reports `CRATONVM_COMPATIBILITY_JDK_ONLY`.**
   The shell scanner does not skip comment lines and has no allowlist — only a
   hardcoded `grep -vxE 'CRATONVM_(NONEXISTENT_VAR_12345|SOMETHING_BRAND_NEW|FOO)'`
   at `tools/flag-census/check-surface.sh:41`. The name is a `libcratonvm` C ABI
   integer constant, matched because `libcratonvm/src/lib.rs:3233` asserts a
   diagnostic message names it. `flag_declaration_guard.rs` already exempts it
   as kind 1. Fix: add it to that `grep -vxE` alternation. Better fix: have the
   shell script read the exemption list from one place instead of duplicating a
   subset of it.
2. **Nineteen `std::env::var[_os]` bypasses in the core runtime** keep step 4
   red. Every one reads a name that *is* now declared, so each is a one-line
   substitution of `cratonvm_types::flags::runtime_var[_os]` for
   `std::env::var[_os]` — no polarity or parsing change:
   `vm/src/config.rs:2253`, `vm/src/runtime/exceptions.rs:1938,1945`,
   `vm/src/vm/vm_exec.rs:11378`, `vm/src/vm/vm_util.rs:782`,
   `jit/src/lib.rs:8880,8970`, `gc/src/gen_heap.rs:106,9880`,
   `classloading/src/class_manager.rs:495`,
   `classloading/src/loaders.rs:176,189`,
   `native-builtins/src/classloader_real.rs:1077`,
   `native-builtins/src/lang_class.rs:1356,4439`,
   `native-builtins/src/logmanager.rs:2480`,
   `native-builtins/src/spring_startup_bootstrap.rs:145`,
   `native-builtins/src/tls.rs:305`. (`native-builtins/src/lib.rs:2629` reads
   `JBOSS_HOME`, not a CratonVM flag; it still trips the grep, which matches the
   call and not the key. That one wants either the same substitution or an
   exemption in the script.)
3. **`docs/jit/compilation-broker.md:310` and
   `docs/jit/loop-rewriter-wiring.md:44`** propose rows for
   `CRATONVM_TIER_BROKER` and `CRATONVM_JIT_BYTECODE_UNROLL`. Neither is
   declared, deliberately. Whoever wires the consumer should land the row in the
   same commit — `types/src/flag_groups.rs` and `types/tests/flag-surface.txt`,
   both edits, or `flag_surface.rs` fails.
4. **`docs/flag-tokens.md` had drifted by 91 tokens** before this pass and was
   regenerated with `tools/flag-census/render-tokens.sh`. It is generated, so it
   silently goes stale whenever someone adds a row without re-running the
   script. No gate checks it in that direction — step 2 of `check-surface.sh`
   only fails when the docs name a token the inventory does *not* have, never
   the reverse. Making that check symmetric would have caught the drift.
