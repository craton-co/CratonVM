// Lane L2 probe (interpreter round i1 wave 18): optimizing (IR) METHOD-ENTRY
// bodies now give their unconditional back-edge polls a mode exit, so a loop
// running in such a body leaves for the interpreter when an agent needs it
// (`i9-L5-jvmti-frames-already-compiled-finish-compiled`, IR item (c)). The
// exit sits on the poll's slow path only; the flag-clear path is unchanged.
//
// What to compare:
// * stdout must equal HotSpot 25's (one checksum line per shape, then a
//   total), with and without --nojit, under --compatible. No agent is
//   attached, so no exit is ever taken: this checks that the new slow-path
//   bytes and pads change no result.
// * Coverage (no extra setup, Linux or Windows): run with CRATONVM_DBG_JITC=1
//   and read the exit line
//   `[c2-supersede] ir entry poll mode exits: given=N | refused: ...`
//   (method-entry compiles; the OSR door's are on the `ir osr poll mode
//   exits` line). `given` counts back edges an agent could pull back;
//   `synchronizedSum` (ACC_SYNCHRONIZED) sought no exit until wave 21;
//   since wave 22 such a method is admitted unless its graph can neither
//   trap nor call (the one body a caller-held sync-direct site binds), which
//   is expected of this pure-arithmetic loop, so it still counts nothing;
//   since wave 19 `lockedSum` is admitted (its loop is outside the block).
//   CRATONVM_DBG_IR_LINEAR_SCAN=1 prints the same per compile as
//   `[ir-ls] entry poll mode exits: ...`.
// * Time: stderr carries per-round and median times; they should match the
//   previous build (the fast path is byte-identical).
public class L2W18IrEntryLoopShapes {
    static final int ROUNDS = 7;
    static final int CALLS = 3_000;
    static final Object LOCK = new Object();

    /// javac's `goto` count loop.
    static int countUp(int n) {
        int i = 0;
        while (i < n) {
            i++;
        }
        return i;
    }

    /// A conditional back edge (do-while), two loop-carried locals.
    static int doWhileSum(int n) {
        int s = 0;
        int i = 0;
        do {
            s += i;
            i++;
        } while (i < n);
        return s;
    }

    /// A temp the bytecode never reads after the loop starts: dead at the
    /// header, which a mode exit may describe as undefined.
    static int deadTemp(int n) {
        int t = n * 7 + 3;
        int acc = t & 1;
        for (int i = 0; i < n; i++) {
            acc += i ^ (acc >>> 3);
        }
        return acc;
    }

    /// Nested loops with a long accumulator.
    static long nested(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            for (int j = 0; j < 8; j++) {
                acc += (long) i * j + (acc >>> 7);
            }
        }
        return acc;
    }

    /// ACC_SYNCHRONIZED and trap-free: no exit (wave 22, `ir_entry_poll_mode_exits`).
    static synchronized int synchronizedSum(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += i * 3;
        }
        return s;
    }

    /// A monitor block before the loop: admitted since wave 19 (lane L2).
    static int lockedSum(int n) {
        int s;
        synchronized (LOCK) {
            s = n;
        }
        for (int i = 0; i < n; i++) {
            s += i & 5;
        }
        return s;
    }

    public static void main(String[] args) {
        long[] sums = new long[6];
        long[] times = new long[ROUNDS];
        for (int r = 0; r < ROUNDS; r++) {
            long t0 = System.nanoTime();
            for (int c = 0; c < CALLS; c++) {
                int n = 200 + (c & 63);
                sums[0] += countUp(n);
                sums[1] += doWhileSum(n);
                sums[2] += deadTemp(n);
                sums[3] += nested(n);
                sums[4] += synchronizedSum(n);
                sums[5] += lockedSum(n);
            }
            times[r] = System.nanoTime() - t0;
            System.err.printf("round %d: %.2f ms%n", r, times[r] / 1e6);
        }
        java.util.Arrays.sort(times);
        System.err.printf("median: %.2f ms%n", times[ROUNDS / 2] / 1e6);
        String[] names = {"countUp", "doWhileSum", "deadTemp", "nested", "synchronizedSum", "lockedSum"};
        long total = 0;
        for (int k = 0; k < sums.length; k++) {
            System.out.println(names[k] + " " + sums[k]);
            total = total * 31 + sums[k];
        }
        System.out.println("total " + total);
    }
}
