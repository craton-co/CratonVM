// JIT round 21 wave 1, lane indy: CompletableFuture stage costs, decomposed.
//
// Target (r20 handoff perf table, R20PIndyVhSplice): `thenApply` / `complete` cost 954 / 406 ns
// (cf-complete 406 after CRATONVM_JIT_SPLICE_VH_IR_BOUND) against HotSpot 60 / 24. One op of
// `complete` is `new CompletableFuture()` + `complete(v)` = `completeValue` (RESULT.compareAndSet,
// a constant reference-field VarHandle CAS) + `postComplete()` (reads STACK). One op of `thenApply`
// on a completed future is `uniApplyStage` -> `newIncompleteFuture` + `uniApplyNow` -> the
// function's `apply` (a lambda) + `completeValue` again. This probe splits those so the slow layer
// is read off the rows instead of guessed:
//   cf-new                 new CompletableFuture()                       (allocation + <init>)
//   cf-complete            new + complete(v)                              (one CAS + postComplete)
//   cf-complete-twice      new + complete(v) + complete(w)                (the failed-CAS arm)
//   cf-completed-future    CompletableFuture.completedFuture(v)           (no CAS: the constructor)
//   cf-thenapply-done      completedFuture(v).thenApply(F)               (F a static non-capturing lambda)
//   cf-thenapply-capture   completedFuture(v).thenApply(x -> x + k)      (a capturing lambda per op)
//   cf-thenapply-pending   cf.thenApply(F); cf.complete(v); dep.join()  (push onto STACK, postComplete)
//   vh-cas-control         a reference-field VarHandle CAS on this probe's own class (the RESULT shape)
//   vh-get-control         a plain VarHandle getAcquire of the same field
//   lambda-apply-control   F.apply(v) through Function (the stage's interface call)
// Facts: values, getNow, isDone, exceptional completion and its message, the second complete's
// false, and join() of a pending stage.
//
// Run lines (build: javac -d tools/probes/out tools/probes/jit/r21/R21CfChain.java):
//   P=tools/probes/out
//   cratonvm -Xmx512m -cp $P R21CfChain                                   # default
//   CRATONVM_JIT_SPLICE_VH_IR_BOUND=0 cratonvm -Xmx512m -cp $P R21CfChain # r20 splice control
//   cratonvm --compatible -Xmx512m -cp $P R21CfChain                      # both modes
//   java -Xmx512m -cp $P R21CfChain                                       # HotSpot 25
// Engagement (one run, not for timing): CRATONVM_DBG_JITC=1, grep the kernels' names
//   (`R21CfChain.complete`, `thenApplyDone`, ...) for `inline-resolve REFUSED
//   java/util/concurrent/CompletableFuture.<name>` lines (completeValue, postComplete, uniApplyStage,
//   uniApplyNow, newIncompleteFuture, <init>) and their reason; `[ir] vh-const` / `ir-vh-bind`
//   lines for the RESULT / STACK sites; `[ir] blind-dispatch` lines naming
//   CompletableFuture methods left on the by-name dispatch helper.
//
// HotSpot expectation: every non-`time` line identical. time rows: HotSpot ~10-30 ns for the cf
// rows, ~1-5 for the controls. The row whose ratio to HotSpot jumps names the slow layer.

import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.util.Arrays;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionException;
import java.util.function.Function;

public class R21CfChain {
    static final int N = 200_000;
    static final int REPS = 9;
    static long sink;

    static final Function<Integer, Integer> F = x -> x + 1;
    static final Integer[] BOXES = new Integer[128];
    static {
        for (int i = 0; i < BOXES.length; i++) BOXES[i] = i;
    }

    volatile Object result;
    static final VarHandle RESULT;
    static {
        try {
            RESULT = MethodHandles.lookup().findVarHandle(R21CfChain.class, "result", Object.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static long cfNew(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            CompletableFuture<Integer> f = new CompletableFuture<>();
            s += f.isDone() ? 1 : 0;
        }
        return s;
    }

    static long complete(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            CompletableFuture<Integer> f = new CompletableFuture<>();
            s += f.complete(BOXES[i & 127]) ? 1 : 0;
        }
        return s;
    }

    static long completeTwice(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            CompletableFuture<Integer> f = new CompletableFuture<>();
            f.complete(BOXES[i & 127]);
            s += f.complete(BOXES[(i + 1) & 127]) ? 1 : 0;
        }
        return s;
    }

    static long completedFuture(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += CompletableFuture.completedFuture(BOXES[i & 127]).getNow(0);
        }
        return s;
    }

    static long thenApplyDone(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += CompletableFuture.completedFuture(BOXES[i & 127]).thenApply(F).getNow(0);
        }
        return s;
    }

    static long thenApplyCapture(int n, int k) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += CompletableFuture.completedFuture(BOXES[i & 127]).thenApply(x -> x + k).getNow(0);
        }
        return s;
    }

    static long thenApplyPending(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            CompletableFuture<Integer> f = new CompletableFuture<>();
            CompletableFuture<Integer> d = f.thenApply(F);
            f.complete(BOXES[i & 127]);
            s += d.join();
        }
        return s;
    }

    static long vhCas(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            R21CfChain h = new R21CfChain();
            s += RESULT.compareAndSet(h, null, BOXES[i & 127]) ? 1 : 0;
        }
        return s;
    }

    static long vhGet(R21CfChain h, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += ((Integer) RESULT.getAcquire(h)) & 1;
        }
        return s;
    }

    static long lambdaApply(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) s += F.apply(BOXES[i & 127]);
        return s;
    }

    static double once(int which, R21CfChain h) {
        long t0 = System.nanoTime();
        long s = switch (which) {
            case 0 -> cfNew(N);
            case 1 -> complete(N);
            case 2 -> completeTwice(N);
            case 3 -> completedFuture(N);
            case 4 -> thenApplyDone(N);
            case 5 -> thenApplyCapture(N, 3);
            case 6 -> thenApplyPending(N);
            case 7 -> vhCas(N);
            case 8 -> vhGet(h, N);
            default -> lambdaApply(N);
        };
        sink += s;
        return (System.nanoTime() - t0) / (double) N;
    }

    static final String[] ROWS = {
        "cf-new", "cf-complete", "cf-complete-twice", "cf-completed-future", "cf-thenapply-done",
        "cf-thenapply-capture", "cf-thenapply-pending", "vh-cas-control", "vh-get-control",
        "lambda-apply-control",
    };

    public static void main(String[] args) {
        CompletableFuture<Integer> f = new CompletableFuture<>();
        System.out.println("fact new isDone=" + f.isDone() + " getNow=" + f.getNow(-1));
        System.out.println("fact complete " + f.complete(7) + " again " + f.complete(8) + " join " + f.join());
        System.out.println("fact thenApply " + f.thenApply(F).join());
        CompletableFuture<Integer> p = new CompletableFuture<>();
        CompletableFuture<Integer> d = p.thenApply(x -> x * 10);
        System.out.println("fact pending-dep isDone=" + d.isDone());
        p.complete(4);
        System.out.println("fact pending-dep join=" + d.join());
        CompletableFuture<Integer> e = new CompletableFuture<>();
        e.completeExceptionally(new IllegalStateException("bad stage"));
        try {
            e.thenApply(F).join();
            System.out.println("fact exceptional no exception");
        } catch (CompletionException ce) {
            System.out.println("fact exceptional " + ce.getClass().getName() + ": " + ce.getMessage());
        }
        R21CfChain h = new R21CfChain();
        System.out.println("fact vh-cas " + RESULT.compareAndSet(h, null, BOXES[5]) + " "
                + RESULT.compareAndSet(h, null, BOXES[6]) + " " + h.result);
        long[] expect = new long[ROWS.length];
        for (int which = 0; which < ROWS.length; which++) {
            sink = 0;
            once(which, h);
            expect[which] = sink;
        }
        System.out.println("fact sums " + Arrays.toString(expect));
        for (int which = 0; which < ROWS.length; which++) {
            for (int w = 0; w < 3; w++) once(which, h);
            double[] t = new double[REPS];
            for (int i = 0; i < REPS; i++) t[i] = once(which, h);
            Arrays.sort(t);
            System.out.println("time " + ROWS[which] + " " + Math.round(t[REPS / 2]) + " ns/op");
        }
    }
}
