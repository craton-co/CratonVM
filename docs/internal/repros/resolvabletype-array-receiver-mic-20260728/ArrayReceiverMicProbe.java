/**
 * Minimal reproducer for the inline monomorphic/polymorphic inline-cache guard
 * comparing only the 4-byte class id at ObjectHeader+0.
 *
 * A reference array stores its COMPONENT class id in that same word, so once a
 * virtual call site has cached "receiver class == Foo -> Foo.equals", a later
 * `Foo[]` receiver passes the guard and is dispatched into `Foo.equals`, whose
 * `checkcast Foo` on the argument then throws
 *   ClassCastException: class [LFoo; cannot be cast to class Foo
 *
 * Arrays do not override equals(): `a1.equals(a2)` on two distinct arrays must
 * be identity-false. Reports a MISMATCH COUNT (0 == clean).
 */
public class ArrayReceiverMicProbe {

    static class Foo {
        final int v;
        Foo(int v) { this.v = v; }
        @Override public boolean equals(Object o) {
            if (this == o) return true;
            if (o == null || o.getClass() != getClass()) return false;
            Foo f = (Foo) o;
            return f.v == v;
        }
        @Override public int hashCode() { return v; }
    }

    /** The polymorphic call site: `a.equals(b)` with a declared type of Object. */
    static boolean cmp(Object a, Object b) {
        return a.equals(b);
    }

    public static void main(String[] args) {
        int warm = args.length > 0 ? Integer.parseInt(args[0]) : 50000;
        int probe = args.length > 1 ? Integer.parseInt(args[1]) : 5000;

        Foo f1 = new Foo(1);
        Foo f2 = new Foo(1);
        Foo f3 = new Foo(2);

        // Warm the call site to a monomorphic Foo receiver so the inline cache
        // caches (class id of Foo -> Foo.equals).
        int acc = 0;
        for (int i = 0; i < warm; i++) {
            if (cmp(f1, f2)) acc++;
            if (cmp(f1, f3)) acc++;
        }

        // Now hand the SAME call site two distinct Foo[] receivers.
        Foo[] a1 = new Foo[] { f1 };
        Foo[] a2 = new Foo[] { f1 };
        long wrongTrue = 0, thrown = 0;
        String firstThrow = null;
        for (int i = 0; i < probe; i++) {
            try {
                if (cmp(a1, a2)) {
                    wrongTrue++;
                }
                if (!cmp(a1, a1)) {
                    wrongTrue++;
                }
            } catch (Throwable t) {
                thrown++;
                if (firstThrow == null) {
                    firstThrow = t.toString();
                }
            }
        }
        if (firstThrow != null) {
            System.out.println("FIRST_THROW=" + firstThrow);
        }
        System.out.println("WARM=" + warm + " PROBE=" + probe + " acc=" + acc);
        System.out.println("WRONG_TRUE=" + wrongTrue);
        System.out.println("THROWN=" + thrown);
        System.out.println("MISMATCH_COUNT=" + (wrongTrue + thrown));
    }
}
