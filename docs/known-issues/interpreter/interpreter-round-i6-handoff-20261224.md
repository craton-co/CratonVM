# Interpreter round i6: handoff (waves 115-134)

**Status: round closed 2026-10-20.** Interpreter round i6 ran waves 115-134 on branch
`claude/interp-i6-round`. It was merged into `dev` at the end of the round. Wave 134, the last,
was limited to defects. This page replaces `interpreter-round-i5-handoff-20261015.md` as the
round's entry point. The Linux triage comes after this round, and the pages below list what it
has to run.

## What the round did

- **Process:** each wave ran up to seven lanes. The lanes read and wrote code only; the
  orchestrator alone built, on Windows with fat LTO, `-j 5`, cargo pinned to the E-cores. A run
  lane per wave worked through the NEEDS-RUN list. Every page change is a dated `## Wave N`
  section on its page.
- **Retired:** more than 100 pages moved to `docs/internal/fixed-bugs/` as DONE, FIXED, CLOSED,
  KEPT, KEPT-OFF or OBSOLETE.
- **Switches:** every decision, with its numbers, is in
  `docs/internal/reviews/interpreter-switch-triage-i6-20261018.md`. Read the "Waves 131-134 flips"
  section first.
- **Run results:** `docs/internal/reviews/interpreter-i6-run-results-20261224.md` (lane R2) and
  `docs/internal/reviews/interpreter-i6-run-results-orch-20261224.md`.

## User decisions taken this round

1. **"Flip all on or close" (2026-10-19).**
   - Four switches were flipped ON in the separate commit `3c507a9735` (subject `WARNING:`):
     `CRATONVM_MONITOREXIT_FRAME_STRICT`, `CRATONVM_DEFINE_BUILTIN_CONSTRAINT_REFUSAL`,
     `CRATONVM_USER_LOADER_RESOURCE_BYTECODE` and `CRATONVM_MH_VIRTUAL_BY_OWNER_ID`. Grep the code
     for `WARNING(i6-user-flip)` to find each site.
   - These pages were closed: i69-L1, i100-L1, i100-L5, i1-L1 stage 4, i40-L1 (frame-only events
     decline methods with several returns) and i38-L4 (the old refusal copies stay).
2. **No stand-in deletion (2026-10-20).** "Keep as is: stand-ins stay compiled and reachable with
   =0, off by default in both modes, since the JDK bodies match HotSpot."
   - i52-L3, i53-L1, i56-L5, i57-L4, both i58-L5 pages and i71-L1 closed as KEPT.
   - Their censuses that could run here passed:
     - i52-L3: 0 family lines in 8 runs;
     - i57-L4: `vh-w6-1-refusal: 0` in 124/124 runs.

## Open here: defects

| Page | What is left |
|---|---|
| `i134-L2c-generic-indy-false-arm-under-jit` | Fix written (`CRATONVM_INDY_GENERIC_KEEP_PRIMITIVE_Z`, ON). Run `L2cW134GenericIndyFalseArm` and `L2bW133GenericIndyThreadRow` on the merged build. Then the Groovy witness, on Linux. |
| `i132-L6-interface-invokespecial-skips-the-receiver-subtype-check` | `L6W132PrivateIfaceThisPin` SAME on w133. The wave-134 per-VM flag read needs `L4W134IfaceSuperLoop` and the `FLAGREADS` loop. |
| `i133-L4-nojit-invoke-with-arguments-recursion-overflows-the-native-stack` | `L4W133NativeReentrySoe` and `L4W130 --nojit` SAME on w133. Re-run on the merged build for the wave-134 lazy-error change. |
| `i133-L1`, `i133-L8` (C JVMTI frame control) | Fix written in wave 134. The MSVC agent `L1W134JvmtiBlockedEntryFrameControl` must be built and HotSpot measured. `i133-L8` shape 2 (nested-loop entry frame) is still `OPAQUE_FRAME`. |
| `i124-L9-compiled-home-write-matches-a-stale-take` | Debug-feature unit tests plus the JDI probe `L2W124JdiCompiledCallerSetValues`. |
| `i126-L1`, `i130-L1b-agentlib-includevirtualthreads-dropped` | Lane R2 measured SAME. Retire once their unit tests pass. |
| `i127-L1-compiled-exception-event-object-collected` | JDWP run with `CRATONVM_JDWP_EVENT_OBJECT_HOLD`. |
| `i127-L4-varhandle-float-get-and-set-returns-zero` | Probes SAME on w133 (`L4W127FloatAtomics`, `L4W123RealVarForm` all arms). Its two unit tests must pass. |
| `i107-L2` (aarch64) | Needs an aarch64 host. |

## Open here: performance

`i110-L4` (`invokeWithArguments` body), `i125-L6`, `i130-L1b-get-thread-state-cost`, `i133-L2`
(`getAnnotations` 16-19x `-Xint`), and from lane R2's runs `i134-R2-compatible-weakreference-refersto-is-not-compiled`
and `i134-R2-record-layout-lookup-is-35x-hotspot`. Two proposals: `i114-L6`, `i116-L1`. Two gaps:
`gap-hidden-walks-...` and `gap-hybrid-cpu-...`.

## Open proposals

45 pages under `docs/internal/proposals/interpreter/`. Most wait on a host census or an A/B that
this Windows host cannot give: app suites, Spring Boot, Tomcat, Quarkus and Groovy. Their last
`## Wave` section names the run.

## For the Linux triage

- **Watch the four WARNING flips first.** For a hang, try `CRATONVM_MONITOREXIT_FRAME_STRICT=0`.
  For a new `loader constraint violation`, try `CRATONVM_DEFINE_BUILTIN_CONSTRAINT_REFUSAL=0`.
- **Censuses still owed:**
  - i23-L7: the leniency census and strict pass counts;
  - i46-L5: the flat-store census;
  - i57-L4: `CRATONVM_DBG_FIELD_SITE=1` over the suite, Spring Boot and Netty;
  - i58-L5: the `JdkJarFileBodies` `--compatible` census;
  - i124-L5: the child-first loader smoke (Tomcat or the fat jar), with `CRATONVM_DBG=access`.
- **Test suites:**
  - The vm library tests ran past the 45-minute limit on this host (E-cores) in w131 and w133.
  - Two tests failed in both builds: `memory::addr_keyed::tests::gcg_k2_raw_address_keyed_declarations_are_classified_or_counted`
    and `debug::tests::gci_i16sn_the_native_transition_hold_follows_the_suspension` (debug feature).
  - The merged-build section below says what the end-of-round build showed.

## Merged build (end of round)

Filled in at merge time; see the section appended below.
