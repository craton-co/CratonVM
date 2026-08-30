import java.util.concurrent.atomic.*;

/**
 * L6 differential sweep: `java.util.concurrent.atomic`.
 *
 * The lane's first pass took ConcurrentHashMap, Thread, ForkJoinTask/Pool and
 * AsynchronousFileChannel; its second took the synchronizers. This is the third
 * half of the same package and had never been asked.
 *
 * Weighted where the campaign keeps finding defects: ARGUMENT VALIDATION,
 * REFUSAL paths and EDGE VALUES. A JDK atomic is a thin wrapper of checks over
 * a VarHandle/Unsafe primitive, so the checks are exactly what a native shadow
 * drops — and this VM's atomics are lock-based rather than hardware, which is a
 * different implementation of the same contract.
 *
 * One row per assertion, never a nested print inside a row.
 */
public class AtomicFamilySweep {
    /** Drop `@<hex>` identity hashes. Hand-rolled: a probe must not normalise
     *  itself with machinery the VM under test also implements. */
    static String norm(String s) {
        StringBuilder b = new StringBuilder(s.length());
        int i = 0;
        while (i < s.length()) {
            char c = s.charAt(i);
            b.append(c);
            i++;
            if (c != '@') {
                continue;
            }
            int j = i;
            if (j + 1 < s.length() && s.charAt(j) == '0' && s.charAt(j + 1) == 'x') {
                j += 2;
            }
            int start = j;
            while (j < s.length()) {
                char h = s.charAt(j);
                boolean hex = (h >= '0' && h <= '9') || (h >= 'a' && h <= 'f') || (h >= 'A' && h <= 'F');
                if (!hex) {
                    break;
                }
                j++;
            }
            if (j > start) {
                b.append("<id>");
                i = j;
            }
        }
        return b.toString();
    }

    static void p(String tag, Object v) { System.out.println(tag + " |" + norm(String.valueOf(v)) + "|"); }

    interface Body { Object run() throws Throwable; }

    static void t(String tag, Body c) {
        try { p(tag, c.run()); }
        catch (Throwable e) {
            String m = e.getMessage();
            p(tag, "THREW " + e.getClass().getName() + (m == null ? "" : ": " + m));
        }
    }

    // Targets for the field updaters.
    static class Holder {
        volatile int vi = 1;
        volatile long vl = 2L;
        volatile String vs = "s";
        int plain = 3;                 // NOT volatile — newUpdater must refuse
        static volatile int svi = 4;   // static — must also be refused
        long plainLong = 5L;           // fails BOTH the type and volatile checks
        static int splain = 7;         // static AND non-volatile
    }

    public static void main(String[] args) {
        // ---- AtomicInteger -------------------------------------------------
        t("ai.default.get", () -> new AtomicInteger().get());
        t("ai.set.get", () -> { AtomicInteger a = new AtomicInteger(); a.set(5); return a.get(); });
        t("ai.getAndSet", () -> new AtomicInteger(1).getAndSet(9));
        t("ai.cas.hit", () -> new AtomicInteger(1).compareAndSet(1, 2));
        t("ai.cas.miss", () -> new AtomicInteger(1).compareAndSet(7, 2));
        t("ai.cas.miss.leaves", () -> { AtomicInteger a = new AtomicInteger(1);
            a.compareAndSet(7, 2); return a.get(); });
        t("ai.getAndIncrement", () -> new AtomicInteger(1).getAndIncrement());
        t("ai.incrementAndGet", () -> new AtomicInteger(1).incrementAndGet());
        t("ai.getAndAdd", () -> new AtomicInteger(1).getAndAdd(4));
        t("ai.overflow", () -> new AtomicInteger(Integer.MAX_VALUE).incrementAndGet());
        t("ai.underflow", () -> new AtomicInteger(Integer.MIN_VALUE).decrementAndGet());
        t("ai.updateAndGet.null", () -> new AtomicInteger(1).updateAndGet(null));
        t("ai.accumulate.null", () -> new AtomicInteger(1).accumulateAndGet(2, null));
        t("ai.accumulateAndGet", () -> new AtomicInteger(3).accumulateAndGet(4, Integer::sum));
        t("ai.toString", () -> new AtomicInteger(7).toString());
        t("ai.intValue", () -> new AtomicInteger(7).intValue());
        t("ai.longValue", () -> new AtomicInteger(7).longValue());
        t("ai.doubleValue", () -> new AtomicInteger(7).doubleValue());
        t("ai.weakCASPlain", () -> new AtomicInteger(1).weakCompareAndSetPlain(1, 2));
        t("ai.compareAndExchange", () -> new AtomicInteger(1).compareAndExchange(1, 2));
        t("ai.compareAndExchange.miss", () -> new AtomicInteger(1).compareAndExchange(9, 2));

        // ---- AtomicLong ----------------------------------------------------
        t("al.default.get", () -> new AtomicLong().get());
        t("al.overflow", () -> new AtomicLong(Long.MAX_VALUE).incrementAndGet());
        t("al.getAndAdd", () -> new AtomicLong(1).getAndAdd(Long.MAX_VALUE));
        t("al.cas.hit", () -> new AtomicLong(1).compareAndSet(1, 2));
        t("al.updateAndGet.null", () -> new AtomicLong(1).updateAndGet(null));
        t("al.toString", () -> new AtomicLong(7).toString());

        // ---- AtomicBoolean -------------------------------------------------
        t("ab.default", () -> new AtomicBoolean().get());
        t("ab.getAndSet", () -> new AtomicBoolean(false).getAndSet(true));
        t("ab.cas.miss", () -> new AtomicBoolean(false).compareAndSet(true, false));
        t("ab.toString", () -> new AtomicBoolean(true).toString());

        // ---- AtomicReference -----------------------------------------------
        t("ar.default", () -> new AtomicReference<String>().get());
        t("ar.cas.nullExpected", () -> new AtomicReference<String>().compareAndSet(null, "x"));
        t("ar.cas.identity", () -> { String a = new String("k"); String b = new String("k");
            AtomicReference<String> r = new AtomicReference<>(a); return r.compareAndSet(b, "z"); });
        t("ar.getAndUpdate.null", () -> new AtomicReference<String>("a").getAndUpdate(null));
        t("ar.updateAndGet", () -> new AtomicReference<>("a").updateAndGet(s -> s + "b"));
        t("ar.toString.null", () -> new AtomicReference<String>().toString());

        // ---- AtomicIntegerArray --------------------------------------------
        t("aia.ctor(-1)", () -> new AtomicIntegerArray(-1));
        t("aia.ctor(null)", () -> new AtomicIntegerArray(null));
        t("aia.length", () -> new AtomicIntegerArray(3).length());
        t("aia.get(-1)", () -> new AtomicIntegerArray(3).get(-1));
        t("aia.get(3)", () -> new AtomicIntegerArray(3).get(3));
        t("aia.set(3,1)", () -> { new AtomicIntegerArray(3).set(3, 1); return "no-throw"; });
        t("aia.cas(-1)", () -> new AtomicIntegerArray(3).compareAndSet(-1, 0, 1));
        t("aia.getAndIncrement(0)", () -> new AtomicIntegerArray(3).getAndIncrement(0));
        t("aia.fromArray", () -> new AtomicIntegerArray(new int[] {4, 5, 6}).get(1));
        t("aia.toString", () -> new AtomicIntegerArray(new int[] {1, 2}).toString());
        t("aia.updateAndGet.null", () -> new AtomicIntegerArray(2).updateAndGet(0, null));

        // ---- AtomicLongArray / AtomicReferenceArray ------------------------
        t("ala.get(5)", () -> new AtomicLongArray(2).get(5));
        t("ala.fromArray", () -> new AtomicLongArray(new long[] {7L, 8L}).get(0));
        t("ara.ctor(null)", () -> new AtomicReferenceArray<String>(null));
        t("ara.get(-1)", () -> new AtomicReferenceArray<String>(2).get(-1));
        t("ara.default", () -> new AtomicReferenceArray<String>(2).get(0));
        t("ara.fromArray", () -> new AtomicReferenceArray<>(new String[] {"a", "b"}).get(1));
        t("ara.cas", () -> new AtomicReferenceArray<String>(2).compareAndSet(0, null, "x"));
        t("ara.toString", () -> new AtomicReferenceArray<>(new String[] {"a"}).toString());

        // ---- Field updaters -------------------------------------------------
        t("aifu.volatileInt", () -> AtomicIntegerFieldUpdater
                .newUpdater(Holder.class, "vi").get(new Holder()));
        t("aifu.nonVolatile", () -> AtomicIntegerFieldUpdater.newUpdater(Holder.class, "plain"));
        t("aifu.missingField", () -> AtomicIntegerFieldUpdater.newUpdater(Holder.class, "nope"));
        t("aifu.wrongType", () -> AtomicIntegerFieldUpdater.newUpdater(Holder.class, "vl"));
        t("aifu.staticField", () -> AtomicIntegerFieldUpdater.newUpdater(Holder.class, "svi"));
        t("aifu.nullClass", () -> AtomicIntegerFieldUpdater.newUpdater(null, "vi"));
        // WHICH CHECK FIRES FIRST. `plainLong` is non-volatile AND the wrong
        // type, so the message names the check the JDK reaches first — the one
        // thing a single-fault row cannot tell you.
        t("aifu.wrongType+nonVol", () -> AtomicIntegerFieldUpdater.newUpdater(Holder.class, "plainLong"));
        t("aifu.staticNonVol", () -> AtomicIntegerFieldUpdater.newUpdater(Holder.class, "splain"));
        t("aifu.nullFieldName", () -> AtomicIntegerFieldUpdater.newUpdater(Holder.class, null));
        t("aifu.incrementAndGet", () -> AtomicIntegerFieldUpdater
                .newUpdater(Holder.class, "vi").incrementAndGet(new Holder()));
        t("aifu.cas", () -> AtomicIntegerFieldUpdater
                .newUpdater(Holder.class, "vi").compareAndSet(new Holder(), 1, 5));
        t("alfu.volatileLong", () -> AtomicLongFieldUpdater
                .newUpdater(Holder.class, "vl").get(new Holder()));
        t("alfu.wrongType", () -> AtomicLongFieldUpdater.newUpdater(Holder.class, "vi"));
        t("arfu.volatileRef", () -> AtomicReferenceFieldUpdater
                .newUpdater(Holder.class, String.class, "vs").get(new Holder()));
        t("arfu.wrongVType", () -> AtomicReferenceFieldUpdater
                .newUpdater(Holder.class, Integer.class, "vs"));

        // ---- LongAdder / DoubleAdder / accumulators -------------------------
        t("la.default.sum", () -> new LongAdder().sum());
        t("la.increment.sum", () -> { LongAdder x = new LongAdder(); x.increment(); x.add(4); return x.sum(); });
        t("la.sumThenReset", () -> { LongAdder x = new LongAdder(); x.add(3); long s = x.sumThenReset(); return s + ":" + x.sum(); });
        t("la.intValue", () -> { LongAdder x = new LongAdder(); x.add(9); return x.intValue(); });
        t("la.toString", () -> { LongAdder x = new LongAdder(); x.add(2); return x.toString(); });
        t("da.default.sum", () -> new DoubleAdder().sum());
        t("da.add.sum", () -> { DoubleAdder d = new DoubleAdder(); d.add(1.5); d.add(2.25); return d.sum(); });
        t("lacc.null.fn", () -> new LongAccumulator(null, 0L));
        t("lacc.max", () -> { LongAccumulator x = new LongAccumulator(Long::max, 0L);
            x.accumulate(5); x.accumulate(2); return x.get(); });
        t("lacc.thenReset", () -> { LongAccumulator x = new LongAccumulator(Long::max, 0L);
            x.accumulate(5); long v = x.getThenReset(); return v + ":" + x.get(); });

        // ---- AtomicStampedReference / AtomicMarkableReference ---------------
        t("asr.getStamp", () -> new AtomicStampedReference<>("a", 3).getStamp());
        t("asr.cas.wrongStamp", () -> new AtomicStampedReference<>("a", 3)
                .compareAndSet("a", "b", 9, 10));
        t("asr.attemptStamp", () -> new AtomicStampedReference<>("a", 3).attemptStamp("a", 4));
        t("amr.isMarked", () -> new AtomicMarkableReference<>("a", true).isMarked());
        t("amr.cas", () -> new AtomicMarkableReference<>("a", false)
                .compareAndSet("a", "b", false, true));

        System.out.println("SWEEP-DONE");
    }
}
