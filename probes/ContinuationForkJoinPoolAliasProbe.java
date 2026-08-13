import java.util.concurrent.ForkJoinPool;

import jdk.internal.vm.Continuation;
import jdk.internal.vm.ContinuationScope;

/**
 * Read-side slot-alias probe for the two unguarded LIVE rows of
 * W7-69-read-side-alias-instrument.md §6 — {@code jdk/internal/vm/Continuation}
 * (5 synthetic slots against a 10-field real class) and
 * {@code java/util/concurrent/ForkJoinPool} (2 against 16).
 *
 * <p>The trap this probe exists to avoid is the one that made the defect
 * invisible for as long as it has been there: the native writes slot <i>k</i>
 * and the native reads slot <i>k</i> back, so the pair agrees no matter what
 * slot <i>k</i> means on the loaded class. <b>Every check below has an
 * independent read</b> — a method whose body is real JDK bytecode addressing
 * the field by its own declared index, with no CratonVM native registered on
 * it — sitting beside the native answer, and prints the two together so a
 * disagreement is the finding rather than something a reader has to infer.
 *
 * <p>The independent reads:
 * <ul>
 *   <li>{@code Continuation.toString()} — JDK bytecode, {@code "... scope: " +
 *       scope}. It reads the real {@code scope} field (declaration index 1).
 *       The synthetic map puts {@code scope} at 0, which is really
 *       {@code target}, so a swapped pair prints the {@code Runnable} here.</li>
 *   <li>{@code ForkJoinPool.toString()} — JDK bytecode, and it prints
 *       {@code parallelism = } from the real {@code parallelism} field
 *       (declaration index 15). The synthetic map puts it at 0, which is
 *       really {@code termination}.</li>
 * </ul>
 *
 * <p>Section 2 is the sharp one and it is a <b>guard</b>, not a feature: the
 * completed-continuation check reads slot 2 as an {@code int} state, and on the
 * real class slot 2 is {@code parent}, a {@code Continuation} reference, so the
 * {@code Value::Int} match falls through to the "never ran" arm and the guard
 * cannot fire. "The continuation ran" is not a test of it; "the second
 * {@code run()} was refused" is.
 *
 * <p>Every expected value is measured against the HotSpot image, never
 * guessed — run this class on both VMs and diff. Needs
 * {@code --add-exports java.base/jdk.internal.vm=ALL-UNNAMED}.
 */
public class ContinuationForkJoinPoolAliasProbe {

    public static void main(String[] args) {
        run("CONT", ContinuationForkJoinPoolAliasProbe::section1And2Continuation);
        run("FJP", ContinuationForkJoinPoolAliasProbe::section3ForkJoinPool);
    }

    /**
     * Each section runs behind its own catch so a failure in one still leaves
     * the other's transcript comparable. A section that dies prints WHY —
     * a swallowed section reads as a section that passed.
     */
    private static void run(String tag, Runnable section) {
        try {
            section.run();
        } catch (Throwable t) {
            System.out.println(tag + " SECTION-FAILED " + t);
        }
    }

    // ------------------------------------------------------------------
    // 1. Continuation scope/target — the swapped reference pair.
    // 2. Continuation completed-guard — the guard that cannot fire.
    // ------------------------------------------------------------------
    private static void section1And2Continuation() {
        ContinuationScope scope = new ContinuationScope("aliasProbeScope");
        final int[] ran = {0};
        Runnable body = () -> ran[0]++;
        Continuation c = new Continuation(scope, body);

        // INDEPENDENT read of the `scope` field: Continuation.toString() is JDK
        // bytecode appending the real `scope` field. `ContinuationScope
        // .toString()` returns its name, so a correctly-stored scope makes the
        // scope name appear here. A swapped pair prints the lambda instead.
        String beforeRun = c.toString();
        System.out.println("CONT scope-via-toString-holds-name="
                + beforeRun.contains("aliasProbeScope"));
        System.out.println("CONT scope-via-toString-holds-lambda="
                + beforeRun.contains("$$Lambda"));

        c.run();
        System.out.println("CONT body-ran-count=" + ran[0]);

        // The guard. NOT "did it run" — "was the second run refused".
        String secondRun;
        try {
            c.run();
            secondRun = "NO-THROW(ran=" + ran[0] + ")";
        } catch (Throwable t) {
            secondRun = "THREW:" + t.getClass().getName();
        }
        System.out.println("CONT second-run=" + secondRun);
        System.out.println("CONT body-ran-count-after-second=" + ran[0]);
    }

    // ------------------------------------------------------------------
    // 3. ForkJoinPool.getParallelism() on a pool the native did not allocate.
    // ------------------------------------------------------------------
    private static void section3ForkJoinPool() {
        // A pool the native did NOT allocate: real bytecode built it and set
        // the real `parallelism` field (declaration index 15). The native
        // `getParallelism` is FORCED ahead of the real bytecode by
        // `is_forkjoin_native_override`, so this is the read under test.
        ForkJoinPool explicit = new ForkJoinPool(4);
        report("explicit-pool", explicit);

        ForkJoinPool common = ForkJoinPool.commonPool();
        report("common-pool", common);
        System.out.println("FJP common-pool getCommonPoolParallelism="
                + ForkJoinPool.getCommonPoolParallelism());
    }

    private static void report(String label, ForkJoinPool pool) {
        int viaAccessor;
        try {
            viaAccessor = pool.getParallelism();
        } catch (Throwable t) {
            System.out.println("FJP " + label + " getParallelism THREW " + t);
            return;
        }
        // INDEPENDENT read: ForkJoinPool.toString() is JDK bytecode and renders
        // the real `parallelism` field. It is not registered as a native here,
        // so it cannot agree with getParallelism() by construction.
        String viaToString;
        try {
            viaToString = parallelismFromToString(pool.toString());
        } catch (Throwable t) {
            viaToString = "THREW:" + t.getClass().getName();
        }
        System.out.println("FJP " + label + " getParallelism=" + viaAccessor
                + " toString-parallelism=" + viaToString
                + " agree=" + String.valueOf(viaAccessor).equals(viaToString));
    }

    private static String parallelismFromToString(String s) {
        int i = s.indexOf("parallelism = ");
        if (i < 0) {
            return "ABSENT";
        }
        int j = i + "parallelism = ".length();
        int k = j;
        while (k < s.length() && Character.isDigit(s.charAt(k))) {
            k++;
        }
        return k == j ? "ABSENT" : s.substring(j, k);
    }
}
