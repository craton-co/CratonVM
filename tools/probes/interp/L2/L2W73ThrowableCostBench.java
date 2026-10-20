// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 73, lane L2 -- what building, throwing and reading
// a java.lang.Throwable costs, and what a class-loader delegation miss pays for
// its ClassNotFoundException. Bench for
// docs/internal/fixed-bugs/interpreter-i73-L2-proposal-memoise-the-delegation-miss-capture-KEPT-OFF-20261106.md.
//
// Switch (opt-in, default OFF; the same trace either way):
//   CRATONVM_THROWABLE_LOADER_MISS_MEMO=1  a delegation miss's capture takes
//       the capture memo: under --jdk-only the loader-middle pairs the decoded
//       rule proves quiet no longer refuse it; under --compatible a served
//       loader's miss hint is keyed instead of refused.
//
// Rows (ITERS operations each, one untimed warm-up pass, then REPS timed
// passes; the MINIMUM is printed):
//   new-shallow        new Exception() three frames below main
//   new-deep50         new Exception() 50 frames deeper
//   throw-catch-same   throw new Exception() caught in the same method
//   throw-catch-5      thrown five frames below the catching method
//   cnfe-platform      ClassLoader.getPlatformClassLoader().loadClass(missing):
//                      one ClassNotFoundException (BuiltinClassLoader.loadClass)
//   cnfe-url           new URLClassLoader(no URLs, platform).loadClass(missing):
//                      the parent's miss (built, caught in ClassLoader.loadClass)
//                      and the loader's own (URLClassLoader.findClass)
//   get-stack-trace    new Exception().getStackTrace()
// The loaders cycle through 64 missing names, so their parallel-lock maps stop
// growing after the warm-up (HotSpot pays the same map).
//
// stdout: per row one deterministic "<row> checksum=<n>" line (no "ns/") and
// one timing line "<row> <x> ns/op (min of REPS)". One row only:
// `L2W73ThrowableCostBench <row>`.
//
// A/B: the SAME binary, interleaved, several rounds, medians of the per-round
// minima, --jdk-only (default) and --compatible, JIT and --nojit:
//   $B --java-home $J/.. -cp out L2W73ThrowableCostBench                                          # (a) off
//   env CRATONVM_THROWABLE_LOADER_MISS_MEMO=1 $B --java-home $J/.. -cp out L2W73ThrowableCostBench # (b) on
//   (each also with --compatible, and with --nojit)
//   $J/java -cp out L2W73ThrowableCostBench                                                        # HotSpot
// Expected: (b) under (a) on cnfe-platform and cnfe-url in both modes; the
// other rows are the controls and must not move. Positive control:
// CRATONVM_DBG_CAPTURE_MEMO=1 prints "[capture-memo] hits=.. misses=.. made=..
// refused=.." on stderr; with (b) the cnfe rows' captures should be hits.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// new-shallow checksum=20000
// new-deep50 checksum=20000
// throw-catch-same checksum=20000
// throw-catch-5 checksum=20000
// cnfe-platform checksum=20000
// cnfe-url checksum=5000
// get-stack-trace checksum=20000
// HOTSPOT_EXPECTED_END
import java.net.URL;
import java.net.URLClassLoader;

public class L2W73ThrowableCostBench {
    static final int ITERS = 20_000;
    static final int URL_ITERS = 5_000;
    static final int REPS = 5;
    static final String[] NAMES = new String[64];
    static ClassLoader platform;
    static URLClassLoader url;
    static Object sink;

    static final int NEW_SHALLOW = 0;
    static final int NEW_DEEP = 1;
    static final int THROW_SAME = 2;
    static final int THROW_FIVE = 3;
    static final int CNFE_PLATFORM = 4;
    static final int CNFE_URL = 5;
    static final int GET_STACK_TRACE = 6;

    static long newShallow(int n) {
        long c = 0;
        for (int i = 0; i < n; i++) {
            Exception e = new Exception();
            sink = e;
            c++;
        }
        return c;
    }

    static long atDepth(int depth, int n) {
        if (depth == 0) {
            return newShallow(n);
        }
        return atDepth(depth - 1, n);
    }

    static long throwSame(int n) {
        long c = 0;
        for (int i = 0; i < n; i++) {
            try {
                throw new Exception();
            } catch (Exception e) {
                sink = e;
                c++;
            }
        }
        return c;
    }

    static void t0(int i) throws Exception {
        throw new Exception();
    }

    static void t1(int i) throws Exception {
        t0(i);
    }

    static void t2(int i) throws Exception {
        t1(i);
    }

    static void t3(int i) throws Exception {
        t2(i);
    }

    static void t4(int i) throws Exception {
        t3(i);
    }

    static long throwFive(int n) {
        long c = 0;
        for (int i = 0; i < n; i++) {
            try {
                t4(i);
            } catch (Exception e) {
                sink = e;
                c++;
            }
        }
        return c;
    }

    static long cnfe(ClassLoader loader, int n) {
        long c = 0;
        for (int i = 0; i < n; i++) {
            try {
                sink = loader.loadClass(NAMES[i & 63]);
            } catch (ClassNotFoundException e) {
                sink = e;
                c++;
            }
        }
        return c;
    }

    static long getStackTrace(int n) {
        long c = 0;
        for (int i = 0; i < n; i++) {
            StackTraceElement[] st = new Exception().getStackTrace();
            if (st.length > 0 && st[0].getMethodName().equals("getStackTrace")) {
                c++;
            }
        }
        return c;
    }

    static long run(int row, int n) {
        switch (row) {
            case NEW_SHALLOW:
                return newShallow(n);
            case NEW_DEEP:
                return atDepth(50, n);
            case THROW_SAME:
                return throwSame(n);
            case THROW_FIVE:
                return throwFive(n);
            case CNFE_PLATFORM:
                return cnfe(platform, n);
            case CNFE_URL:
                return cnfe(url, n);
            default:
                return getStackTrace(n);
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

    public static void main(String[] args) throws Exception {
        for (int i = 0; i < NAMES.length; i++) {
            NAMES[i] = "l2w73.missing.Absent" + i;
        }
        platform = ClassLoader.getPlatformClassLoader();
        url = new URLClassLoader(new URL[0], platform);
        String only = args.length > 0 ? args[0] : null;
        String[] rows = {
            "new-shallow", "new-deep50", "throw-catch-same", "throw-catch-5",
            "cnfe-platform", "cnfe-url", "get-stack-trace",
        };
        for (int row = 0; row < rows.length; row++) {
            if (only != null && !only.equals(rows[row])) {
                continue;
            }
            bench(rows[row], row, row == CNFE_URL ? URL_ITERS : ITERS);
        }
    }
}
