/**
 * A loop whose frame at the header is a UNIQUE fingerprint of its iteration
 * count — the shape the OSR frame comparator needs, and the reason it exists as
 * a separate probe from {@code OsrExitDifferentialProbe}.
 *
 * <h2>Why the other probe cannot serve</h2>
 *
 * The comparator maps each traced frame to its index in an un-compiled run's
 * sequence by exact frame equality. That index is only well defined when the
 * sequence is <b>injective</b>. Running it against
 * {@code OsrExitDifferentialProbe} showed how easily that fails, in two ways at
 * once:
 *
 * <ul>
 *   <li>it runs every shape <b>twice</b> (a warm round and a re-entry round),
 *       so every header frame appears at least twice;
 *   <li>several of its methods hold <b>reference</b> locals, whose raw heap
 *       addresses are not stable across two processes.
 * </ul>
 *
 * The address problem is fixed in the tracer (references render as
 * {@code null}/{@code ref}); the repetition is a property of the program and
 * has to be fixed by the program.
 *
 * <h2>What this one guarantees</h2>
 *
 * <ul>
 *   <li><b>One call, one loop.</b> {@code kernel} is invoked exactly once, so
 *       its loop header is reached exactly {@code n} times in a run.
 *   <li><b>A strictly monotone induction variable</b> in the frame, so no two
 *       arrivals can render identically.
 *   <li><b>Primitive locals only.</b> Nothing in the frame carries a heap
 *       address, so nothing depends on the tracer's reference handling.
 *   <li><b>A loop-carried accumulator the induction variable cannot
 *       re-derive</b> — {@code acc} depends on its own previous value — so a
 *       resume one iteration early or late is not recoverable by running the
 *       remaining trips, and the final value is not a pure function of
 *       {@code n}.
 * </ul>
 *
 * No {@code try}/{@code catch}: {@code compile_osr_artifact} refuses a method
 * with a non-empty exception table, and such a method would silently never OSR.
 */
public final class OsrFrameProbe {

    private static long kernel(int n) {
        long acc = 1;
        long mix = 0;
        for (int i = 0; i < n; i++) {
            acc = acc * 6364136223846793005L + (i | 1);
            acc ^= acc >>> 29;
            mix += acc & 0xffff;
        }
        return acc ^ mix;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 60000;
        System.out.println("OsrFrameProbe n=" + n + " result=" + kernel(n));
    }
}
