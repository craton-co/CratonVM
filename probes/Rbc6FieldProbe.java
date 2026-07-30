/**
 * Does RBC.6's exemption of getfield/putfield (0xb4/0xb5) preserve handler
 * locals?
 *
 * Each probe method has the exact shape RBC.6 exists to protect:
 *   - a protected range containing a FIELD ACCESS on a possibly-null receiver,
 *   - a local written INSIDE the try that is NOT a parameter slot,
 *   - a handler that READS that local.
 *
 * If the field access throws NPE with only a params-only reconstructed frame,
 * the handler observes 0 instead of the value the try body stored. That is a
 * silent wrong answer, not a crash — so we compare against HotSpot and against
 * CratonVM --nojit rather than looking for a fault.
 */
public final class Rbc6FieldProbe {

    static final class Holder {
        int value = 7;
        Object ref = "r";
        long wide = 0x1234_5678_9ABCL;
    }

    /** getfield (0xb4) on a null receiver; handler reads a non-parameter local. */
    static int getfieldIntHandlerLocal(Holder h, int n) {
        int scratch = 0;
        try {
            scratch = n * 7 + 3;   // non-parameter local, written inside the try
            return h.value;        // 0xb4 — NPE when h == null
        } catch (NullPointerException e) {
            return scratch;        // handler READS it
        }
    }

    /** getfield of a reference field, same shape. */
    static int getfieldRefHandlerLocal(Holder h, int n) {
        int scratch = 0;
        Object seen = null;
        try {
            scratch = n * 11 + 5;
            seen = h.ref;          // 0xb4, reference-typed
            return scratch + seen.hashCode();
        } catch (NullPointerException e) {
            return scratch + (seen == null ? 0 : 1);
        }
    }

    /** getfield of a category-2 field. */
    static long getfieldLongHandlerLocal(Holder h, int n) {
        long scratch = 0;
        try {
            scratch = n * 1_000_003L;
            return h.wide;         // 0xb4, long
        } catch (NullPointerException e) {
            return scratch;
        }
    }

    /** putfield (0xb5) on a null receiver; handler reads a non-parameter local. */
    static int putfieldHandlerLocal(Holder h, int n) {
        int scratch = 0;
        try {
            scratch = n * 13 + 1;
            h.value = n;           // 0xb5 — NPE when h == null
            return -1;
        } catch (NullPointerException e) {
            return scratch;
        }
    }

    /** Two locals, so a partial reconstruction shows up as a mismatch. */
    static int twoLocalsHandler(Holder h, int n) {
        int a = 0;
        int b = 0;
        try {
            a = n + 100;
            b = n + 200;
            return h.value;
        } catch (NullPointerException e) {
            return a * 1000 + b;
        }
    }

    public static void main(String[] args) {
        int iterations = args.length == 0 ? 200_000 : Integer.parseInt(args[0]);
        Holder live = new Holder();
        long acc = 0;

        for (int i = 0; i < iterations; i++) {
            // Mostly the non-throwing path so the method gets hot and compiled,
            // then a null receiver on a fraction of iterations to take the
            // handler with the compiled frame live.
            Holder h = (i % 8 == 0) ? null : live;
            acc += getfieldIntHandlerLocal(h, i);
            acc += getfieldRefHandlerLocal(h, i);
            acc += getfieldLongHandlerLocal(h, i);
            acc += putfieldHandlerLocal(h, i);
            acc += twoLocalsHandler(h, i);
        }

        // Spot values on the throwing path, printed so a wrong local is visible
        // as a number rather than only as a checksum drift.
        System.out.println("acc=" + acc);
        System.out.println("getfieldInt(null,5)=" + getfieldIntHandlerLocal(null, 5) + " expect=38");
        System.out.println("getfieldRef(null,5)=" + getfieldRefHandlerLocal(null, 5) + " expect=60");
        System.out.println("getfieldLong(null,5)=" + getfieldLongHandlerLocal(null, 5) + " expect=5000015");
        System.out.println("putfield(null,5)=" + putfieldHandlerLocal(null, 5) + " expect=66");
        System.out.println("twoLocals(null,5)=" + twoLocalsHandler(null, 5) + " expect=105205");
    }
}
