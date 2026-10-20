import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * gci i28/tl (page gci-i20st-compatible-threadlocal-initial-value-pins-a-virtual-thread-20261015,
 * the residual initialValue() arm): a ThreadLocal SUBCLASS that overrides initialValue()
 * (no withInitial supplier) and parks inside it. On HotSpot 25 ThreadLocal.get() ->
 * setInitialValue() -> initialValue() is all Java, so the park unmounts the virtual thread
 * and every worker starts. Under --compatible this VM served get() with a native that ran
 * initialValue() nested under its own frame, pinning the carrier.
 *
 * Modes:
 *   iv N WAIT     N virtual threads; each counts STARTED down, then get()s a ThreadLocal whose
 *                 initialValue() parks until main opens the gate. Prints
 *                   tl28 iv threads=N started=S done=D
 *                 (HotSpot: S = D = N).
 *   ivset N WAIT  the same with a subclass that ALSO overrides set(Object). This VM keeps the
 *                 nested call there (setInitialValue never calls this.set, so the VM's helper,
 *                 which stores through tl.set, may not run an override): a RECORDED residual.
 *                   tl28 ivset threads=N started=S done=D
 *   sem           initialValue() semantics, on the main (platform) thread and inside one virtual
 *                 thread, each with fresh ThreadLocals; six lines per place:
 *                   tl28 sem <plat|virt> count get=v1,v1 calls=1 afterRemove=v2
 *                   tl28 sem <plat|virt> throw first=java.lang.IllegalStateException:first second=ok calls=2
 *                   tl28 sem <plat|virt> selfset get=outer again=outer
 *                   tl28 sem <plat|virt> recursive a=a+b b=b
 *                   tl28 sem <plat|virt> setcount get=s sets=0
 *                   tl28 sem <plat|virt> inherit parent=parent-init child=child-of-parent-init
 *
 * stdout is a pure function of the arguments on HotSpot; diagnostics on stderr.
 * Usage: GciTl28InitialValueProbe MODE [N [WAIT]]   (N default 300, WAIT default 60)
 */
public class GciTl28InitialValueProbe {
    static final AtomicInteger MISMATCH = new AtomicInteger();
    static final CountDownLatch GO = new CountDownLatch(1);
    static int waitSeconds = 60;

    /** Park until main opens the gate (bounded: a lost wake-up is a mismatch, not a hang). */
    static void gate() {
        try {
            if (!GO.await(2L * waitSeconds, TimeUnit.SECONDS)) {
                MISMATCH.incrementAndGet();
            }
        } catch (InterruptedException e) {
            MISMATCH.incrementAndGet();
        }
    }

    static final class ParkingLocal extends ThreadLocal<Integer> {
        @Override
        protected Integer initialValue() {
            gate();
            return Integer.valueOf(28);
        }
    }

    static final class ParkingLocalWithSet extends ThreadLocal<Integer> {
        final AtomicInteger sets = new AtomicInteger();

        @Override
        protected Integer initialValue() {
            gate();
            return Integer.valueOf(29);
        }

        @Override
        public void set(Integer v) {
            sets.incrementAndGet();
            super.set(v);
        }
    }

    static final ParkingLocal IV = new ParkingLocal();
    static final ParkingLocalWithSet IVSET = new ParkingLocalWithSet();

    static void runPark(String mode, int n) throws InterruptedException {
        final boolean withSet = mode.equals("ivset");
        final CountDownLatch started = new CountDownLatch(n);
        final CountDownLatch done = new CountDownLatch(n);
        for (int i = 0; i < n; i++) {
            Thread.ofVirtual().name("tl28-" + i).start(() -> {
                started.countDown();
                Integer v = withSet ? IVSET.get() : IV.get();
                int want = withSet ? 29 : 28;
                if (v == null || v.intValue() != want) {
                    MISMATCH.incrementAndGet();
                }
                done.countDown();
            });
        }
        started.await(waitSeconds, TimeUnit.SECONDS);
        long s = n - started.getCount();
        GO.countDown();
        done.await(waitSeconds, TimeUnit.SECONDS);
        long d = n - done.getCount();
        System.out.println("tl28 " + mode + " threads=" + n + " started=" + s + " done=" + d);
        System.err.println("[tl28] " + mode + " mismatches=" + MISMATCH.get()
                + (withSet ? " sets=" + IVSET.sets.get() : ""));
    }

    static final class CountingLocal extends ThreadLocal<String> {
        int calls;

        @Override
        protected String initialValue() {
            calls++;
            return "v" + calls;
        }
    }

    static final class ThrowingLocal extends ThreadLocal<String> {
        int calls;

        @Override
        protected String initialValue() {
            calls++;
            if (calls == 1) {
                throw new IllegalStateException("first");
            }
            return "ok";
        }
    }

    static final class SelfSetLocal extends ThreadLocal<String> {
        @Override
        protected String initialValue() {
            set("inner");
            return "outer";
        }
    }

    static final class InnerLocal extends ThreadLocal<String> {
        @Override
        protected String initialValue() {
            return "b";
        }
    }

    static final class OuterLocal extends ThreadLocal<String> {
        final InnerLocal inner;

        OuterLocal(InnerLocal inner) {
            this.inner = inner;
        }

        @Override
        protected String initialValue() {
            return "a+" + inner.get();
        }
    }

    static final class SetCountingLocal extends ThreadLocal<String> {
        int sets;

        @Override
        protected String initialValue() {
            return "s";
        }

        @Override
        public void set(String v) {
            sets++;
            super.set(v);
        }
    }

    static final class InheritingLocal extends InheritableThreadLocal<String> {
        @Override
        protected String initialValue() {
            return "parent-init";
        }

        @Override
        protected String childValue(String parentValue) {
            return "child-of-" + parentValue;
        }
    }

    static void semantics(String where) throws InterruptedException {
        String p = "tl28 sem " + where + " ";

        CountingLocal count = new CountingLocal();
        String g1 = count.get();
        String g2 = count.get();
        int callsBefore = count.calls;
        count.remove();
        String g3 = count.get();
        System.out.println(p + "count get=" + g1 + "," + g2 + " calls=" + callsBefore + " afterRemove=" + g3);

        ThrowingLocal thrower = new ThrowingLocal();
        String first;
        try {
            first = "returned:" + thrower.get();
        } catch (IllegalStateException e) {
            first = e.getClass().getName() + ":" + e.getMessage();
        }
        String second = thrower.get();
        System.out.println(p + "throw first=" + first + " second=" + second + " calls=" + thrower.calls);

        SelfSetLocal self = new SelfSetLocal();
        String s1 = self.get();
        String s2 = self.get();
        System.out.println(p + "selfset get=" + s1 + " again=" + s2);

        InnerLocal inner = new InnerLocal();
        OuterLocal outer = new OuterLocal(inner);
        String a = outer.get();
        String b = inner.get();
        System.out.println(p + "recursive a=" + a + " b=" + b);

        SetCountingLocal setCounting = new SetCountingLocal();
        String sc = setCounting.get();
        System.out.println(p + "setcount get=" + sc + " sets=" + setCounting.sets);

        final InheritingLocal inheriting = new InheritingLocal();
        String parent = inheriting.get();
        final String[] child = new String[1];
        Thread t = new Thread(() -> child[0] = inheriting.get(), "tl28-child-" + where);
        t.start();
        t.join();
        System.out.println(p + "inherit parent=" + parent + " child=" + child[0]);
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "iv";
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 300;
        waitSeconds = args.length > 2 ? Integer.parseInt(args[2]) : 60;
        if (mode.equals("iv") || mode.equals("ivset")) {
            runPark(mode, n);
        } else if (mode.equals("sem")) {
            semantics("plat");
            final Throwable[] failure = new Throwable[1];
            Thread v = Thread.ofVirtual().name("tl28-sem").start(() -> {
                try {
                    semantics("virt");
                } catch (Throwable e) {
                    failure[0] = e;
                }
            });
            v.join();
            if (failure[0] != null) {
                System.out.println("tl28 sem virt failed " + failure[0].getClass().getName());
                failure[0].printStackTrace();
            }
        } else {
            System.out.println("tl28 unknown mode " + mode);
        }
    }
}
