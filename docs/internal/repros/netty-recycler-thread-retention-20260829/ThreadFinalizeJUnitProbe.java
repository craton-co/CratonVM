import java.lang.ref.WeakReference;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.stream.Stream;

import org.junit.jupiter.api.Timeout;
import org.junit.jupiter.params.ParameterizedTest;
import org.junit.jupiter.params.provider.Arguments;
import org.junit.jupiter.params.provider.MethodSource;

/**
 * The cell `netty-recycler-thread-retention-20260829`'s probes never covered:
 * netty's shape run INSIDE JUnit, with a witness that separates the two
 * failures its assertion cannot tell apart.
 *
 * <p>`RecyclerTest.testThreadCanBeCollectedEvenIfHandledObjectIsReferenced`
 * spins until a `finalize()` override sets a flag. That flag stays false in two
 * completely different worlds:
 *
 * <ul>
 *   <li>the Thread is still REACHABLE — the retention the page assumes; or
 *   <li>the Thread was collected perfectly well and its `finalize()` never
 *       ran — a finalization defect, which would make every root-scan lever in
 *       that page irrelevant by construction.
 * </ul>
 *
 * <p>A `WeakReference` to the same Thread distinguishes them: it is cleared
 * when (and only when) the referent becomes unreachable, and it does not depend
 * on the finalizer thread doing anything. So `weakCleared=true` with
 * `finalized=false` is the second world, and `weakCleared=false` is the first.
 *
 * <p>Deliberately NOT an assertion. It reports all six parameterisations and
 * passes either way, because the point is to read the two bits, not to add
 * another red test.
 */
public class ThreadFinalizeJUnitProbe {

    static Stream<Arguments> sixShapes() {
        return Stream.of(
                Arguments.of("NONE", true),
                Arguments.of("NONE", false),
                Arguments.of("PINNED", true),
                Arguments.of("PINNED", false),
                Arguments.of("FAST_THREAD_LOCAL", true),
                Arguments.of("FAST_THREAD_LOCAL", false));
    }

    @ParameterizedTest
    @Timeout(value = 60, unit = TimeUnit.SECONDS)
    @MethodSource("sixShapes")
    public void threadCollectedAndFinalized(String ownerType, boolean unguarded) throws Exception {
        final AtomicBoolean collected = new AtomicBoolean();

        Thread thread = new Thread(new Runnable() {
            @Override
            public void run() {
                // Nothing: this probe is asking about the Thread, and the
                // page's RecyclerRetainProbe already showed the Recycler's own
                // reference graph is not what retains it.
            }
        }) {
            @Override
            protected void finalize() throws Throwable {
                try {
                    collected.set(true);
                } finally {
                    super.finalize();
                }
            }
        };

        WeakReference<Thread> weak = new WeakReference<>(thread);

        thread.start();
        thread.join();
        thread = null;

        long deadlineNanos = System.nanoTime() + TimeUnit.SECONDS.toNanos(10);
        int rounds = 0;
        while (System.nanoTime() < deadlineNanos && !(collected.get() && weak.get() == null)) {
            System.gc();
            System.runFinalization();
            Thread.sleep(50);
            rounds++;
        }

        System.out.println("CK probe owner=" + ownerType + " unguarded=" + unguarded
                + " weakCleared=" + (weak.get() == null)
                + " finalized=" + collected.get()
                + " rounds=" + rounds);
    }
}
