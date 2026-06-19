# Cluster — NullPointerException (62 CratonVM-only FAILs, OPEN)

| | |
|---|---|
| **Kind** | FAIL (assertion/exception), NPE-dominant |
| **CratonVM** | FAIL · **HotSpot** OK |
| **Status** | OPEN — heterogeneous; needs a per-class trace pass |

The single largest CratonVM-only FAIL bucket: 62 classes whose dominant failure is a
`java.lang.NullPointerException`. **Not a single root cause** — there is no shared faulting
frame across them. Likely a mix of:

- reflection / field gaps (a native returning `null` where the JDK returns a value),
- unmodelled lazy-init fields on synthetic JDK classes (read back null),
- synthetic-class field-layout holes.

## Why it's not yet sub-clustered

The sweep's launcher (`KRun`) originally captured only RESULT **counts**, not stack traces, so
the NPE origin frames were unavailable for grouping. KRun has since been **enhanced** to emit
full per-failure stack traces + cause chains (`KRUN-FAILURE` markers + `printStackTrace`).

Some of these 62 may already be resolved by the dev stacktrace fixes that landed after the
sweep (`backfill StackTraceElement.of`, `null-safe StackTraceElement formatting`) — a re-run
on latest dev is needed to confirm.

## Next steps

Re-run the 62 on **latest dev** with the enhanced KRun (idle box), group by the first
app/JDK frame of each NPE, and split into concrete sub-bugs. Expect a handful of shared
root causes once grouped.

(The class list was preserved during analysis; regenerate from the sweep `joined` map:
classes where `HS=OK` and `CV=FAIL` with an NPE-dominant signature.)
