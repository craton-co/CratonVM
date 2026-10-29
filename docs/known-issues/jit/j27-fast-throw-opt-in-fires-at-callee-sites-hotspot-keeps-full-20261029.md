> **j27 wave 9 (lane W9RPL, 2026-10-29): NOT FIXED (opt-in arm only; the default arm is unaffected).
> Reproduced, and a more faithful rule designed, but the change is not small. The switch stays opt-in.**
>
> **Reproduced.** Binary `cvm-rpl-1`. `J27MiscRows -XX:+OmitStackTraceInFastThrow` vs default `java`:
>
> | row | HotSpot | `--jdk-only` | `--compatible` |
> |---|---|---|---|
> | arraylist-init | 0 | 2143 | 737 |
> | linkedlist-addall | 684 | 332 | 0 |
> | linkedlist-addall-at | 0 | 2999 | 1870 |
> | toarray rows | 0 | 5 | 5 |
> | arraydeque / hashset / treemap / containsall | 0 | 1948-2308 | 470-852 |
>
> **Why the suggested model would not close it.** The "What would fix it" model gates a site with no local
> handler on `PerMethodTrapLimit` (100). Each row throws 3 000 times, so that site would still turn
> stackless after about 100 compiled throws. HotSpot keeps these rows at `bad=0` for a different reason:
> - C2 reads the trap bits when it PARSES the method (`too_many_traps` -> `has_trap_at`).
> - `ArrayList.<init>(Collection)` and its siblings were compiled during boot, before any trap was
>   recorded. So the first null takes an uncommon trap (`Action_make_not_entrant`), and the site turns hot
>   only after a RECOMPILE. A recompile does not happen within 3 000 throws.
> - The emulation reads the profile at the THROW. Our interpreter records the early interpreted throws, so
>   the first compiled throw already finds a trap and goes stackless.
>
> **A faithful rule (not built).** A site is hot iff its body is C2-equivalent AND a trap of that kind was
> recorded at (method, bci) BEFORE that body was compiled. Use the body's `CompiledMethod::artifact_id`
> (monotonic) against a per-trap mark of the artifact counter, taken when `FastThrowTrapProfile::note`
> runs.
> - `J27T1StrTrapRows` stays fast: its interpreter throws precede the compile, which is HotSpot's order.
> - The rows HotSpot keeps full stay full until a body is recompiled.
>
> **What the build needs (why it is not small):**
> 1. A public mark of `jit/src/lib.rs`'s `next_artifact_id` counter.
> 2. A trap mark stored in both `FastThrowTrapProfile` maps.
> 3. The throwing body's `artifact_id` carried to `FastThrowSite`. That means a new field in
>    `ActiveCompiledFrame` (`vm/src/jit/conservative_roots.rs`, built by struct literal in 7 places across
>    6 files) or in `fast_throw_c2_tier`'s producer, plus the interpreter-resume path
>    (`take_trap_site_if` already returns the body).
> 4. A recompile model. Without one, a body compiled before the trap stays full until the 10 000-throw
>    count fallback, whereas HotSpot's `linkedlist-addall` goes stackless after about 600 throws. HotSpot's
>    counts are timing-dependent.
>
> First check, before building: is `ArrayList.<init>(Ljava/util/Collection;)V`'s body compiled before the
> probe's first null? `CRATONVM_DBG_JITC=1` prints its compile line; compare its position with the first
> `null-arraylist-init` round.

# The opt-in `OmitStackTraceInFastThrow` emulation goes stackless at JDK callee sites HotSpot keeps full

**Status:** OPEN (found by j27 wave 9, lane W9EXC, builder, Windows, 2026-10-29). Affects only the opt-in arm
(`-XX:+OmitStackTraceInFastThrow` or `CRATONVM_JIT=omit-stack-trace-in-fast-throw`; default OFF by the owner's
decision); the default arm equals `java -XX:-OmitStackTraceInFastThrow` on every probe below.
**Area:** `vm/src/runtime/exceptions.rs` hotness rule (`try_fast_throw`, `fast_throw_profile_hot`,
`body_is_c2_equivalent`, count fallback `FAST_THROW_HOT_THRESHOLD`).

## Evidence (measured, Windows, HotSpot 25.0.3 vs `cvm-exc-1` = tree 47479dc6e0 + wave-9 WIP, switch on)

`tools/probes/jit/j27/J27MiscRows.java`: each null-argument row calls a JDK collection constructor / bulk
method with a null argument 3 000 times, catching in the probe's own `npeMessage`; `bad` counts rounds whose
message differs from round 0 (a stackless fast throw has message `null`).

| row | HotSpot default `java` | cratonvm `--jdk-only` | cratonvm `--compatible` |
|---|---|---|---|
| null-arraylist-init | bad=0 | bad=2153 | bad=763 |
| null-arraylist-addall | bad=0 | bad=2291 | bad=788 |
| null-vector-addall | bad=0 | bad=2093 | bad=628 |
| null-linkedlist-addall | bad=619 / 671 / 556 (3 runs) | bad=318 | bad=0 |
| null-linkedlist-addall-at | bad=0, msg `null` | bad=2999 | bad=1882 |
| null-linkedlist-toarray / null-arraylist-toarray | bad=0 | bad=5 / 5 | bad=5 / 5 |
| null-arraydeque-addall, null-hashset-init, null-linkedhashset-init, null-treemap-init-sorted, null-hashset-containsall | bad=0 | bad=1925..2285 | bad=437..821 |

HotSpot keeps nearly all of these throws full: the throwing bci is in a JDK method with no handler of its own
(`GraphKit::builtin_throw` then needs `too_many_traps(reason)` for that method, and the throws its C1 tier-3
code makes record no trap in the MDO), so C2 does not reach the fast throw within 3 000 calls. The emulation
makes the site hot after its first compiled throw (profile rule) whatever the method's handlers. The
`toarray` rows' `bad=5` also shows a short burst of fast throws that stops again.

`J27T1StrTrapRows` (throw caught in the throwing method itself) is the shape the rule models well: SAME as
HotSpot in both modes with the switch on.

## What would fix it

Model `builtin_throw`'s two conditions per site: (1) the method containing the bci HAS a handler covering it and
has trapped there (fast after the first recorded trap, as now), or (2) no local handler: fast only once the
method's trap count for that reason passes HotSpot's `PerMethodTrapLimit` (100) AND a trap was recorded at the
bci by the interpreter or by a C2-equivalent body (not by a single-pass body below the tier-4 count, which is
C1 and records nothing). Then rerun `J27MiscRows` and `J27T1StrTrapRows` with the switch on (both modes; the
HotSpot `bad=` counts are timing-dependent, so expect the rows HotSpot keeps at 0 to be 0 and the
`linkedlist-addall` row to be non-zero), and `J27DeoptRows` / `J27RematRows` (rows that HotSpot itself varies on).
