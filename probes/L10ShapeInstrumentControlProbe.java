import java.lang.reflect.Field;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.Executor;
import java.util.concurrent.ThreadPoolExecutor;
import java.util.concurrent.TimeUnit;

/**
 * NEGATIVE CONTROL for `CRATONVM_DBG_TPE_SHAPE` — not a correctness probe, and
 * deliberately NOT in any strict-corpus probe list.
 *
 * <p>The L10 lane's claim is that the `ThreadPoolExecutor.execute`
 * receiver-shape predicate answers `true` for every executor the factories
 * produce. Measured on both strict-corpus workloads that is `true=62 false=0`
 * and `true=38 false=0` — but a `false` count of zero only means something once
 * the `false` branch has been shown to fire at all. Otherwise "no fabricated
 * executor reached the sites" is indistinguishable from "the instrument's
 * failure path is dead code", which is the same silence-is-not-zero trap the
 * flag was designed around.
 *
 * <p>So this probe manufactures the one receiver that must answer `false`: a
 * `ThreadPoolExecutor` allocated with no constructor run, whose `workers` field
 * is therefore null. `Unsafe.allocateInstance` is how Objenesis (and therefore
 * every mocking framework) builds one, so this is not a synthetic curiosity —
 * it is the shape a Mockito mock of an executor actually has.
 *
 * <p><b>This probe diverges from HotSpot on purpose and must never be diffed
 * against it.</b> On a real JVM, `execute()` on a constructor-less executor
 * dereferences the null `ctl` and throws; the point here is only which branch
 * of the instrument fires on the way. Run it as:
 *
 * <pre>
 *   CRATONVM_DBG_TPE_SHAPE=1 cratonvm --real-jdk --java-home $JDK \
 *       -cp probes L10ShapeInstrumentControlProbe 2>&amp;1 | grep tpe-shape
 * </pre>
 *
 * and expect at least one `real=false reason=null-workers` line. If every line
 * says `real=true`, the instrument is broken and every `false=0` reading taken
 * with it is worthless.
 */
public final class L10ShapeInstrumentControlProbe {

    public static void main(String[] args) throws Exception {
        Object unsafe = theUnsafe();
        if (unsafe == null) {
            System.out.println("control.unsafe=unavailable — CANNOT run the control");
            return;
        }
        Object raw = unsafe.getClass()
                .getMethod("allocateInstance", Class.class)
                .invoke(unsafe, ThreadPoolExecutor.class);
        System.out.println("control.allocated=" + (raw != null)
                + " class=" + (raw == null ? "-" : raw.getClass().getName()));

        // A constructor-less executor: `workers` is null, so the receiver-shape
        // probe must answer false and the registered native must be forced.
        final CountDownLatch ran = new CountDownLatch(1);
        Executor e = (Executor) raw;
        String outcome;
        try {
            e.execute(ran::countDown);
            outcome = ran.await(10, TimeUnit.SECONDS) ? "task-ran" : "task-did-not-run";
        } catch (Throwable t) {
            // The HotSpot answer. Either way the probe has already fired.
            outcome = "threw:" + t.getClass().getName();
        }
        System.out.println("control.execute=" + outcome);
        System.out.println("L10ShapeInstrumentControlProbe done");
    }

    private static Object theUnsafe() {
        for (String cn : new String[] {"sun.misc.Unsafe", "jdk.internal.misc.Unsafe"}) {
            try {
                Class<?> c = Class.forName(cn);
                Field f = c.getDeclaredField("theUnsafe");
                f.setAccessible(true);
                Object u = f.get(null);
                if (u != null) return u;
            } catch (Throwable ignored) {
                // try the next one
            }
        }
        return null;
    }
}
