/**
 * JLS 12.6: "the thread that invokes the finalizer will not be holding any
 * user-visible synchronization locks when the finalizer is invoked".
 *
 * The main thread allocates garbage finalizable objects inside
 * {@code synchronized (LOCK)} with {@code inside = true}. Each
 * {@code finalize()} enters {@code LOCK} and checks {@code inside}. Monitors
 * are re-entrant, so a finalizer run inline by the allocating thread ENTERS
 * the critical section it is halfway through and sees {@code inside == true}.
 * A finalizer on any other thread blocks until main leaves the section.
 *
 * Filed by gc-common wave 2, lane F
 * ({@code common-w2f-finalizers-and-cleaners-run-on-the-allocating-mutator-FIXED-20260923.md}).
 * Fixed by default in wave 4, lane D ({@code FinalizerDelivery::WhenLockHeld}).
 * Committed as a probe in wave 5, lane D (2026-09-24).
 *
 * Prints {@code PROBE-OK} when no finalizer saw {@code inside} and at least one
 * finalizer ran after the section ended. HotSpot prints PROBE-OK.
 *
 * The run that discriminates is the interpreted allocation door, whose
 * collections drain finalizers inline. Measured on the round's w4 binary,
 * {@code -XX:+UseGenerationalGC --compatible} with
 * {@code CRATONVM_DISABLE_JIT=1}:
 * <ul>
 * <li>{@code CRATONVM_FINALIZER_THREAD=0} (the pre-w4 inline drain) prints
 *     {@code reentered=180763 PROBE-FAIL};</li>
 * <li>the default prints {@code reentered=0 PROBE-OK}.</li>
 * </ul>
 * With the JIT on, the compiled loop's collections come through the
 * allocation-failure door, which drains nothing, so every arm passes.
 */
public class FinalizerLockReentryProbe {
    static final Object LOCK = new Object();
    static boolean inside;
    static volatile int reentered;
    static volatile int ran;
    static volatile Object sink;

    static final class Fin {
        final byte[] pad = new byte[64];

        @Override
        @SuppressWarnings("deprecation")
        protected void finalize() {
            synchronized (LOCK) {
                if (inside) {
                    reentered++;
                }
                ran++;
            }
        }
    }

    public static void main(String[] a) throws Exception {
        synchronized (LOCK) {
            inside = true;
            for (int i = 0; i < 400_000; i += 2) {
                sink = new Fin();
                sink = new byte[512];
            }
            inside = false;
        }
        for (int i = 0; i < 20 && ran == 0; i++) {
            System.gc();
            System.runFinalization();
            Thread.sleep(50);
        }
        boolean ok = reentered == 0 && ran > 0;
        System.out.println("reentered=" + reentered + " ran=" + (ran > 0 ? "yes" : "NO")
                + (ok ? " PROBE-OK" : " PROBE-FAIL"));
    }
}
