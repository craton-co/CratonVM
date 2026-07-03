# Tomcat DoHead — rare main-vm array-helper boundary read (SIGSEGV, distinct from the GC-corruption family)

Status: **OPEN, rare (1/18 runs).** Observed once during the combined-binary
validation of the DoHead GC fixes (2026-07-02, binary = dev@`11ca8abc` +
`fix/dohead-sweep-freelist`, run `dhswcomb-7`). NOT the walk-desync /
register-invisible-root family: zero GC warnings of any kind in the log, and
the face reproduced on neither 12 pure-dev (`11ca8abc`) runs nor 18 runs of
the Layer-2-only binary nor 18 unfixed-baseline runs (whose one crash was the
classic garbage-base+0x18 worker-thread corruption face).

## Signature

```
EXCEPTION_ACCESS_VIOLATION (SIGSEGV) at pc RVA exe+0x20214C  (native helper, called from JIT)
Faulting access: read at address 0x000000003E960000
thread: "main-vm"
rdi=0x3AE0D9B0  r14=0x000000000076A4C5  r13=0x45B  rbx=0x3AE0D928
```

Decode: fault address == `rdi + r14*8 + 0x28` **exactly** — an object-array
element read (`base + index*8 + HEADER_SIZE`) inside a native helper, with
index r14 = 7,775,429 (absurd; r13 = 1,115 nearby suggests the real bound).
The read runs off the end of a mapped region (fault at the round boundary
0x3E960000). Crash happened at sub-test 44 **startup** ("Starting
ProtocolHandler http-nio-...auto-90"), not during stop-churn.

Repro/logs: `apps/tomcat/.suite/results/dhswcomb-7/real-jit/*.log.err`
(same `-Xmx500m`/150 s idx-38 harness as the GC-corruption repro).
Symbolization of the release PDB is line-tables-only (nearest-symbol garbage);
a `profsym` build is needed to name exe+0x20214C / the JIT return sites
(code bytes of three JIT frames are in the dump).

## Hypotheses (unverified)

- A JIT-called array/arraycopy-style helper fed a runaway index or a corrupt
  array length (cf. `reference_jit_arraycopy_spill_aliasing`,
  `reference_jit_lentable_regalloc_fastmath` families).
- Incidence too low (1/48 across all hardened+devpure runs that day) for
  black-box A/B; needs the symbolized helper name first.

## Next steps

1. Rebuild with `[profile.release] strip="none"`/`debug="line-tables-only"`
   (or profsym) and symbolize exe+0x20214C, exe+0x1F394A, exe+0xBCBAB9.
2. Once the helper is named, audit its index/length inputs for the
   JIT-miscompile families above; check whether the 2026-07-02 dev drift
   (types/value.rs, compact_value.rs, jit/tiered.rs) touched its operands.
