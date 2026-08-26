import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

/**
 * Try to reproduce, in seconds, the stale virtual-call receiver that takes
 * hours to catch in netty.
 *
 * The catch (netty `ParameterizedSslHandlerTest`, 1 whole-class run in ~230)
 * was `NoSuchMethodError 'void java.lang.Object.address()J'` from
 * `CleanerJava25.allocate` calling `MemorySegment.address()` **through a
 * MethodHandle**, with the collector's own ledger confirming the receiver
 * address had been evacuated (`was_vacated=true moved_to=…`) and nothing
 * repaired the holder.
 *
 * The three ingredients that shape reproduces are: a MethodHandle virtual
 * invoke (which routes through `mh_dispatch` in `lang_invoke.rs` rather than an
 * ordinary `invokevirtual`), a receiver allocated immediately before the call,
 * and enough allocation between the two for a young evacuation to land in the
 * window. This does all three in a tight loop.
 *
 * A `NoSuchMethodError` naming `java.lang.Object` is the catch. Anything else
 * — including a clean finish — is the probe failing to reproduce, which is a
 * result about the PROBE and not about the defect.
 */
public final class MhStaleReceiverProbe {

    /** Six instance fields, matching the shape the catch dumped. */
    public static final class Seg {
        private final long base;
        private final long len;
        private final Object scope;
        private final int flags;
        private final Object extraA;
        private final Object extraB;

        Seg(long base, long len, Object scope) {
            this.base = base;
            this.len = len;
            this.scope = scope;
            this.flags = 0;
            this.extraA = null;
            this.extraB = null;
        }

        public long address() {
            return base;
        }

        public long size() {
            return len + (scope == null ? 0 : 1);
        }
    }

    private static Object sink;

    public static void main(String[] args) throws Throwable {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 4_000_000;
        int churn = args.length > 1 ? Integer.parseInt(args[1]) : 24;
        int workers = args.length > 2 ? Integer.parseInt(args[2]) : 4;

        // The collection must be requested by a PEER, not by the thread whose
        // receiver is at risk — see this file's header.
        Thread pressure = new Thread(() -> {
            Object[] ring = new Object[512];
            int k = 0;
            while (!Thread.currentThread().isInterrupted()) {
                ring[k++ & 511] = new byte[4096];
            }
        }, "gc-pressure");
        pressure.setDaemon(true);
        pressure.start();

        if (workers > 1) {
            Thread[] ts = new Thread[workers];
            final int per = rounds / workers;
            final java.util.concurrent.atomic.AtomicLong caught =
                    new java.util.concurrent.atomic.AtomicLong();
            for (int w = 0; w < workers; w++) {
                ts[w] = new Thread(() -> {
                    try {
                        caught.addAndGet(loop(per, churn));
                    } catch (Throwable t) {
                        System.out.println("PROBE worker threw " + t);
                    }
                }, "mh-worker-" + w);
                ts[w].start();
            }
            for (Thread t : ts) {
                t.join();
            }
            pressure.interrupt();
            System.out.println("PROBE done rounds=" + rounds + " churn=" + churn
                    + " workers=" + workers + " caught=" + caught.get());
            System.out.flush();
            System.exit(caught.get() > 0 ? 3 : 0);
        }

        long single = loop(rounds, churn);
        pressure.interrupt();
        System.out.println("PROBE done rounds=" + rounds + " churn=" + churn
                + " workers=1 caught=" + single);
        System.out.flush();
        System.exit(single > 0 ? 3 : 0);
    }

    private static long loop(int rounds, int churn) throws Throwable {
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodHandle address = lookup.findVirtual(
                Seg.class, "address", MethodType.methodType(long.class));
        MethodHandle size = lookup.findVirtual(
                Seg.class, "size", MethodType.methodType(long.class));

        Object[] keep = new Object[128];
        long sum = 0;
        long reported = 0;
        for (int i = 0; i < rounds; i++) {
            Object scope = new Object();
            Seg seg = new Seg(i, 131072, scope);
            // Allocate hard BETWEEN constructing the receiver and calling
            // through the handle: this is the window a young evacuation has to
            // land in for the receiver's holder to be left naming from-space.
            for (int j = 0; j < churn; j++) {
                keep[(i + j) & 127] = new byte[192];
            }
            sink = scope;
            try {
                sum += (long) address.invoke(seg);
                sum += (long) size.invoke(seg);
            } catch (NoSuchMethodError e) {
                // THE CATCH. Print and keep going so one run can report a rate
                // rather than a single event.
                reported++;
                System.out.println("PROBE CAUGHT round=" + i + " " + e.getMessage());
                System.out.flush();
                if (reported >= 20) {
                    break;
                }
            }
        }
        if (sum == Long.MIN_VALUE) {
            System.out.println("unreachable " + sum);
        }
        return reported;
    }
}
