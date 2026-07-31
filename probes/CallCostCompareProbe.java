import java.util.Calendar;
import java.util.GregorianCalendar;
import java.util.Locale;
import java.util.TimeZone;

/**
 * Is `Calendar.get`'s 10 us a DISPATCH cost or something specific to Calendar?
 *
 * Known-issue 30.A's chain bottoms out at `Calendar.get(SECOND)` costing ~10 us
 * on an already-computed calendar — a `complete()` that should short-circuit
 * plus a `fields[i]` array read. This VM's own probe table puts a compiled
 * `invokevirtual` at 56 ns, so 10 us is ~180x more than dispatch explains.
 *
 * These rows are all measured in ONE process so they share JIT state and host
 * load. The user-defined rows mimic Calendar's shape exactly: a virtual call
 * into a method that reads a boolean guard and then indexes an int[].
 */
public final class CallCostCompareProbe {

    static class Shape {
        boolean ready;
        final int[] fields = new int[17];
        Shape() { for (int i = 0; i < fields.length; i++) { fields[i] = i * 3; } ready = true; }
        void complete() { if (!ready) { recompute(); } }
        void recompute() { ready = true; }
        final int internalGet(int f) { return fields[f]; }
        public int get(int f) { complete(); return internalGet(f); }
    }

    /** Same shape, but subclassed so the call site is genuinely polymorphic. */
    static class Sub extends Shape {
        @Override public int get(int f) { complete(); return internalGet(f) + 0; }
    }

    private static Shape SHAPE;
    private static Shape SHAPE_POLY;
    private static Calendar CAL;
    private static int[] RAW = new int[17];
    private static long sink;

    private static void rawArrayRead(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) { s += RAW[Calendar.SECOND]; }
        sink += s;
    }

    private static void userVirtualGet(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) { s += SHAPE.get(Calendar.SECOND); }
        sink += s;
    }

    private static void userPolyVirtualGet(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) { s += SHAPE_POLY.get(Calendar.SECOND); }
        sink += s;
    }

    private static void calendarGet(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) { s += CAL.get(Calendar.SECOND); }
        sink += s;
    }

    /** Calendar.getTimeZone — a plain field-returning virtual on the same object. */
    private static void calendarGetTimeZone(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) { s += CAL.getTimeZone() == null ? 0 : 1; }
        sink += s;
    }

    /** Calendar.isLenient — a plain boolean field read. */
    private static void calendarIsLenient(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) { s += CAL.isLenient() ? 1 : 0; }
        sink += s;
    }

    public static void main(String[] args) {
        int blocks = args.length > 0 ? Integer.parseInt(args[0]) : 3;
        int bs = args.length > 1 ? Integer.parseInt(args[1]) : 50_000;

        SHAPE = new Shape();
        SHAPE_POLY = new Sub();
        for (int i = 0; i < RAW.length; i++) { RAW[i] = i * 3; }
        CAL = new GregorianCalendar(TimeZone.getDefault(), Locale.US);
        CAL.setTimeInMillis(1_700_000_000_000L);
        CAL.get(Calendar.SECOND);

        System.out.printf("%-26s", "stage (ns/op by block)");
        for (int b = 0; b < blocks; b++) { System.out.printf("%10d", b); }
        System.out.println();

        String[] names = {"rawArrayRead", "userVirtualGet", "userPolyVirtualGet",
                "calendarIsLenient", "calendarGetTimeZone", "calendarGet"};
        for (int k = 0; k < names.length; k++) {
            StringBuilder out = new StringBuilder(String.format("%-26s", names[k]));
            for (int b = 0; b < blocks; b++) {
                long t0 = System.nanoTime();
                switch (k) {
                    case 0: rawArrayRead(bs); break;
                    case 1: userVirtualGet(bs); break;
                    case 2: userPolyVirtualGet(bs); break;
                    case 3: calendarIsLenient(bs); break;
                    case 4: calendarGetTimeZone(bs); break;
                    default: calendarGet(bs); break;
                }
                out.append(String.format("%10d", (System.nanoTime() - t0) / bs));
            }
            System.out.println(out);
        }
        System.out.println("sink=" + sink);
    }
}
