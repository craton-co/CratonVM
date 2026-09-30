# JIT round 14 wave 7, lane mega: proposals

Ranked. Each is gated on a count this wave made readable or on a run already named on its page.

## MG7-1. Bind a site's shared-table selector at its first successful resolution

Shape 4 of `r12w3-mega2-megamorphic-interface-call-remaining-round-trips-20260926.md`: a PIC binds
its selector only in `helpers.rs` `install_mega_dispatch_way`, i.e. at the site's first
PUBLICATION of a compiled body. A fresh site whose first receivers have interpreted callees (or
whose publications are barred) keeps reading nothing from the table while it holds every way.

* Change: in `jit_invoke_virtual_mic_body`'s resolving arms, once the target resolution for any
  receiver SUCCEEDED (every caller-dependent check -- constant-pool resolution and its access
  checks -- has passed; selection errors are receiver-dependent and never publish), intern the
  selector under `mega_selector_context` and `bind_mega_dispatch` it, even when nothing is
  published. The binding soundness argument of M3-2 (`mega_selector_context` doc) is preserved: the
  site binds only after its own resolution passed the only caller-dependent checks. Switch
  `CRATONVM_JIT_MEGA_BIND_AT_RESOLUTION`.
* Benefit: the `mic_unbound_table_held` count (this wave's census) of full resolutions per run.
* Cost / risk: one selector interning per fresh site (a lock and three boxed strings); low.
* Gate: land only if `R14MegaUnboundSites` or a Spring census shows `mic_unbound_table_held`
  growing with the call count rather than tens per fresh site.
* First step: read the census on `R14MegaUnboundSites` and on the Spring `MergedAnnotations` run
  under `CRATONVM_DBG=mic-prof`.

## MG7-2. Count what the M8-1 catch-up lifts

`JitPICSlot::install` returns only "refused for a lagging grace AFTER the retry", so a run cannot
say how many refusals M8-1's catch-up (and this wave's self-stamp) lifted -- the number that says
whether the catch-up rate limit (`IC_GRACE_CATCH_UP_INTERVAL` = 64) is right.

* Change: return a small outcome enum from `install` (`Published`, `Held`, `LagRefused`,
  `LagLifted`) with a `pub fn refused_for_grace_lag(self) -> bool` so the three helper call sites
  keep their shape; the helpers count `mic_grace_lag_lifted` beside `mic_grace_lag_refused`.
* Cost: a signature change across `inline_cache_pic.rs` tests and three `helpers.rs` sites; no
  runtime cost off mic-prof. Risk low.
* First step: only if the orchestrator's `R14MicSpinnerMisses` / `R14MegaSelfStamp` runs leave the
  catch-up's effect ambiguous.

## MG7-3. The shape-4 census for blind `jit_invoke_dispatch` sites (shape 6)

`jit_invoke_dispatch_body`'s virtual arm publishes into the shared table but never reads it
(shape 6). The same peek `note_unbound_selector_table_held` does (it interns nothing:
`MegaDispatchTable::interned_selector_in_context`) placed before that arm's
`virtual_dispatch_target_cached` would count the calls the table could have answered
(`out_blind_table_held`). If that count is large, the fix is a per-(site, thread) memoized selector
and a table read before the target resolution, with the same owner / epoch re-validation
`try_mega_dispatch_table_entry` does.

* Cost: a few lines in the interpreter round's hunk of `helpers.rs` (hence a proposal, not landed
  here). Risk nil off mic-prof.

## MG7-4. Decide the handshake default from one run and retire the grace page

`r12w7-mega6-grace-starves-while-a-thread-stays-compiled-CLOSED-20260929.md` is CLOSED-PENDING on one
run: `R14MicSpinnerMisses` phase `pure` under `CRATONVM_DBG=mic-prof`, default against
`CRATONVM_JIT_IC_GRACE_HANDSHAKE_MS=100`. Split its census per phase so the decision does not need
two processes: print the `[DISP_CENSUS]` line on demand (e.g. from `System.gc()`'s `door=` hook or
a `jcmd`-style trigger the VM already has), or run the probe once per phase with an argument.
The cheaper route is the argument: `R14MicSpinnerMisses pure` running phase `pure` only.
