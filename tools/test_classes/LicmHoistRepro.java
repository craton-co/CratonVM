// Repro for the LICM aaload-hoist soundness bug: the x86-64 JIT hoists any
// loop-invariant `aload X; iload Y; aaload` (an Object[][]/int[][] row
// pointer) from ANYWHERE in the loop body to the loop preheader and emits it
// as a raw unchecked load — no null check, no bounds check, and the
// preheader runs even when the loop is zero-trip or the in-body access is
// conditionally skipped. So a load the program NEVER performs executes
// anyway, faulting (null/far-OOB) or caching a garbage row pointer.
//
// Shapes (each its own method so it tier-compiles independently):
//   condNull  — body guards with `if (m != null)`; called with m == null.
//               HotSpot: returns 0. Buggy JIT: hoisted aaload derefs null.
//   zeroTrip  — unconditional body, called with n == 0 (loop never runs)
//               and m == null. HotSpot: returns 0. Buggy JIT: null deref.
//   uncondOob — unconditional body, j >= m.length, n > 0. HotSpot: AIOOBE
//               on the first iteration. Buggy JIT: unchecked spine read,
//               garbage row pointer, then garbage iaload — no AIOOBE.
//   condOob   — body guards with `if (j < m.length)`; called with huge j.
//               HotSpot: returns 0. Buggy JIT: far-OOB spine read (may or
//               may not fault depending on heap mapping).
//
// Each probe prints one line: <name> OK <value> | <name> THROWN <class>.
// HotSpot reference: condNull/zeroTrip/condOob print OK 0, uncondOob prints
// THROWN java.lang.ArrayIndexOutOfBoundsException. A VM crash before a
// line prints is the bug.
//
// A/B: CRATONVM_DISABLE_AALOAD_LICM=1 disables the hoist.
public class LicmHoistRepro {
    static long condNull(int[][] m, int j, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            if (m != null) {
                s += m[j][i];
            }
        }
        return s;
    }

    static long zeroTrip(int[][] m, int j, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += m[j][i];
        }
        return s;
    }

    static long uncondOob(int[][] m, int j, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += m[j][i];
        }
        return s;
    }

    static long condOob(int[][] m, int j, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            if (j < m.length) {
                s += m[j][i];
            }
        }
        return s;
    }

    public static void main(String[] args) {
        final int ROWS = 4, COLS = 64;
        final int[][] m = new int[ROWS][COLS];
        for (int r = 0; r < ROWS; r++)
            for (int c = 0; c < COLS; c++) m[r][c] = r + c;

        // Warm every method with valid inputs so each tier-compiles (both
        // call-count tiering and OSR of the inner loop).
        long sink = 0;
        for (int w = 0; w < 60000; w++) {
            sink += condNull(m, 1, 8);
            sink += zeroTrip(m, 2, 8);
            sink += uncondOob(m, 3, 8);
            sink += condOob(m, 1, 8);
        }
        System.out.println("warmed sink=" + sink);

        // Bad-shape probes. Expected (HotSpot):
        //   condNull OK 0 / zeroTrip OK 0 / uncondOob THROWN AIOOBE / condOob OK 0
        try {
            System.out.println("condNull OK " + condNull(null, 1, 8));
        } catch (Throwable t) {
            System.out.println("condNull THROWN " + t.getClass().getName());
        }
        try {
            System.out.println("zeroTrip OK " + zeroTrip(null, 1 << 27, 0));
        } catch (Throwable t) {
            System.out.println("zeroTrip THROWN " + t.getClass().getName());
        }
        try {
            System.out.println("uncondOob OK " + uncondOob(m, ROWS + 3, 8));
        } catch (Throwable t) {
            System.out.println("uncondOob THROWN " + t.getClass().getName());
        }
        try {
            System.out.println("condOob OK " + condOob(m, 1 << 27, 8));
        } catch (Throwable t) {
            System.out.println("condOob THROWN " + t.getClass().getName());
        }
        System.out.println("done");
    }
}
