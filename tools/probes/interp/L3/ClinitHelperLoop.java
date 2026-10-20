// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 4, lane L3: the class-initialization barrier on
// `invokestatic` sites (docs/internal/fixed-bugs/
// interpreter-L3-proposal-static-site-clinit-barrier-FIXED-20260923.md).
//
// A class whose <clinit> fills a table through its own static helper used to
// pay the uncached slow path (~1 us) on every helper call, because a site
// filled while its holder is initializing may not be cached. The barrier
// serves those calls from a per-thread memo that re-checks "still being
// initialized by this thread" on every hit.
//
// Run with --nojit (and the default), diff stdout against HotSpot 25, which
// prints exactly:
//
//   table checksum -1181483008
//   table[12345]=381152
//   fail EIIE java.lang.IllegalStateException: boom after 50000
//   fail NCDFE Could not initialize class ClinitHelperLoop$FailingTable
//   fail NCDFE Could not initialize class ClinitHelperLoop$FailingTable
//
// Timing goes to stderr: "[clinit] fill ms=..." is the <clinit> duration of
// the 2^18-iteration fill. Compare the build before and after the barrier
// (interleaved runs, medians), and CratonVM against HotSpot.
public class ClinitHelperLoop {
    static final int N = 1 << 18;

    static class Table {
        static final int[] T = new int[N];
        static long fillNanos;

        static {
            long t0 = System.nanoTime();
            for (int i = 0; i < N; i++) {
                T[i] = mix(i);
            }
            fillNanos = System.nanoTime() - t0;
        }

        static int mix(int i) {
            return i * 31 ^ (i >>> 3);
        }
    }

    static class FailingTable {
        static int calls;

        static {
            for (int i = 0; i < 50000; i++) {
                bump();
            }
            if (calls == 50000) {
                throw new IllegalStateException("boom after " + calls);
            }
        }

        static void bump() {
            calls++;
        }
    }

    public static void main(String[] args) {
        int sum = 0;
        for (int v : Table.T) {
            sum = sum * 7 + v;
        }
        System.out.println("table checksum " + sum);
        System.out.println("table[12345]=" + Table.T[12345]);
        System.err.println("[clinit] fill ms=" + (Table.fillNanos / 1_000_000.0));

        try {
            FailingTable.bump();
        } catch (ExceptionInInitializerError e) {
            System.out.println("fail EIIE " + e.getCause());
        }
        for (int i = 0; i < 2; i++) {
            try {
                FailingTable.bump();
                System.out.println("fail bump ran after a failed <clinit>");
            } catch (NoClassDefFoundError e) {
                System.out.println("fail NCDFE " + e.getMessage());
            }
        }
    }
}
