# `R26SegLongLoops` never finishes under `--compatible` on dev `c386bad829`

Status: OPEN. Area: `--compatible` boot / FFM segment path (not the JIT). Severity: high for the probe (no output,
killed at the timeout), unknown for users. Found 2026-10-29 by round rx27 while landing `jit-regex-r27` on dev.

## What is true

`tools/probes` `R26SegLongLoops` (the battery's FFM long-loop probe), Linux host 64.177.43.150, 150 s timeout:

| binary | tree | `--compatible` | `--jdk-only` |
|---|---|---|---|
| `cvm-rx27b19` | `jit-regex-r27` `fac6921ae7` (dev `df86fe90ba` + round rx27, before the dev merge) | 2.97-3.24 s, rc 0 (6/6 runs) | equal to HotSpot |
| `cvm-devtip` | dev `c386bad829` alone | **timeout, rc 124** | - |
| `cvm-rx27b23` | `jit-regex-r27` after merging dev `c386bad829` | **timeout, rc 124** (3/3) | equal to HotSpot (battery) |
| `cvm-rx27b23` with every rx27 default switch `=0` | same | **timeout** (2/2) | - |

So the hang came in with dev's commits between `df86fe90ba` and `c386bad829` (143 commits; the `--compatible`
boot was reworked there: the fast-boot revert `75dddd1b62` / `f285700b7c` / `862b40a67f`, `fec940c8e2`,
`b480d30ec6`, `b7fbf4eaa2`, the Phase 5 cold-start merge `66cccebe1b` and the native-registration merge
`70d9dc4950`), not with round rx27's JIT switches.

## How to reproduce

```
cvm --compatible -cp <bat cls dir of R26SegLongLoops> R26SegLongLoops   # hangs; --jdk-only finishes
```

Next step: bisect `df86fe90ba..c386bad829` on `--compatible` with this probe (a `git bisect run` with a 20 s
timeout), then a thread dump (`kill -3`) of the hung process to name the spinning or parked thread.
