// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 82, lane L2 -- what a Throwable whose trace is
// never read costs, against one whose trace is read, at two depths, and the
// "quiet exception" idiom (a fillInStackTrace() override that captures
// nothing). Bench for
// docs/internal/fixed-bugs/interpreter-i73-L2-proposal-backtrace-owned-by-the-throwable-KEPT-OFF-20261115.md.
//
// Switch (opt-in, default OFF; the same traces either way):
//   CRATONVM_THROWABLE_BACKTRACE_IN_OBJECT=1  a capture's frames go into a
//       long[] holder stored in the throwable's own `backtrace` field (frame
//       ids into a per-VM table) instead of the VM's address-keyed registry;
//       readers decode the holder. No registry write lock per capture, no
//       registry probe per read, and a dead throwable's frames die with it
//       instead of in the next collection's registry sweep.
//
// Rows (ITERS operations each, one untimed warm-up pass, then REPS timed
// passes; the MINIMUM is printed):
//   new-exception-unused          new Exception(), dropped (two frames below main)
//   new-exception-getStackTrace   new Exception().getStackTrace()
//   deep-50-unused                new Exception() 50 frames deeper, dropped
//   deep-50-getStackTrace         the same, then getStackTrace()
//   fillInStackTrace-override-noop  new Quiet() whose fillInStackTrace()
//                                 returns `this` (no capture: the control)
//
// stdout: per row one deterministic "<row> checksum=<n>" line (no "ns/") and
// one timing line "<row> <x> ns/op (min of REPS)". One row only:
// `L2W82ThrowableCostBench <row>`.
//
// A/B: the SAME binary, interleaved, >= 3 rounds, medians of the per-round
// minima, --jdk-only (default) and --compatible, JIT and --nojit:
//   $B --java-home $J/.. -cp out L2W82ThrowableCostBench                                              # (a) off
//   env CRATONVM_THROWABLE_BACKTRACE_IN_OBJECT=1 $B --java-home $J/.. -cp out L2W82ThrowableCostBench # (b) on
//   (each also with --compatible, and with --nojit)
//   $J/java -cp out L2W82ThrowableCostBench                                                           # HotSpot
// Expected: (b) under (a) on the two *-unused rows (no registry store);
// the *-getStackTrace rows flat or slightly under (one field read and a
// decode instead of a shard probe); fillInStackTrace-override-noop is the
// control and must not move. GC side: CRATONVM_GC_STATS=1 --verbose:gc
// (before the mode flag) on `L2W82ThrowableCostBench deep-50-unused`: the
// pauses should not carry the registry sweep with (b).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// new-exception-unused checksum=20000
// new-exception-getStackTrace checksum=20000
// deep-50-unused checksum=20000
// deep-50-getStackTrace checksum=20000
// fillInStackTrace-override-noop checksum=20000
// HOTSPOT_EXPECTED_END

public class L2W82ThrowableCostBench {
    static final int ITERS = 20_000;
    static final int REPS = 5;
    static Object sink;

    static final int NEW_UNUSED = 0;
    static final int NEW_READ = 1;
    static final int DEEP_UNUSED = 2;
    static final int DEEP_READ = 3;
    static final int QUIET = 4;

    static final class Quiet extends Exception {
        @Override
        public synchronized Throwable fillInStackTrace() {
            return this;
        }
    }

    static long newUnused(int n) {
        long c = 0;
        for (int i = 0; i < n; i++) {
            Exception e = new Exception();
            sink = e;
            c++;
        }
        return c;
    }

    static long newRead(int n, int expectedMin) {
        long c = 0;
        for (int i = 0; i < n; i++) {
            StackTraceElement[] st = new Exception().getStackTrace();
            if (st.length >= expectedMin && st[0].getMethodName().equals("newRead")) {
                c++;
            }
        }
        return c;
    }

    static long quiet(int n) {
        long c = 0;
        for (int i = 0; i < n; i++) {
            Quiet q = new Quiet();
            sink = q;
            if (q.getStackTrace().length == 0) {
                c++;
            }
        }
        return c;
    }

    static long atDepth(int depth, boolean read, int n) {
        if (depth == 0) {
            return read ? newRead(n, 50) : newUnused(n);
        }
        return atDepth(depth - 1, read, n);
    }

    static long run(int row, int n) {
        switch (row) {
            case NEW_UNUSED:
                return newUnused(n);
            case NEW_READ:
                return newRead(n, 1);
            case DEEP_UNUSED:
                return atDepth(50, false, n);
            case DEEP_READ:
                return atDepth(50, true, n);
            default:
                return quiet(n);
        }
    }

    static void bench(String name, int row, int n) {
        long checksum = run(row, n);
        long best = Long.MAX_VALUE;
        for (int rep = 0; rep < REPS; rep++) {
            long t0 = System.nanoTime();
            long got = run(row, n);
            long t1 = System.nanoTime();
            best = Math.min(best, t1 - t0);
            if (got != checksum) {
                System.out.println(name + " unstable " + got + " vs " + checksum);
            }
        }
        System.out.println(name + " checksum=" + checksum);
        System.out.printf("%s %.1f ns/op (min of %d)%n", name, (double) best / n, REPS);
    }

    public static void main(String[] args) {
        String[] names = {
            "new-exception-unused",
            "new-exception-getStackTrace",
            "deep-50-unused",
            "deep-50-getStackTrace",
            "fillInStackTrace-override-noop",
        };
        String only = args.length > 0 ? args[0] : null;
        for (int row = 0; row < names.length; row++) {
            if (only == null || only.equals(names[row])) {
                bench(names[row], row, ITERS);
            }
        }
    }
}
