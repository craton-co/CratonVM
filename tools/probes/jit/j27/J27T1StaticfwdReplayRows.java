// j27 wave 7, lane T1STATICFWD: STATICFWD21-1 (primitive half,
// CRATONVM_JIT_IR_SPLICE_PUTSTATIC_LOOP_REPLAY, opt-in) -- a spliced callee that LOOPS and whose
// only writes are plain primitive putstatics is spliced without the replay fence
// (docs/feature-designs/jit-ir-optimizations.md). A trap inside such a body
// re-runs the whole call from the caller's invoke; the rows below check that the re-run leaves
// exactly HotSpot's values.
//
// Expected output on HotSpot 25 (both CratonVM modes, every arm, must print the same `fact` lines):
//   fact fill 9
//   fact fill-warm 180000
//   fact sum 55
//   fact sum-warm 1100000
//   fact trap-warm none:S=2:T=12
//   fact trap-bounds java.lang.ArrayIndexOutOfBoundsException:Index 3 out of bounds for length 3:S=3:T=6
//   fact trap-div java.lang.ArithmeticException:/ by zero:S=0:T=-1
//   fact wide 31
//   fact bump 45
// Derivation: fill(10) leaves S = 9 (20000 warm calls: 180000). sum of 1..10 = 55 (x 20000 =
// 1100000). fillTrap({4,8,12}, 3, 1): S = 2, T = 12/1 = 12. With n = 4, d = 2: i = 0..2 store
// S = i, T = a[i]/2 = 2, 4, 6; i = 3 stores S = 3, then a[3] throws (T keeps 6). With d = 0
// (T reset to -1): i = 0 stores S = 0, then 4/0 throws. wide(10): L = 9*3 = 27, D = 4.5,
// 27 + (long) 4.5 = 31. bump(10) from C = 0: 0+1+...+9 = 45 (this body reads C, so it is NOT
// admitted and keeps the `ir-splice-putstatic-loop` refusal).
// The trap rows are thrown ONCE after warming on non-throwing inputs, so HotSpot's
// OmitStackTraceInFastThrow never drops the message.
//
// Timing lines (`time <row> <ns/call>`, medians of 7) only with -Dj27.time=true.
//
// Arms (env vars; run each in --real-jdk and --compatible; `grep '^fact'` must equal HotSpot):
//   A  (none)                                                        -- control (switch off)
//   B  CRATONVM_JIT_IR_SPLICE_PUTSTATIC_LOOP_REPLAY=1
//   C  B + CRATONVM_DBG_JIT_DEOPT_A_LOT=97        (facts only: every trap replays)
//   D  B + CRATONVM_JIT_IR_SPLICE_CHAIN_FENCES=0  (facts only: flat resumes, the replay route)
// Engagement (arm A vs B, --real-jdk, CRATONVM_DBG_JITC=1, stderr):
//   `[cratonvm-jitc] ir-splice-refused ir-splice-putstatic-loop <callee>` is printed for fill / sumInto / fillTrap / wideStores / bumpLoop in arm A; in arm B only for
//   bumpLoop and (since j27 wave 9, lane W9RPL, racy-inputs rule) fillTrap, whose `a[i]` read and
//   trap follow its store `S = i`, and no `IrBuilder::build refused` line names callFill / callSum /
//   callWide (a refusal there means planner and builder disagree: a defect, not a timing loss).
// Timing decision (flip the switch default ON only if): B below A on `fill-1k` and `sum-1k`
//   (the call is gone), nothing slower than A by more than noise, facts equal in A-D on both
//   modes and both hosts (pin to P-cores on Windows; interleave arms).
public class J27T1StaticfwdReplayRows {
    static int S;
    static int T;
    static int SUM;
    static long L;
    static double D;
    static int C;

    static void fill(int n) {
        for (int i = 0; i < n; i++) {
            S = i;
        }
    }

    static int callFill(int n) {
        fill(n);
        return S;
    }

    static void sumInto(int[] a) {
        int s = 0;
        for (int i = 0; i < a.length; i++) {
            s += a[i];
        }
        SUM = s;
    }

    static int callSum(int[] a) {
        sumInto(a);
        return SUM;
    }

    static void fillTrap(int[] a, int n, int d) {
        for (int i = 0; i < n; i++) {
            S = i;
            T = a[i] / d;
        }
    }

    static String callFillTrap(int[] a, int n, int d) {
        try {
            fillTrap(a, n, d);
            return "none:S=" + S + ":T=" + T;
        } catch (RuntimeException e) {
            return e.getClass().getName() + ":" + e.getMessage() + ":S=" + S + ":T=" + T;
        }
    }

    static void wideStores(int n) {
        for (int i = 0; i < n; i++) {
            L = (long) i * 3L;
            D = i * 0.5;
        }
    }

    static long callWide(int n) {
        wideStores(n);
        return L + (long) D;
    }

    static void bumpLoop(int n) {
        for (int i = 0; i < n; i++) {
            C += i;
        }
    }

    static int callBump(int n) {
        bumpLoop(n);
        return C;
    }

    static void fact(String row, Object v) {
        System.out.println("fact " + row + " " + v);
    }

    static final int WARM = 20000;

    public static void main(String[] args) {
        int[] ten = {1, 2, 3, 4, 5, 6, 7, 8, 9, 10};
        int[] three = {4, 8, 12};

        long fillWarm = 0;
        long sumWarm = 0;
        String trapWarm = "";
        long wideWarm = 0;
        for (int r = 0; r < WARM; r++) {
            fillWarm += callFill(10);
            sumWarm += callSum(ten);
            trapWarm = callFillTrap(three, 3, 1);
            wideWarm += callWide(10);
            C = 0;
            callBump(10);
        }

        fact("fill", callFill(10));
        fact("fill-warm", fillWarm);
        fact("sum", callSum(ten));
        fact("sum-warm", sumWarm);
        fact("trap-warm", trapWarm);
        fact("trap-bounds", callFillTrap(three, 4, 2));
        T = -1;
        fact("trap-div", callFillTrap(three, 3, 0));
        fact("wide", callWide(10) + (wideWarm - (long) WARM * 31L));
        C = 0;
        fact("bump", callBump(10));

        if (Boolean.getBoolean("j27.time")) {
            int[] big = new int[1000];
            for (int i = 0; i < big.length; i++) {
                big[i] = i;
            }
            time("fill-1k", () -> callFill(1000));
            time("sum-1k", () -> callSum(big));
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
