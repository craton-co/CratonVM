/**
 * Which of `Rbc6FieldProbe`'s five rungs lets a NullPointerException escape?
 *
 * The probe it bisects reports the failure as one uncaught NPE at `main`, which
 * names the loop and not the rung. Each rung here is called through its own
 * `try`/`catch (Throwable)`, so an escape is attributed rather than fatal, and
 * the surviving rungs still run — a bisect that stops at the first failure
 * cannot tell one broken rung from five.
 *
 * The bodies are copied verbatim from `Rbc6FieldProbe` (same locals, same
 * order, same protected ranges) so the compiled shape is the one under test.
 */
public final class Rbc6FieldBisect {

    static final class Holder {
        int value = 7;
        Object ref = "r";
        long wide = 0x1234_5678_9ABCL;
    }

    static int getfieldIntHandlerLocal(Holder h, int n) {
        int scratch = 0;
        try {
            scratch = n * 7 + 3;
            return h.value;
        } catch (NullPointerException e) {
            return scratch;
        }
    }

    static int getfieldRefHandlerLocal(Holder h, int n) {
        int scratch = 0;
        Object seen = null;
        try {
            scratch = n * 11 + 5;
            seen = h.ref;
            return scratch + seen.hashCode();
        } catch (NullPointerException e) {
            return scratch + (seen == null ? 0 : 1);
        }
    }

    static long getfieldLongHandlerLocal(Holder h, int n) {
        long scratch = 0;
        try {
            scratch = n * 1_000_003L;
            return h.wide;
        } catch (NullPointerException e) {
            return scratch;
        }
    }

    static int putfieldHandlerLocal(Holder h, int n) {
        int scratch = 0;
        try {
            scratch = n * 13 + 1;
            h.value = n;
            return -1;
        } catch (NullPointerException e) {
            return scratch;
        }
    }

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

    static final String[] NAMES = {
        "getfieldIntHandlerLocal", "getfieldRefHandlerLocal", "getfieldLongHandlerLocal",
        "putfieldHandlerLocal", "twoLocalsHandler",
    };

    static long call(int rung, Holder h, int i) {
        switch (rung) {
            case 0: return getfieldIntHandlerLocal(h, i);
            case 1: return getfieldRefHandlerLocal(h, i);
            case 2: return getfieldLongHandlerLocal(h, i);
            case 3: return putfieldHandlerLocal(h, i);
            default: return twoLocalsHandler(h, i);
        }
    }

    public static void main(String[] args) {
        int iterations = args.length == 0 ? 200_000 : Integer.parseInt(args[0]);
        Holder live = new Holder();
        long[] acc = new long[NAMES.length];
        int[] escapes = new int[NAMES.length];
        int[] firstEscapeIter = new int[NAMES.length];
        java.util.Arrays.fill(firstEscapeIter, -1);

        for (int i = 0; i < iterations; i++) {
            Holder h = (i % 8 == 0) ? null : live;
            for (int r = 0; r < NAMES.length; r++) {
                try {
                    acc[r] += call(r, h, i);
                } catch (Throwable t) {
                    escapes[r]++;
                    if (firstEscapeIter[r] < 0) {
                        firstEscapeIter[r] = i;
                        System.out.println("ESCAPE rung=" + NAMES[r] + " iter=" + i
                                + " receiver=" + (h == null ? "null" : "live")
                                + " threw=" + t.getClass().getName());
                    }
                }
            }
        }
        for (int r = 0; r < NAMES.length; r++) {
            System.out.println(NAMES[r] + " acc=" + acc[r] + " escapes=" + escapes[r]
                    + " firstEscapeIter=" + firstEscapeIter[r]);
        }
    }
}
