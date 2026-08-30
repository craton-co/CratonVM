import java.lang.ref.WeakReference;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.stream.Stream;

import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.Timeout;
import org.junit.jupiter.params.ParameterizedTest;
import org.junit.jupiter.params.provider.Arguments;
import org.junit.jupiter.params.provider.MethodSource;

/**
 * Narrows `recyclertest-thread-not-collected-once-the-jit-warms-up` from "JUnit"
 * to a feature of JUnit — or off JUnit entirely.
 *
 * <p>`ThreadFinalizeJUnitProbe` established two things the page did not have: the
 * Thread really is RETAINED (a `WeakReference` to it never clears, so this is not
 * a finalization defect), and netty's `Recycler` is not involved at all — the
 * probe that reproduces has none. What is left is that the same shape collects
 * standalone and does not under JUnit.
 *
 * <p>Every method below is the SAME body. They differ only in how they are
 * invoked, so whichever ones retain name the ingredient:
 *
 * <ul>
 *   <li>{@link #main} — no JUnit at all, the standalone control;
 *   <li>{@link #plainTest} — reflective invocation by JUnit, nothing else;
 *   <li>{@link #timedTest} — adds `@Timeout`;
 *   <li>{@link #parameterizedTest} — adds the parameterized machinery, which
 *       holds an `Object[]` of arguments and the test instance;
 *   <li>{@link #timedParameterizedTest} — netty's exact combination.
 * </ul>
 */
public class ThreadRetainMatrix {

    /** The body under test. Returns "weakCleared=<b> finalized=<b> rounds=<n>". */
    static String runOnce(String label) throws Exception {
        final AtomicBoolean collected = new AtomicBoolean();
        Thread thread = new Thread(() -> { }) {
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

        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
        int rounds = 0;
        while (System.nanoTime() < deadline && !(collected.get() && weak.get() == null)) {
            System.gc();
            System.runFinalization();
            Thread.sleep(50);
            rounds++;
        }
        String r = "CK matrix " + label
                + " weakCleared=" + (weak.get() == null)
                + " finalized=" + collected.get()
                + " rounds=" + rounds;
        System.out.println(r);
        return r;
    }

    public static void main(String[] args) throws Exception {
        runOnce("standalone-main");
        runOnce("standalone-main-2nd");
    }

    @Test
    public void plainTest() throws Exception {
        runOnce("junit-plain");
    }

    @Test
    @Timeout(value = 60, unit = TimeUnit.SECONDS)
    public void timedTest() throws Exception {
        runOnce("junit-timed");
    }

    static Stream<Arguments> twoShapes() {
        return Stream.of(Arguments.of("A", true), Arguments.of("B", false));
    }

    @ParameterizedTest
    @MethodSource("twoShapes")
    public void parameterizedTest(String name, boolean flag) throws Exception {
        runOnce("junit-param-" + name + "-" + flag);
    }

    @ParameterizedTest
    @Timeout(value = 60, unit = TimeUnit.SECONDS)
    @MethodSource("twoShapes")
    public void timedParameterizedTest(String name, boolean flag) throws Exception {
        runOnce("junit-timed-param-" + name + "-" + flag);
    }
}
