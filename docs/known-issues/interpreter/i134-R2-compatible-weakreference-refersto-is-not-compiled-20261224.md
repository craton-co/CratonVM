# `--compatible`: `WeakReference.refersTo` is never compiled, so `weak-new` costs 12x `--jdk-only`

**Status: OPEN (performance; found by a run, cause not located).** Filed 2026-10-19 by interpreter
round i6 wave 134 from lane R2's run of `i77-L5` (w131 `07ed7469ce`, Windows, P-cores, medians of 5).
Not built.

## Symptom

`tools/probes/interp/L5/L5W132RefNotesThreadsBench.java`, row `weak-new`: about 308 ns per op under
`--jdk-only` and about 3769 ns under `--compatible`. Output is SAME as HotSpot in both modes.

## What the run shows

`CRATONVM_DBG=jit-compiled` lists `java/lang/ref/WeakReference.refersTo` and
`Reference.refersToImpl` as compiled only under `--jdk-only`. Every other `Reference` body the bench
reaches is compiled in both modes. So `--compatible` serves `refersTo` off the compiled path,
probably through a registered stand-in or a force-native row in front of the JDK body. The usual
fix for that shape is a HotSpotAlignment row plus a COMPAT_FAMILIES entry, not an allow-list.

## What would close it

1. `CRATONVM_DBG=jit-compiled,tierup-decline` and a `resolve_dispatch` trace for
   `Reference.refersTo0` / `refersToImpl` under `--compatible`, against `--jdk-only`.
2. If a stand-in is in front of the JDK body, a parity row (default ON, `=0`) that lets the JDK body
   run, as for the other compat families. By user decision (2026-10-19) the stand-in itself is not
   deleted.
3. Pass: `weak-new` under `--compatible` within 1.10 of `--jdk-only`; `L5W132RefNotesThreadsBench`
   output SAME in both modes, JIT and `--nojit`.

Logs: `/c/cvt-i6/runs/w130/i77-dbg-*.err`, `/c/cvt-i6/runs/w130/ab/i77_*` (removed with the
round's target dirs; numbers above).
