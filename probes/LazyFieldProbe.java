/**
 * Does a `putfield` inside `synchronized (this)` inside a `try/finally` stick?
 *
 * `FileChannelImpl.fileLockTable()` is exactly that shape:
 *
 *   private volatile FileLockTable fileLockTable;
 *   if (fileLockTable == null) {
 *       synchronized (this) {
 *           if (fileLockTable == null) {
 *               int ti = threads.add();
 *               try { ensureOpen(); fileLockTable = new FileLockTable(this, fd); }
 *               finally { threads.remove(ti); }
 *           }
 *       }
 *   }
 *   return fileLockTable;
 *
 * and it returns null on CratonVM. This strips everything NIO-specific away so
 * the answer cannot be about file descriptors: same field kind, same nesting,
 * same re-read at the end. Each case removes one ingredient.
 */
public class LazyFieldProbe {

    static final class Holder {
        final int id;
        Holder(int id) { this.id = id; }
        @Override public String toString() { return "Holder#" + id; }
    }

    // --- case 1: the exact shape ------------------------------------------
    static final class Exact {
        private volatile Holder h;
        private int counter;
        Holder get() {
            if (h == null) {
                synchronized (this) {
                    if (h == null) {
                        int ti = counter++;
                        try {
                            h = new Holder(1);
                        } finally {
                            counter -= (counter - ti) - 1;
                        }
                    }
                }
            }
            return h;
        }
    }

    // --- case 2: volatile + synchronized, no try/finally --------------------
    static final class NoFinally {
        private volatile Holder h;
        Holder get() {
            if (h == null) {
                synchronized (this) {
                    if (h == null) {
                        h = new Holder(2);
                    }
                }
            }
            return h;
        }
    }

    // --- case 3: volatile + try/finally, no synchronized --------------------
    static final class NoSync {
        private volatile Holder h;
        Holder get() {
            if (h == null) {
                try {
                    h = new Holder(3);
                } finally {
                    // nothing
                }
            }
            return h;
        }
    }

    // --- case 4: plain (non-volatile) field, everything else the same -------
    static final class NotVolatile {
        private Holder h;
        Holder get() {
            if (h == null) {
                synchronized (this) {
                    if (h == null) {
                        try {
                            h = new Holder(4);
                        } finally {
                            // nothing
                        }
                    }
                }
            }
            return h;
        }
    }

    // --- case 5: the field is preceded by many others, so it sits late in
    //     the layout — a field-offset problem shows up here and not above.
    static final class LateField {
        private long a, b, c, d, e, f, g, h1;
        private Object o1, o2, o3, o4, o5, o6, o7, o8;
        private volatile Holder h;
        Holder get() {
            if (h == null) {
                synchronized (this) {
                    if (h == null) {
                        h = new Holder(5);
                    }
                }
            }
            return h;
        }
        long sum() { return a + b + c + d + e + f + g + h1; }
        Object refs() { return "" + o1 + o2 + o3 + o4 + o5 + o6 + o7 + o8; }
    }

    public static void main(String... args) {
        check("1 volatile + synchronized + try/finally (the exact shape)",
                () -> new Exact().get());
        check("2 volatile + synchronized, no try/finally", () -> new NoFinally().get());
        check("3 volatile + try/finally, no synchronized", () -> new NoSync().get());
        check("4 non-volatile + synchronized + try/finally", () -> new NotVolatile().get());
        check("5 the field sits after 16 others", () -> new LateField().get());

        // Repeat case 1 many times: a lazy init that only sometimes sticks is a
        // different bug from one that never does.
        int nulls = 0;
        for (int i = 0; i < 10_000; i++) {
            if (new Exact().get() == null) {
                nulls++;
            }
        }
        System.out.println((nulls == 0 ? "OK   " : "FAIL ")
                + "case 1 repeated 10000x: " + nulls + " null(s)");
        System.out.println("=== DONE");
    }

    private interface Get {
        Holder run();
    }

    private static void check(String label, Get g) {
        try {
            Holder h = g.run();
            System.out.println((h == null ? "FAIL " : "OK   ") + label + " -> " + h);
        } catch (Throwable t) {
            System.out.println("FAIL " + label + " -> " + t);
        }
        System.out.flush();
    }
}
