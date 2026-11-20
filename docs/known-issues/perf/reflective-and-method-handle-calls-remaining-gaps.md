# Reflective and method-handle calls: remaining gaps to HotSpot

**Status: OPEN** (2026-11-18). What is left after the rounds recorded in
[`../jit/reflective-calls-trail-hotspot-by-microseconds.md`](../jit/reflective-calls-trail-hotspot-by-microseconds.md),
which holds the history and the fixes. This page scopes the remaining work: per row, the current
gap, the measured cause, and what a fix needs.

## Where the rows stand

ProxySplit rows and MiProbe, `--real-jdk`, last 3 reps of 2000, ns/op, on dev `5ea77ed97b`.
HotSpot 25 from the earlier rounds on the same host:

| Row | What it calls | CratonVM G1 | CratonVM ZGC | HotSpot 25 |
| --- | --- | --- | --- | --- |
| `invoke` | `static final Method.invoke` on a lambda | 9.2-9.6 | 8.5-9.4 | 6.8-7.5 |
| MiProbe | `static final Method.invoke` on a class instance | 10.1-10.3 | 9.8-10.0 | 2.9-3.1 |
| `invcls` | `Method.invoke` on a class receiver | 10.1-10.4 | 10.3-10.5 | n/m |
| `invacc` | `invoke` after `setAccessible(true)` | 5.2-5.6 | 5.3-5.5 | n/m |
| `mhc` | `static final` `asType`'d handle, `invokeExact` | 3.3-4.6 | 2.8-5.1 | 0.3 |
| `mhv` | non-constant handle in a local | 5.8-6.6 | 5.8-6.0 | 15-18 |
| `mhvs` | non-final static handle read in a spliced helper | 2.4-3.0 | 2.5-5.2 | 15 |
| `prefl` | JDK `Proxy` whose handler calls `Method.invoke` | 23.3-24.8 | 26.1-26.4 | 3.1 |

`mhv` and `mhvs` are at or below HotSpot. The host is shared: compare rows interleaved against a
base build, never across days.

## Remaining work

### 1. Class receivers do not get the folded member splice (MiProbe, `invcls`)

The lambda case is spliced: the nested resolver plans the folded handle's member at the refused
`invokeExact` inside `DirectMethodHandleAccessor.invokeImpl` (`folded_handle_member_row`), and the
builder splices it through the fold. For a class receiver the receiver class now arrives (`347`,
`MiProbe$Plain`), but the trace shows
`nest-mh-member invokeExact(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object; depth=3 -> refused: none of 1 fold rows resolves for class 347`:
the one candidate member does not resolve for that class.

Next: print the candidate member's class, name and descriptor and the
`inline-resolve REFUSED` reason for it; likely a declaring-class or descriptor mismatch between
the fold row (`ConstMhFold::info_ptr`, possibly the erased `DirectMethodHandle` form) and
`Plain.price(I)I`. Expected gain: the target call (about 14 % of `invoke` before the lambda
splice), so MiProbe from about 10 to about 8.5 ns.

### 2. The `invoke` caller body (9.3 ns vs 7)

With the target spliced, a named profile puts 93.8 % in `ProxySplit.invoke` itself. Earlier
instruction-level samples sat on constant-root reloads (`IndyRootRef` loads and the field loads that
depend on them), reloaded because a collector may move the object at any safepoint, and on stack
home stores and slot clears.

Next: reload a constant root once per safepoint-free stretch instead of at every use, and keep the
loop's values in registers instead of stack homes. Both are register-allocation and GC-root work in
`ir_lower`; measure by instruction counts, not wall time.

### 3. Boxes around a folded converting handle (`mhc` 3-5 ns vs 0.3, part of `prefl`)

HotSpot removes the argument and result boxes after inlining. Here `Integer.valueOf` is spliced,
not a call, and the converting site's guards keep the box live in their deopt snapshots, so the
box/unbox fold (`ea_ir_bridge`) cannot apply.

Next: rematerialize such a box at deopt (an auto-box deopt recipe already exists for a lone box
named only by snapshots), then let the fold drop the spliced `valueOf`.

### 4. The proxy row (`prefl` 23-26 ns vs 3.1)

The handler's `Method.invoke` now splices its target (the same route as `invoke`). The rest is the
`Proxy` chain: the handler call, its argument array and the boxes of item 3.

Next: a named profile of `prefl` on dev to split the remaining time between the handler body, the
varargs array (escape analysis across the spliced `Method.invoke`) and the boxes.

### 5. Run-to-run variance

- `prefl` on G1 still lands at about 1.4 us in roughly 1 run in 8 (the guarded-splice evidence bar
  was one cause, fixed; the run that stays slow never touches the converting-handle path). Trace
  the first optimizing compile of a slow run (`ir-profile-guard-miss obs=`).
- The `R21ReflectInvoke` `null-receiver` rows give `NullPointerException: null` or the helpful
  message depending on whether the fast-throw profile has recorded the site yet; dev itself flips
  between 0 and 4 differing lines from HotSpot across runs. Make the probe deterministic, or the
  fast-throw site naming stable, before using those rows as a gate.

### 6. Test-suite crash

`ir_lower::tests::part_01::r9w8_an_ldc_with_a_constant_slot_reads_it_before_calling` kills the
`cratonvm-jit --lib` test process on dev, with or without the reflective-call changes, so a plain
`cargo test` run stops about 1,900 tests in. Run the suite with that test skipped (and the crash-skip
loop) until it is fixed.

## How to measure

- Probes: `ProxySplit <row> 2000 20000` (last three `row` lines) and `MiProbe 3000000`.
- Interleave base and change per round, G1 and ZGC, three rounds; quote the quiet rounds.
- Names in `perf`: a throwaway build with `CRATONVM_JIT_PERF_MAP` and the trace switches read from
  the environment (they are hard-wired off on dev).
- Parity: every probe's non-timing output against HotSpot 25 in `--real-jdk` and `--compatible`,
  repeated three times on base and change (see item 5).
