// Interpreter round i1, wave 3, lane L2 — class-target subtype checks answered
// from the primary-supers display (`ClassStore::primary_display_verdict`).
//
// Correctness: STDOUT must match HotSpot 25 exactly:
//   rows    a<:A0=true a<:A3=true b<:A3=false c<:A5=true c<:B1=false arr<:A0=false
//   catch   A3 A3 E2 other
//   poly    checksum=3000000
//   neg     checksum=0
//   exc     checksum=1000000
//
// What to measure (STDERR, ns/iteration): run with CRATONVM_DISABLE_JIT=1,
// with and without CRATONVM_LOADER_NO_SUBTYPE_DISPLAY=1 (same binary), and
// interleave the two arms, median of several reps (in-JVM timings on this host
// swing ~3x). Expected effect of the display:
//   poly — four receiver classes rotate through one `instanceof A2` site, so
//          the per-site receiver memo misses every time and each check reaches
//          `is_subclass_of`: a DAG walk before, one compare after.
//   neg  — `instanceof B1` for receivers in the A family: a full walk of each
//          receiver's supertype DAG before, a compare after (rotating receivers
//          defeat the negative memo, which is monomorphic).
//   exc  — exception-handler matching of E4 against `catch (E1)` after one
//          non-matching `catch (Other)` row; catch types are classes.
public class L2SubtypeDisplay {
    interface Marker {}
    static class A0 {}
    static class A1 extends A0 implements Marker {}
    static class A2 extends A1 {}
    static class A3 extends A2 {}
    static class A4 extends A3 {}
    static class A5 extends A4 {}
    static class B0 {}
    static class B1 extends B0 {}

    static class E1 extends RuntimeException { E1() { super(null, null, false, false); } }
    static class E2 extends E1 {}
    static class E3 extends E2 {}
    static class E4 extends E3 {}
    static class Other extends RuntimeException {}

    static final int N = 1_000_000;

    static String rows() {
        Object a = new A4();
        Object b = new B1();
        Object c = new A5();
        Object arr = new A0[1];
        return "a<:A0=" + (a instanceof A0) + " a<:A3=" + (a instanceof A3)
                + " b<:A3=" + (b instanceof A3) + " c<:A5=" + (c instanceof A5)
                + " c<:B1=" + (c instanceof B1) + " arr<:A0=" + (arr instanceof A0);
    }

    static String classify(RuntimeException e) {
        try {
            throw e;
        } catch (Other o) {
            return "other";
        } catch (A3Ex x) {
            return "never";
        } catch (E3 x) {
            return "A3";
        } catch (E2 x) {
            return "E2";
        }
    }

    // Unused catch type, present so the handler table has a non-matching
    // class row before the matching one.
    static class A3Ex extends RuntimeException {}

    static int poly(Object[] xs) {
        int s = 0;
        for (int i = 0; i < 4 * N; i++) {
            if (xs[i & 3] instanceof A2) s++;
        }
        return s;
    }

    static int neg(Object[] xs) {
        int s = 0;
        for (int i = 0; i < 4 * N; i++) {
            if (xs[i & 3] instanceof B1) s++;
        }
        return s;
    }

    static int exc(RuntimeException e) {
        int s = 0;
        for (int i = 0; i < N; i++) {
            try {
                throw e;
            } catch (Other o) {
                s += 2;
            } catch (E1 x) {
                s++;
            }
        }
        return s;
    }

    public static void main(String[] args) {
        System.out.println("rows    " + rows());
        System.out.println("catch   " + classify(new E4()) + " " + classify(new E3()) + " "
                + classify(new E2()) + " " + classify(new Other()));

        Object[] mixed = { new A1(), new A3(), new A4(), new A5() };
        long t0 = System.nanoTime();
        int p = poly(mixed);
        long t1 = System.nanoTime();
        System.out.println("poly    checksum=" + p);
        System.err.printf("poly %.2f ns/iter%n", (t1 - t0) / (4.0 * N));

        t0 = System.nanoTime();
        int n = neg(mixed);
        t1 = System.nanoTime();
        System.out.println("neg     checksum=" + n);
        System.err.printf("neg  %.2f ns/iter%n", (t1 - t0) / (4.0 * N));

        E4 e4 = new E4();
        t0 = System.nanoTime();
        int x = exc(e4);
        t1 = System.nanoTime();
        System.out.println("exc     checksum=" + x);
        System.err.printf("exc  %.2f ns/iter%n", (t1 - t0) / (1.0 * N));
    }
}
