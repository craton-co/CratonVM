import java.util.concurrent.atomic.*;

/**
 * Semantics of the atomic classes, so a speed change can be shown not to have
 * broken them. Every assertion names the value it expected.
 *
 * Matters because the registered natives AND the JIT intrinsics both address
 * `value` as FIELD SLOT 0, while the JDK bytecode addresses it by resolved
 * name. If those two disagree, dropping the constructor native writes the seed
 * to one slot and reads it from another — which shows up as a wrong VALUE, not
 * a crash, and not in any timing run.
 */
public class AtomicCorrectness {
    static int fails = 0;
    static void eq(String what, long got, long want) {
        if (got != want) { System.out.println("CK FAIL " + what + ": got " + got + " want " + want); fails++; }
    }
    static void eqb(String what, boolean got, boolean want) {
        if (got != want) { System.out.println("CK FAIL " + what + ": got " + got + " want " + want); fails++; }
    }
    public static void main(String[] a) throws Exception {
        AtomicLong l = new AtomicLong(42);
        eq("AtomicLong(42).get", l.get(), 42);
        eq("AtomicLong.incrementAndGet", l.incrementAndGet(), 43);
        eq("AtomicLong.getAndIncrement", l.getAndIncrement(), 43);
        eq("AtomicLong.get after", l.get(), 44);
        eq("AtomicLong.getAndAdd(10)", l.getAndAdd(10), 44);
        eq("AtomicLong.addAndGet(-4)", l.addAndGet(-4), 50);
        eqb("AtomicLong.CAS(50,7) hit", l.compareAndSet(50, 7), true);
        eqb("AtomicLong.CAS(50,9) miss", l.compareAndSet(50, 9), false);
        eq("AtomicLong.get after CAS", l.get(), 7);
        l.set(-1); eq("AtomicLong.set(-1)", l.get(), -1);
        eq("AtomicLong.getAndSet(5)", l.getAndSet(5), -1);
        eq("AtomicLong.longValue", l.longValue(), 5);
        eq("AtomicLong.intValue", l.intValue(), 5);
        eq("new AtomicLong().get", new AtomicLong().get(), 0);
        eq("AtomicLong big", new AtomicLong(0x0123456789ABCDEFL).get(), 0x0123456789ABCDEFL);

        AtomicInteger i = new AtomicInteger(7);
        eq("AtomicInteger(7).get", i.get(), 7);
        eq("AtomicInteger.incrementAndGet", i.incrementAndGet(), 8);
        eqb("AtomicInteger.CAS(8,3) hit", i.compareAndSet(8, 3), true);
        eqb("AtomicInteger.CAS(8,4) miss", i.compareAndSet(8, 4), false);
        eq("AtomicInteger.get after CAS", i.get(), 3);
        eq("new AtomicInteger().get", new AtomicInteger().get(), 0);
        eq("AtomicInteger min", new AtomicInteger(Integer.MIN_VALUE).get(), Integer.MIN_VALUE);

        AtomicReference<String> r = new AtomicReference<>("a");
        eqb("AtomicReference.get", "a".equals(r.get()), true);
        eqb("AtomicReference.CAS hit", r.compareAndSet("a", "b"), true);
        eqb("AtomicReference.get after", "b".equals(r.get()), true);
        eqb("new AtomicReference().get==null", new AtomicReference<Object>().get() == null, true);

        AtomicBoolean bo = new AtomicBoolean(true);
        eqb("AtomicBoolean(true).get", bo.get(), true);
        eqb("AtomicBoolean.CAS", bo.compareAndSet(true, false), true);
        eqb("AtomicBoolean.get after", bo.get(), false);

        // The toString path reads the field by a different route than get().
        eqb("AtomicLong.toString", new AtomicLong(1234).toString().equals("1234"), true);
        eqb("AtomicInteger.toString", new AtomicInteger(-99).toString().equals("-99"), true);

        // Contended CAS: two threads racing must sum exactly.
        AtomicLong c = new AtomicLong(0);
        Thread[] ts = new Thread[4];
        for (int t = 0; t < ts.length; t++) {
            ts[t] = new Thread(() -> { for (int k = 0; k < 50_000; k++) c.incrementAndGet(); });
            ts[t].start();
        }
        for (Thread t : ts) t.join();
        eq("AtomicLong contended incrementAndGet", c.get(), 200_000);

        AtomicLong d = new AtomicLong(0);
        Thread[] us = new Thread[4];
        for (int t = 0; t < us.length; t++) {
            us[t] = new Thread(() -> {
                for (int k = 0; k < 50_000; k++) {
                    long v; do { v = d.get(); } while (!d.compareAndSet(v, v + 1));
                }
            });
            us[t].start();
        }
        for (Thread t : us) t.join();
        eq("AtomicLong contended CAS loop", d.get(), 200_000);

        System.out.println("CK AtomicCorrectness fails=" + fails);
    }
}
