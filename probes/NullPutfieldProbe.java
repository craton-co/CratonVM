/**
 * Does a `putfield` on a null receiver throw NPE in a COMPILED method that has
 * no exception table at all?
 *
 * Rbc6FieldProbe showed a putfield-on-null silently doing nothing inside a
 * protected range. That was masked by keeping such methods interpreted, but the
 * store itself is lowered the same way in a method with no try/catch — so check
 * that path directly. The catch is in the CALLER, so the compiled callee has an
 * empty exception table and RBC.6 never applies to it.
 */
public final class NullPutfieldProbe {

    static final class Holder {
        int value = 1;
        Object ref = "x";
        long wide = 2L;
    }

    // No try/catch anywhere in these -- empty exception table.
    static void storeInt(Holder h, int v) {
        h.value = v;
    }

    static void storeRef(Holder h, Object v) {
        h.ref = v;
    }

    static void storeLong(Holder h, long v) {
        h.wide = v;
    }

    static int loadInt(Holder h) {
        return h.value;
    }

    private static String attempt(String name, Runnable r) {
        try {
            r.run();
            return name + "=NO-THROW";
        } catch (NullPointerException e) {
            return name + "=NPE";
        }
    }

    public static void main(String[] args) {
        int iterations = args.length == 0 ? 400_000 : Integer.parseInt(args[0]);
        Holder live = new Holder();

        // Warm each callee well past any compile threshold on a non-null
        // receiver, so the null call below hits compiled code.
        long sink = 0;
        for (int i = 0; i < iterations; i++) {
            storeInt(live, i);
            storeRef(live, live);
            storeLong(live, i);
            sink += loadInt(live);
        }
        System.out.println("warm sink=" + sink);

        System.out.println(attempt("putfield-int ", () -> storeInt(null, 5)));
        System.out.println(attempt("putfield-ref ", () -> storeRef(null, "y")));
        System.out.println(attempt("putfield-long", () -> storeLong(null, 9L)));
        System.out.println(attempt("getfield-int ", () -> loadInt(null)));
        System.out.println("(all four must be NPE)");
    }
}
