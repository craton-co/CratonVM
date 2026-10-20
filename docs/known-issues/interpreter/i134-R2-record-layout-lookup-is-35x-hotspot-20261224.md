# The record-layout lookup costs about 4 us under the JIT, about 35x HotSpot

**Status: OPEN (performance; measured, cause not located).** Filed 2026-10-19 by interpreter round
i6 wave 134 from lane R2's rerun of the `CRATONVM_RECORD_LAYOUT_TABLE` A/B (w131 `07ed7469ce`,
Windows, P-cores, medians of 5 interleaved reps). Not built.

## Symptom

The record-layout bench used for the `RECORD_LAYOUT_TABLE` A/B prints about 7136 ns per lookup in
its first rows and settles near 4 us per lookup under the JIT, against roughly 115 ns for HotSpot
(the bench header's figure). `CRATONVM_RECORD_LAYOUT_TABLE` itself is flat (0.94-1.07), so it stays
KEPT-OFF; the gap is elsewhere.

## Correction to the w124 numbers

The w124 rows that read "4 vs 7 ns" were a parse error: the bench prints a decimal comma
(`7136,0 ns/lookup`) and the old A/B script read only the digits before it. Lane R2's
`ab3.py` reads the comma. Treat every earlier sub-microsecond figure for this bench as wrong.

## What would close it

A RIP sample (the Windows sampler recipe) of the bench's lookup loop under the JIT in both modes,
then a page per hot frame. Pass: within 3x of HotSpot, behaviour lines unchanged.
