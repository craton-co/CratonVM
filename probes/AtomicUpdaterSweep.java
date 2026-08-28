import java.util.concurrent.atomic.*;

/** The three `Atomic*FieldUpdater` families, aimed by the survey's own prior.
 *
 *  `native-builtins/src/atomic_updater.rs` registers 51 natives, and several of
 *  them go on the ABSTRACT BASE as well as the concrete impl:
 *
 *      // getAndIncrement / getAndDecrement / addAndGet on both impl + base.
 *      for cls in [CLS_INT_FIELD_UPDATER_IMPL, CLS_INT_FIELD_UPDATER] { .. }
 *
 *  `AtomicIntegerFieldUpdater` is a PUBLIC ABSTRACT class with a protected
 *  constructor, so an application may extend it and supply its own `get`/`set`
 *  /`compareAndSet`. A native registered on the base runs in front of that
 *  subclass's inherited bodies -- and the JDK's own base-class
 *  `getAndIncrement` is written in terms of `get`/`compareAndSet`, so it MUST
 *  dispatch back into the subclass. That is the shape recorded at
 *  `a-base-class-native-shadows-the-overloads-a-provider-subclass-does-not`,
 *  and this probe asks it directly: a counting subclass whose own `get`/`set`
 *  must be entered by every base-class default method.
 *
 *  DETERMINISM: single-threaded throughout. These updaters are lock-based in
 *  this VM rather than hardware-atomic, so a contended probe would measure the
 *  lock and not the semantics; every question here is about what one thread
 *  observes, which is a contract, not a race.
 */
public class AtomicUpdaterSweep {
    static String esc(String s) {
        StringBuilder b = new StringBuilder(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7e) b.append(String.format("\\u%04x", (int) c));
            else b.append(c);
        }
        return b.toString();
    }
    static void p(String tag, Object v) {
        System.out.println(esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }
    static void t(String tag, ThrowingRun r) {
        try { r.run(); p(tag, "no-throw"); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }
    interface ThrowingRun { void run() throws Throwable; }

    public static class Holder {
        public volatile int i = 10;
        public volatile long l = 10L;
        public volatile String s = "a";
        public volatile Integer boxed = 1;
        public int plainInt = 5;              // NOT volatile -> newUpdater must refuse
        public static volatile int stat = 1;  // static -> newUpdater must refuse
        public volatile double d = 1.0;       // wrong type for an int updater
    }

    /** A user subclass of the ABSTRACT base, counting every entry into its own
     *  bodies. The JDK's base-class `getAndIncrement`, `addAndGet`,
     *  `getAndAdd`, `getAndSet` and `updateAndGet` are all written in terms of
     *  `get` and `compareAndSet`, so each must raise these counters. A native
     *  registered on the base short-circuits them and the counters stay 0. */
    public static class CountingUpdater extends AtomicIntegerFieldUpdater<Holder> {
        int gets, sets, cas;
        int value = 100;
        public boolean compareAndSet(Holder obj, int expect, int update) {
            cas++;
            if (value != expect) return false;
            value = update;
            return true;
        }
        public boolean weakCompareAndSet(Holder obj, int expect, int update) {
            return compareAndSet(obj, expect, update);
        }
        public void set(Holder obj, int newValue) { sets++; value = newValue; }
        public void lazySet(Holder obj, int newValue) { set(obj, newValue); }
        public int get(Holder obj) { gets++; return value; }
        String counters() { return "gets=" + gets + " sets=" + sets + " cas=" + cas; }
    }

    static void subclassDispatch() {
        Holder h = new Holder();
        CountingUpdater u = new CountingUpdater();
        p("[subclass] initial get", u.get(h));
        p("[subclass] after get", u.counters());

        CountingUpdater a = new CountingUpdater();
        p("[subclass] getAndIncrement result", a.getAndIncrement(h));
        p("[subclass] getAndIncrement value", a.value);
        // THE ROW: the base-class default must have gone through get/compareAndSet.
        p("[subclass] getAndIncrement entered subclass", a.gets > 0 && a.cas > 0);
        p("[subclass] getAndIncrement counters", a.counters());

        CountingUpdater b = new CountingUpdater();
        p("[subclass] getAndDecrement result", b.getAndDecrement(h));
        p("[subclass] getAndDecrement entered subclass", b.gets > 0 && b.cas > 0);

        CountingUpdater c = new CountingUpdater();
        p("[subclass] addAndGet result", c.addAndGet(h, 5));
        p("[subclass] addAndGet entered subclass", c.gets > 0 && c.cas > 0);

        CountingUpdater d = new CountingUpdater();
        p("[subclass] getAndAdd result", d.getAndAdd(h, 7));
        p("[subclass] getAndAdd entered subclass", d.gets > 0 && d.cas > 0);

        CountingUpdater e = new CountingUpdater();
        p("[subclass] incrementAndGet result", e.incrementAndGet(h));
        p("[subclass] incrementAndGet entered subclass", e.gets > 0 && e.cas > 0);

        CountingUpdater f = new CountingUpdater();
        p("[subclass] decrementAndGet result", f.decrementAndGet(h));
        p("[subclass] decrementAndGet entered subclass", f.gets > 0 && f.cas > 0);

        CountingUpdater g = new CountingUpdater();
        p("[subclass] getAndSet result", g.getAndSet(h, 55));
        p("[subclass] getAndSet value", g.value);
        p("[subclass] getAndSet entered subclass", g.gets > 0);

        CountingUpdater k = new CountingUpdater();
        p("[subclass] updateAndGet result", k.updateAndGet(h, x -> x * 2));
        p("[subclass] updateAndGet entered subclass", k.gets > 0 && k.cas > 0);

        CountingUpdater m = new CountingUpdater();
        p("[subclass] accumulateAndGet result", m.accumulateAndGet(h, 3, Integer::sum));
        p("[subclass] accumulateAndGet entered subclass", m.gets > 0 && m.cas > 0);

        // The subclass's OWN state must be what everything read -- if a native
        // read Holder.i instead, these would answer 10-flavoured numbers.
        CountingUpdater n = new CountingUpdater();
        n.getAndIncrement(h);
        p("[subclass] holder untouched", h.i);
        p("[subclass] subclass value moved", n.value);
    }

    static void intUpdater() {
        AtomicIntegerFieldUpdater<Holder> u =
            AtomicIntegerFieldUpdater.newUpdater(Holder.class, "i");
        Holder h = new Holder();
        p("[int] get", u.get(h));
        u.set(h, 20);
        p("[int] after set", u.get(h));
        p("[int] field agrees", h.i);
        u.lazySet(h, 21);
        p("[int] after lazySet", u.get(h));
        p("[int] getAndSet", u.getAndSet(h, 30));
        p("[int] after getAndSet", u.get(h));
        p("[int] cas true", u.compareAndSet(h, 30, 31));
        p("[int] cas false", u.compareAndSet(h, 999, 40));
        p("[int] after cas", u.get(h));
        p("[int] weakCas", u.weakCompareAndSet(h, 31, 32));
        p("[int] getAndIncrement", u.getAndIncrement(h));
        p("[int] getAndDecrement", u.getAndDecrement(h));
        p("[int] incrementAndGet", u.incrementAndGet(h));
        p("[int] decrementAndGet", u.decrementAndGet(h));
        p("[int] getAndAdd", u.getAndAdd(h, 100));
        p("[int] addAndGet", u.addAndGet(h, -100));
        p("[int] updateAndGet", u.updateAndGet(h, x -> x * 3));
        p("[int] getAndUpdate", u.getAndUpdate(h, x -> x + 1));
        p("[int] accumulateAndGet", u.accumulateAndGet(h, 10, Integer::sum));
        p("[int] getAndAccumulate", u.getAndAccumulate(h, 10, (x, y) -> x - y));
        p("[int] final", u.get(h));
        p("[int] final field", h.i);
        t("[int] null target get", () -> u.get(null));
        t("[int] null target set", () -> u.set(null, 1));
    }

    static void longUpdater() {
        AtomicLongFieldUpdater<Holder> u =
            AtomicLongFieldUpdater.newUpdater(Holder.class, "l");
        Holder h = new Holder();
        p("[long] get", u.get(h));
        u.set(h, 20L);
        p("[long] after set", u.get(h));
        p("[long] getAndSet", u.getAndSet(h, 30L));
        p("[long] cas true", u.compareAndSet(h, 30L, 31L));
        p("[long] cas false", u.compareAndSet(h, 999L, 40L));
        p("[long] getAndIncrement", u.getAndIncrement(h));
        p("[long] addAndGet big", u.addAndGet(h, 1L << 40));
        p("[long] getAndAdd negative", u.getAndAdd(h, -(1L << 40)));
        p("[long] updateAndGet", u.updateAndGet(h, x -> x * 2));
        p("[long] accumulateAndGet", u.accumulateAndGet(h, 5L, Long::sum));
        p("[long] final", u.get(h));
        p("[long] final field", h.l);
    }

    static void refUpdater() {
        AtomicReferenceFieldUpdater<Holder, String> u =
            AtomicReferenceFieldUpdater.newUpdater(Holder.class, String.class, "s");
        Holder h = new Holder();
        p("[ref] get", u.get(h));
        u.set(h, "b");
        p("[ref] after set", u.get(h));
        p("[ref] field agrees", h.s);
        p("[ref] getAndSet", u.getAndSet(h, "c"));
        p("[ref] cas true", u.compareAndSet(h, "c", "d"));
        // compareAndSet is by REFERENCE identity, not equals: a fresh String
        // equal to the current value must NOT match.
        p("[ref] cas by identity", u.compareAndSet(h, new String("d"), "e"));
        p("[ref] after identity cas", u.get(h));
        p("[ref] cas to null", u.compareAndSet(h, u.get(h), null));
        p("[ref] after null", u.get(h));
        p("[ref] cas from null", u.compareAndSet(h, null, "f"));
        p("[ref] updateAndGet", u.updateAndGet(h, x -> x + "!"));
        p("[ref] accumulateAndGet", u.accumulateAndGet(h, "z", (x, y) -> x + y));
        p("[ref] final", u.get(h));
    }

    static void refusals() {
        t("newUpdater non-volatile", () ->
            AtomicIntegerFieldUpdater.newUpdater(Holder.class, "plainInt"));
        t("newUpdater static", () ->
            AtomicIntegerFieldUpdater.newUpdater(Holder.class, "stat"));
        t("newUpdater missing field", () ->
            AtomicIntegerFieldUpdater.newUpdater(Holder.class, "nope"));
        t("newUpdater wrong type", () ->
            AtomicIntegerFieldUpdater.newUpdater(Holder.class, "d"));
        t("newUpdater int on long field", () ->
            AtomicIntegerFieldUpdater.newUpdater(Holder.class, "l"));
        t("newUpdater long on int field", () ->
            AtomicLongFieldUpdater.newUpdater(Holder.class, "i"));
        t("newUpdater ref on int field", () ->
            AtomicReferenceFieldUpdater.newUpdater(Holder.class, Integer.class, "i"));
        t("newUpdater ref wrong class", () ->
            AtomicReferenceFieldUpdater.newUpdater(Holder.class, Integer.class, "s"));
        t("newUpdater null class", () ->
            AtomicIntegerFieldUpdater.newUpdater(null, "i"));
        t("newUpdater null name", () ->
            AtomicIntegerFieldUpdater.newUpdater(Holder.class, null));
        // A boxed Integer field is a REFERENCE, so an int updater must refuse.
        t("newUpdater int on boxed", () ->
            AtomicIntegerFieldUpdater.newUpdater(Holder.class, "boxed"));
        // The wrong receiver TYPE is a ClassCastException at use, not at build.
        AtomicIntegerFieldUpdater<Holder> u =
            AtomicIntegerFieldUpdater.newUpdater(Holder.class, "i");
        t("wrong receiver type", () -> {
            @SuppressWarnings("unchecked")
            AtomicIntegerFieldUpdater<Object> raw = (AtomicIntegerFieldUpdater<Object>) (Object) u;
            raw.get(new Object());
        });
    }

    public static void main(String[] a) {
        intUpdater();
        longUpdater();
        refUpdater();
        refusals();
        subclassDispatch();
        System.out.println("DONE AtomicUpdaterSweep");
    }
}
