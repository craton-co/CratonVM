// Lane L2 timing probe (interpreter round i1 wave 22): the one speed risk of
// giving ACC_SYNCHRONIZED optimizing method-entry bodies a back-edge mode exit.
//
// A compiled caller CALLs a `static synchronized` callee directly, holding the
// class monitor itself (the caller-held sync-direct site), only when the
// callee's body has no trap point at all (`jit_bridge::caller_held_body_is_closed`);
// a poll exit is a trap point. Wave 22 withholds the exit from a synchronized
// body whose graph can neither trap nor call (`ir_lower::ir_entry_poll_mode_exits`),
// which keeps `pureLoop` bound. `staticLoop` reads and writes a static field in
// its loop: its graph "can trap" by the allowlist, although its body may emit
// no trap point, so after wave 22 it gets an exit and its compiled callers may
// fall back from the direct CALL to the dispatch helper (the door-locked
// static door still enters the compiled body).
//
// Rows to compare, A/B (the build before this wave vs after), JIT on,
// --compatible, medians of 3 runs, interleaved builds:
//   pureLoop   ns/call   expected: unchanged
//   staticLoop ns/call   expected: unchanged or slightly slower (the price of
//                        the exit; a large regression means the prediction
//                        should be sharpened: see
//                        docs/internal/fixed-bugs/interpreter-L2-ir-entry-exits-refused-for-monitor-methods-FIXED-20260926.md)
// CRATONVM_DBG_JITC=1 shows whether each call site was bound
// (`sync-direct` lines) in either build.
//
// stdout (checksums) must equal HotSpot 25's:
//   pureLoop checksum=17100000
//   staticLoop checksum=1200000 counter=1200000
// Times go to stderr.
public class L2W22SyncLeafLoopBench {
    static int counter;

    static synchronized int pureLoop(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += i;
        }
        return s;
    }

    static synchronized int staticLoop(int n) {
        for (int i = 0; i < n; i++) {
            counter++;
        }
        return counter;
    }

    /// Compiled at method entry after a few rounds; its call site is the one
    /// the caller-held route binds.
    static long drivePure(int calls, int n) {
        long s = 0;
        for (int i = 0; i < calls; i++) {
            s += pureLoop(n) + (i & 1);
        }
        return s;
    }

    static long driveStatic(int calls) {
        long last = 0;
        for (int i = 0; i < calls; i++) {
            last = staticLoop(2);
        }
        return last;
    }

    public static void main(String[] args) {
        final int calls = 20_000;
        final int rounds = 30;
        long pure = 0;
        long stat = 0;
        long[] pureNs = new long[rounds];
        long[] statNs = new long[rounds];
        for (int round = 0; round < rounds; round++) {
            long t0 = System.nanoTime();
            pure += drivePure(calls, 8);
            long t1 = System.nanoTime();
            stat = driveStatic(calls);
            long t2 = System.nanoTime();
            pureNs[round] = t1 - t0;
            statNs[round] = t2 - t1;
        }
        System.out.println("pureLoop checksum=" + pure);
        System.out.println("staticLoop checksum=" + stat + " counter=" + counter);
        // The last ten rounds, after tier-up: their median.
        long[] pureTail = java.util.Arrays.copyOfRange(pureNs, rounds - 10, rounds);
        long[] statTail = java.util.Arrays.copyOfRange(statNs, rounds - 10, rounds);
        java.util.Arrays.sort(pureTail);
        java.util.Arrays.sort(statTail);
        System.err.printf("pureLoop   %.1f ns/call%n", pureTail[5] / (double) calls);
        System.err.printf("staticLoop %.1f ns/call%n", statTail[5] / (double) calls);
    }
}
