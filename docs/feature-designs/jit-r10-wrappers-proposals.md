# Round 10, lane `wrappers` — directions worth pursuing

**Written:** 2026-09-21, round 10 wave 6.
**Files this lane owned:** `vm/src/jit/code_cache_lifecycle.rs`,
`vm/tests/no_test_only_public_api.rs`.
**Pages closed:** `r10-gauges-code-cache-push-wrappers-outlived-their-direction-20260921.md`.
**Pages opened:** `r10-wrappers-process-report-has-no-reader-and-seven-unfed-fields-20260921-RETIRED-20260922.md`.

Everything below was established by reading. This lane could not build, test or
run the two ratchets it reasoned about, and each proposal says what would have
to be measured before it is believed.

---

## P1. Give the code-cache lifecycle report a production reader — but in this order

The full argument is in the known-issues page. The design point worth recording
separately is the **sequencing**, because the obvious one-line change is the
wrong first step:

1. `jit/src/lib.rs`: add to the reclamation accounting the figures the queue can
   honestly supply and the report currently invents from an unfed model — a peak
   live-bytes high-water mark first, since it is a single `fetch_max` on the
   install path and is the field whose zero is most obviously impossible.
2. `vm/src/jit/code_cache_lifecycle.rs`: pull them; and for what still has no
   source, stop printing it. `Display` should omit a line it cannot fill rather
   than print `0`. The module already does this for two cases
   (`retire_reason_breakdown` and `allocation_failure_breakdown` both suppress an
   empty list) — the inconsistency is that the compilation line prints its zeros
   unconditionally.
3. `vm-cli/src/main.rs`: print it, beside
   `[cratonvm] implicit null-check table health:`.

Doing (3) first ships an operator a `peak` of 0 under a live figure of hundreds
of megabytes. The measurement that settles it: any workload that recompiles one
method, with the report printed — compare `peak` against `live` and
`versions_per_method` against 1.0.

## P2. Teach `check-orphan-instruments.sh` that `#[cfg(test)]` is not `pub`

This lane hit the gate's one blind spot from the inside. C1 matches
`^[[:space:]]*pub fn (record|note)_…\(` over source text with no `cfg` awareness,
so the shape "gated, private, model-only recorder" — which is what a correctly
retired instrument looks like — would have remained a candidate if it had kept
`pub`. Two consequences worth acting on:

* A lane that does the right thing but keeps `pub` (because an integration test
  needs it — the third disposition in `no_test_only_public_api.rs`'s header) gets
  no credit from this gate and must allowlist instead. That is a false positive
  the allowlist absorbs silently.
* Conversely, a `#[cfg(test)]`-gated `pub fn record_…` that nothing calls reads
  to the gate exactly like a production orphan, so the census cannot tell
  "test-only by design" from "nobody wired it".

The cheap fix is one line of context: the definition pass already runs with
`-A 12` and could equally use `-B 2`, and a `#[cfg(test)]` within two lines above
a candidate's signature is decisive. `untyped-alloc-ratchet.sh`'s column-0 block
scanner is the expensive fix and is probably not warranted. Either way the
allowlist must be re-frozen in the same change, and the delta inspected name by
name — a gate whose matching rules moved without the baseline moving with them is
how a ratchet starts reporting fiction, which this gate's own first execution
already demonstrated once.

## P3. Decide what `CodeCacheLifecycle` the model is for, once

About twenty `pub` items in `vm/src/jit/code_cache_lifecycle.rs` have no
reference anywhere outside their own file: `with_simulated_quiescence`,
`simulated_depth`, `queued_bodies`, `versions_of`, `slack_bytes`, `wx_step`,
`wx_replay`, `install_sequence_is_legal`, `is_writable`, `is_executable`,
`retire_reason_breakdown`, `allocation_failure_breakdown`, `WxState`, `WxEvent`,
`WxViolation`, `InstallProtocolError`, `BodyExtent`, `SweepOutcome`,
`CodeCacheLifecycleRaw`, `CodeCacheLifecycleReport` and the `retire_reason` /
`install_step` constants. That is not a bug — the module's header is explicit
that the type is an executable model whose tests pin the protocol — but it means
a large part of one `vm/src` module is, by construction, permanently inside
`no_test_only_public_api.rs`'s frozen 291.

Three honest end states, in increasing order of cost:

* **Leave it, and name it.** Add the model to that test's header the way
  `admit_direct_native_entry` and `release_submission` are named, so the next
  person working the backlog down does not spend an afternoon rediscovering that
  the model is deliberate. Cheapest, and it does not move the number.
* **Gate the model.** `#[cfg(test)]` everything except the four process-level
  functions (`pending_retirements`, `sweep_if_quiescent`,
  `code_cache_lifecycle_raw`, `code_cache_lifecycle_report`) and the types those
  four expose. This would lower `BASELINE_OFFENDERS` substantially.
* **Move it to `vm/tests/`.** It is a model of a protocol implemented in another
  crate; an integration test is arguably where it belongs. But it reads
  `crate::jit::conservative_roots::any_thread_in_jit`, so the production
  quiescence signal would have to be `pub` for it — which trades one kind of
  test-only surface for another.

**Why this lane did not attempt the second or third.** Both move the offender
count, `BASELINE_OFFENDERS` is an `assert_eq!`, and the exact new count cannot be
derived by reading with acceptable confidence — the scanner's cross-crate name
collisions (documented in its own header as making it *lenient*) mean the number
of names that come off is not the number of items gated. A wrong number is a red
build, and the only way to get the right one is to run the gate before and after
and `comm` the two lists. That is a job for a lane that can execute it. The
2026-09-21 change was chosen precisely because it does **not** move the number.

## P4. Two `SweepOutcome` fields the production path cannot fill

`sweep_if_quiescent()` hard-codes `oldest_deferral_sweeps: 0` and passes the
queue's `drains` as `sequence`, and `SweepOutcome`'s `Display` labels them
`sweep #{sequence}` and `oldest deferred {age} sweeps`. In the RETAINED branch —
the branch whose whole purpose is to make a stuck queue visible — the age is the
signal, and it is a constant. Either the JIT queue starts stamping a per-body
deferral age that this can pull, or these two fields should not be printed on the
process path. Small, and it is the same defect class as P1 at one field's scale.

## P5. A positive control for the pull, not just for the numbers

`the_process_report_reads_the_jits_refused_allocations` is the model this whole
area should follow: it forces a real refusal through
`platform::alloc_executable(0)` and asserts the report carries it in the right
bucket. There is no equivalent for the *install* and *retirement* halves of the
pull beyond
`the_process_report_reads_the_real_jit_accounting`'s delta assertions, and none
at all for the cap and arena-census branches (both configuration-dependent:
`CRATONVM_JIT_CODE_CACHE_MAX_MB`, `CRATONVM_JIT_CODE_ARENA`). A test that runs
with the arena enabled and asserts the free-space triple is non-zero would close
the last field group whose only evidence today is that the code looks right.
