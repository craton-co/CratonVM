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

        // --- compareAndSet specifics, warm enough to be COMPILED ----------
        // The inline arm is one `LOCK CMPXCHG`, and the two mistakes it can
        // make are invisible to a single cold call: comparing against the
        // wrong register (the receiver instead of the expected value), and
        // returning the witness instead of ZF. Both show up as a wrong
        // BOOLEAN, so every assertion here checks the boolean AND the field.
        AtomicLong cas = new AtomicLong(0);
        long hits = 0, misses = 0;
        for (int k = 0; k < 400_000; k++) {
            long want = cas.get();
            if (cas.compareAndSet(want, want + 1)) hits++;
            // A CAS against a value the field does NOT hold must fail and must
            // leave the field alone.
            if (cas.compareAndSet(want - 7, 999)) misses++;
        }
        eq("CAS hits", hits, 400_000);
        eq("CAS bogus-expect successes", misses, 0);
        eq("CAS final value", cas.get(), 400_000);

        AtomicInteger icas = new AtomicInteger(0);
        long ihits = 0, imisses = 0;
        for (int k = 0; k < 400_000; k++) {
            int want = icas.get();
            if (icas.compareAndSet(want, want + 1)) ihits++;
            if (icas.compareAndSet(want - 7, 999)) imisses++;
        }
        eq("int CAS hits", ihits, 400_000);
        eq("int CAS bogus-expect successes", imisses, 0);
        eq("int CAS final value", icas.get(), 400_000);

        // weakCompareAndSet shares the emitter arm; `LOCK CMPXCHG` never fails
        // spuriously, so a warmed loop must reach the target exactly.
        AtomicLong weak = new AtomicLong(0);
        for (int k = 0; k < 200_000; k++) {
            long w;
            do { w = weak.get(); } while (!weak.weakCompareAndSet(w, w + 1));
        }
        eq("weakCAS final value", weak.get(), 200_000);

        // Boundary values: the 64-bit arm must not be reading 32 bits.
        AtomicLong wide = new AtomicLong(0);
        for (int k = 0; k < 100_000; k++) {
            wide.set(0x0123456789ABCDEFL);
            if (!wide.compareAndSet(0x0123456789ABCDEFL, Long.MIN_VALUE)) fails++;
            eq("wide CAS result", wide.get(), Long.MIN_VALUE);
            if (wide.compareAndSet(0x89ABCDEFL, 1)) fails++;   // low 32 bits only
            if (fails > 10) break;
        }

        System.out.println("CK AtomicCorrectness fails=" + fails);
    }
}
