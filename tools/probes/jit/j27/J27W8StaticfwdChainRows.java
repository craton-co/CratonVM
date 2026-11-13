// j27 wave 8, lane W8T1: STATICFWD21-1 reference half (CRATONVM_JIT_IR_SPLICE_PUTSTATIC_LOOP_CHAIN,
// opt-in) -- a spliced callee that LOOPS and commits a plain putstatic (a reference store, or a
// primitive one that reads what it writes) is no longer refused by the planner as
// `ir-splice-putstatic-loop`; every trap after the commit resumes the callee at its own bci through
// the chain snapshot, so nothing committed is re-run
// (docs/feature-designs/jit-ir-optimizations.md).
//
// Expected output on HotSpot 25 (both CratonVM modes, every arm, must print the same `fact` lines):
//   fact pick 1
//   fact pick-identity true
//   fact pick-warm 20000
//   fact alloc 9
//   fact alloc-warm 180000
//   fact count-warm none:COUNT=3:SINK=3
//   fact trap-bounds java.lang.ArrayIndexOutOfBoundsException:Index 3 out of bounds for length 3:COUNT=3:SINK=3
//   fact trap-null java.lang.NullPointerException:COUNT=0:SINK=null
//   fact big 99999 true
// Derivation: pick(10) leaves SINK = BOXES[9 & 7] = BOXES[1] (the same object: identity true);
// callPick returns 1 on every warm call (20000). alloc(10) leaves a Cell(9) (20000 x 9). countRef
// over {1, 2, 3} with n = 3 from COUNT = 0: COUNT = 3, SINK = BOXES[3]. With n = 4 the fourth
// iteration's a[3] throws after three committed iterations: COUNT = 3, SINK = BOXES[3] -- a replay
// of the call from the caller's invoke would print COUNT=6. A null array throws at a[0]: COUNT = 0,
// SINK = null. big: alloc(100000) leaves Cell(99999) (an allocation-heavy loop: GC inside the
// spliced body).
// The trap rows are thrown ONCE after warming on non-throwing inputs, so HotSpot's
// OmitStackTraceInFastThrow never drops the message.
//
// Timing lines (`time <row> <ns/call>`, medians of 7) only with -Dj27.time=true.
//
// Arms (env vars; run each in --real-jdk and --compatible; `grep '^fact'` must equal HotSpot):
//   A  (none)                                                         -- control (switch off)
//   B  CRATONVM_JIT_IR_SPLICE_PUTSTATIC_LOOP_CHAIN=1
//   C  B + CRATONVM_DEOPT_EAGER_CHAINS=1       (facts only: every 64th passing chain guard resumes)
//   D  B + CRATONVM_JIT_IR_SPLICE_CHAIN_FENCES=0 (facts only: no chain fences, the switch is inert)
//   N  --nojit
// Engagement (arm A vs B, --real-jdk, CRATONVM_DBG_JITC=1, stderr):
//   A: `ir-splice-refused ir-splice-putstatic-loop` names pick / alloc / countRef.
//   B: `[cratonvm-jitc] ir-splice-putstatic-loop by chain J27W8StaticfwdChainRows.pick(I)V at caller pc ..`
//      (and alloc, countRef); D: no `by chain` line. In B an `IrBuilder::build refused` line for
//      callPick / callAlloc / callCountRef is the switch's cost (a pc with no chain snapshot): count
//      them, they decide the flip together with the timing.
// Timing decision (flip default ON only if): B below A on `pick-1k` and `alloc-1k`, nothing slower
//   than A by more than noise, facts equal in A-D and N on both modes (pin to P-cores on Windows,
//   interleave arms, 5 reps).
public class J27W8StaticfwdChainRows {
    static Object SINK;
    static int COUNT;
    static final Integer[] BOXES = new Integer[8];

    static {
        for (int i = 0; i < BOXES.length; i++) {
            BOXES[i] = Integer.valueOf(i);
        }
    }

    static final class Cell {
        final int v;

        Cell(int v) {
            this.v = v;
        }
    }

    static void pick(int n) {
        for (int i = 0; i < n; i++) {
            SINK = BOXES[i & 7];
        }
    }

    static int callPick(int n) {
        pick(n);
        return ((Integer) SINK).intValue();
    }

    static void alloc(int n) {
        for (int i = 0; i < n; i++) {
            SINK = new Cell(i);
        }
    }

    static int callAlloc(int n) {
        alloc(n);
        return ((Cell) SINK).v;
    }

    static void countRef(int[] a, int n) {
        for (int i = 0; i < n; i++) {
            SINK = BOXES[a[i] & 7];
            COUNT++;
        }
    }

    static String callCountRef(int[] a, int n) {
        try {
            countRef(a, n);
            return "none:COUNT=" + COUNT + ":SINK=" + SINK;
        } catch (ArrayIndexOutOfBoundsException e) {
            return e.getClass().getName() + ":" + e.getMessage() + ":COUNT=" + COUNT + ":SINK=" + SINK;
        } catch (NullPointerException e) {
            return e.getClass().getName() + ":COUNT=" + COUNT + ":SINK=" + SINK;
        }
    }

    static void fact(String row, Object v) {
        System.out.println("fact " + row + " " + v);
    }

    static final int WARM = 20000;

    public static void main(String[] args) {
        int[] three = {1, 2, 3};

        long pickWarm = 0;
        long allocWarm = 0;
        String countWarm = "";
        for (int r = 0; r < WARM; r++) {
            pickWarm += callPick(10);
            allocWarm += callAlloc(10);
            COUNT = 0;
            countWarm = callCountRef(three, 3);
        }

        fact("pick", callPick(10));
        fact("pick-identity", SINK == BOXES[1]);
        fact("pick-warm", pickWarm);
        fact("alloc", callAlloc(10));
        fact("alloc-warm", allocWarm);
        fact("count-warm", countWarm);
        COUNT = 0;
        fact("trap-bounds", callCountRef(three, 4));
        COUNT = 0;
        SINK = null;
        fact("trap-null", callCountRef(null, 2));
        int big = callAlloc(100000);
        fact("big", big + " " + (SINK instanceof Cell));

        if (Boolean.getBoolean("j27.time")) {
            time("pick-1k", () -> callPick(1000));
            time("alloc-1k", () -> callAlloc(1000));
        }
    }

    interface Op {
        long run();
    }

    static long timeSink;

    static void time(String row, Op op) {
        long[] reps = new long[7];
        int calls = 20000;
        for (int rep = 0; rep < reps.length; rep++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < calls; i++) {
                timeSink += op.run();
            }
            reps[rep] = (System.nanoTime() - t0) / calls;
        }
        java.util.Arrays.sort(reps);
        System.out.println("time " + row + " " + reps[reps.length / 2]);
    }
}
