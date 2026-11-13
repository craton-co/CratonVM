// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Same-target receiver pairs, separate exact-class rows, and guard misses.
import java.lang.ref.*;

public class J27SameTargetReferenceGet {
    static final Object TARGET = new Object();
    static final Object OTHER = new Object();
    static final WeakReference<Object> WEAK = new WeakReference<>(TARGET);
    static class Sub extends WeakReference<Object> { Sub(Object o) { super(o); } }
    static final Sub SUB = new Sub(TARGET);
    static final class Third extends Sub { Third() { super(TARGET); } }
    static final class Override extends Sub {
        Override() { super(TARGET); }
        public Object get() { return OTHER; }
    }
    // Loaded only after the shared kernel has warmed.
    public static final class Late extends Sub {
        public Late() { super(TARGET); }
        public Object get() { return OTHER; }
    }
    static int shared(WeakReference<Object> r) {
        int n = 0;
        for (int i = 0; i < 16; i++) if (r.get() == TARGET) n++;
        return n;
    }
    static int weakOnly(WeakReference<Object> r) {
        int n = 0;
        for (int i = 0; i < 16; i++) if (r.get() == TARGET) n++;
        return n;
    }
    static int subOnly(Sub r) {
        int n = 0;
        for (int i = 0; i < 16; i++) if (r.get() == TARGET) n++;
        return n;
    }
    static int distinct(WeakReference<Object> r) {
        int n = 0;
        for (int i = 0; i < 16; i++) if (r.get() == TARGET) n++;
        return n;
    }
    static long sink;
    static void fact(String name, Object value) { System.out.println("fact " + name + " " + value); }
    // Separate drivers keep the timed receiver class stable. A shared driver
    // timed first with WEAK and then SUB would measure its own profile change.
    static void timeWeak() {
        for (int i = 0; i < 200000; i++) sink += weakOnly(WEAK);
        int reps = Integer.getInteger("j27.reps", 1000000);
        long t = System.nanoTime(), sum = 0;
        for (int i = 0; i < reps; i++) sum += weakOnly(WEAK);
        long elapsed = System.nanoTime() - t;
        if (sum != 16L * reps) throw new AssertionError("weak checksum " + sum);
        sink += sum;
        fact("timing-weak-checksum", sum);
        System.out.println("time weak-only " + ((double)elapsed / reps) + " ns/16");
    }
    static void timeSub() {
        for (int i = 0; i < 200000; i++) sink += subOnly(SUB);
        int reps = Integer.getInteger("j27.reps", 1000000);
        long t = System.nanoTime(), sum = 0;
        for (int i = 0; i < reps; i++) sum += subOnly(SUB);
        long elapsed = System.nanoTime() - t;
        if (sum != 16L * reps) throw new AssertionError("sub checksum " + sum);
        sink += sum;
        fact("timing-sub-checksum", sum);
        System.out.println("time sub-only " + ((double)elapsed / reps) + " ns/16");
    }
    @SuppressWarnings("unchecked")
    public static void main(String[] args) throws Exception {
        Override overridden = new Override();
        for (int i = 0; i < Integer.getInteger("j27.warm", 300000); i++) {
            sink += shared(WEAK) + shared(SUB) + weakOnly(WEAK) + subOnly(SUB);
            sink += distinct(WEAK) + distinct(overridden);
        }
        fact("weak", shared(WEAK));
        fact("sub", shared(SUB));
        fact("weak-only", weakOnly(WEAK));
        fact("sub-only", subOnly(SUB));
        if (Boolean.getBoolean("j27.time")) {
            timeWeak();
            timeSub();
        }
        fact("third", shared(new Third()));
        fact("override", shared(overridden));
        fact("distinct-weak", distinct(WEAK));
        fact("distinct-override", distinct(overridden));
        WeakReference<Object> late = (WeakReference<Object>)Class.forName(
            "J27SameTargetReferenceGet$Late").getConstructor().newInstance();
        fact("late-override", shared(late));
        String npe;
        try { shared(null); npe = "missing"; }
        catch (NullPointerException e) { npe = e.toString(); }
        fact("null", npe);
        WEAK.clear(); SUB.clear();
        fact("cleared-weak", shared(WEAK));
        fact("cleared-sub", shared(SUB));
        fact("cleared-refers-to-null", WEAK.refersTo(null) && SUB.refersTo(null));
        for (int i = 0; i < 12; i++) {
            WeakReference<Object> r = new WeakReference<>(TARGET);
            byte[] garbage = new byte[262144]; garbage[0] = (byte)i;
            System.gc();
            if (shared(r) != 16 || !r.refersTo(TARGET)) throw new AssertionError("live referent " + i);
            r.clear();
            if (shared(r) != 0 || !r.refersTo(null)) throw new AssertionError("clear " + i);
        }
        fact("gc-live-and-clear", true);
        Reference.reachabilityFence(TARGET);
        fact("sink-positive", sink > 0);
    }
}
