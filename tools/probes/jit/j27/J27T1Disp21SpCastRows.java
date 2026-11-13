// j27 wave 7 (lane T1DISP21): DISPATCH21-4's single-pass twin
// (CRATONVM_JIT_SP_IC_CAST_KIND_SCREEN_ELIDE, opt-in): the single-pass inline-cache
// cascade drops its receiver kind screen at a receiver-only call right after a
// `checkcast` to a type no array is assignable to. Book:
// docs/feature-designs/native-dispatch-architecture.md.
//
// Kernels (each its own method, one call site each):
//   castTwo   ((Shape) os[i]).sides()            two classes -- the elided shape
//   castFive  ((Shape) os[i]).sides()            five classes (PIC ways / MIC+4)
//   castArg   ((Shape) os[i]).plus(1)            a call with an argument -- NOT elided (the
//             cast's result is not the stack top at the call)
//   decoy     acc += -16384 + s.sides()          `sipush 0xC000 ; aload_3 ; invokeinterface`:
//             the byte 3 before the call is 0xC0 but an OPERAND -- must NOT be elided
//   mixed     the castTwo shape fed Shape[] / int[] / String / null among Shapes
//
// Facts (`fact <row> <value>`; equal to HotSpot 25 in every arm and both modes). Each sum line
// carries an independently computed expectation: `<value> ok` or `<value> BAD <expected>`.
// The cold-* rows cut CCE messages at " (" (the module / loader suffix); the hot-* rows count
// exceptions only (C2's OmitStackTraceInFastThrow drops the messages once a site is hot).
//
// Expected HotSpot 25 output (facts; worked out from JDK semantics and the ring arithmetic,
// N = 1000, REPS = 2000; the first host `java` run is the reference):
//   fact cold-cce-ref-array class [LJ27T1Disp21SpCastRows$Shape; cannot be cast to class J27T1Disp21SpCastRows$Shape
//   fact cold-cce-int-array class [I cannot be cast to class J27T1Disp21SpCastRows$Shape
//   fact cold-cce-string class java.lang.String cannot be cast to class J27T1Disp21SpCastRows$Shape
//   fact cold-npe NPE
//   fact sum-cast-two 2666000 ok
//   fact sum-cast-five 5984000 ok
//   fact sum-cast-arg 4666000 ok
//   fact sum-decoy -32762016000 ok
//   fact hot-mixed 2400000 ok
//   fact hot-cce 160000
//   fact hot-npe 40000
//
// Run (orchestrator; NON-BUILDER lane wrote this):
//   javac -d <dir> tools/probes/jit/j27/J27T1Disp21SpCastRows.java
//   java -cp <dir> J27T1Disp21SpCastRows > hs.txt
//   for arm in "" "CRATONVM_JIT_SP_IC_CAST_KIND_SCREEN_ELIDE=1"; do
//     for sp in "" "CRATONVM_DBG_JIT_SINGLE_PASS_ONLY=1"; do
//       for mode in "" "--compatible" "--nojit"; do
//         env $arm $sp cratonvm $mode -cp <dir> J27T1Disp21SpCastRows | diff - hs.txt; done; done; done
// Engagement (CRATONVM_DBG_JITC=1 CRATONVM_DBG_JIT_SINGLE_PASS_ONLY=1 + the arm):
//   `sp ic kind screen elided by the checkcast before it: ...castTwo...` and `...castFive...`
//   (and `...mixed...` where its protected site keeps an inline cache); NEVER for `decoy` or
//   `castArg`; no such line without the arm.
// Timing (-Dj27.time=true, separate `time` lines, ns/op, median of 5 reps): cast-two, cast-five.

public class J27T1Disp21SpCastRows {
    interface Shape {
        int sides();

        int plus(int k);
    }

    static final class S1 implements Shape {
        public int sides() { return 1; }
        public int plus(int k) { return 1 + k; }
    }

    static final class S2 implements Shape {
        public int sides() { return 2; }
        public int plus(int k) { return 2 + k; }
    }

    static final class S3 implements Shape {
        public int sides() { return 3; }
        public int plus(int k) { return 3 + k; }
    }

    static final class S4 implements Shape {
        public int sides() { return 4; }
        public int plus(int k) { return 4 + k; }
    }

    static final class S5 implements Shape {
        public int sides() { return 5; }
        public int plus(int k) { return 5 + k; }
    }

    static final int N = 1000;
    static final int REPS = 2000;

    static Object[] ring(Shape... kinds) {
        Object[] os = new Object[N];
        for (int i = 0; i < N; i++) {
            os[i] = kinds[(i * 5 + i / 3) % kinds.length];
        }
        return os;
    }

    static long expectSides(Object[] os, int add) {
        long e = 0;
        for (Object o : os) {
            e += ((Shape) o).sides() + add;
        }
        return e * REPS;
    }

    static long castTwo(Object[] os) {
        long acc = 0;
        for (int i = 0; i < os.length; i++) {
            acc += ((Shape) os[i]).sides();
        }
        return acc;
    }

    static long castFive(Object[] os) {
        long acc = 0;
        for (int i = 0; i < os.length; i++) {
            acc += ((Shape) os[i]).sides();
        }
        return acc;
    }

    static long castArg(Object[] os) {
        long acc = 0;
        for (int i = 0; i < os.length; i++) {
            acc += ((Shape) os[i]).plus(1);
        }
        return acc;
    }

    // Locals ss=0, acc=1, i=2, s=3 so the receiver load is the one-byte aload_3 and the
    // byte three before the invokeinterface is sipush's 0xC0 high operand byte.
    static long decoy(Shape[] ss) {
        int acc = 0;
        for (int i = 0; i < ss.length; i++) {
            Shape s = ss[i];
            acc += -16384 + s.sides();
        }
        return acc;
    }

    /** The castTwo shape fed non-Shape receivers: [0] sum, [1] CCE count, [2] NPE count. */
    static long[] mixed(Object[] os) {
        long[] r = new long[3];
        for (int i = 0; i < os.length; i++) {
            try {
                r[0] += ((Shape) os[i]).sides();
            } catch (ClassCastException e) {
                r[1]++;
            } catch (NullPointerException e) {
                r[2]++;
            }
        }
        return r;
    }

    static void sumFact(String row, long got, long want) {
        System.out.println("fact " + row + " " + got + (got == want ? " ok" : " BAD " + want));
    }

    static String cut(String m) {
        if (m == null) {
            return "null";
        }
        int k = m.indexOf(" (");
        return k < 0 ? m : m.substring(0, k);
    }

    static String coldCast(Object o) {
        try {
            return "sides=" + ((Shape) o).sides();
        } catch (ClassCastException e) {
            return cut(e.getMessage());
        } catch (NullPointerException e) {
            return "NPE";
        }
    }

    public static void main(String[] args) {
        boolean time = Boolean.getBoolean("j27.time");
        Object[] two = ring(new S1(), new S2());
        Object[] five = ring(new S1(), new S2(), new S3(), new S4(), new S5());
        Shape[] decoys = new Shape[N];
        for (int i = 0; i < N; i++) {
            decoys[i] = (Shape) five[i];
        }

        System.out.println("fact cold-cce-ref-array " + coldCast(new Shape[1]));
        System.out.println("fact cold-cce-int-array " + coldCast(new int[1]));
        System.out.println("fact cold-cce-string " + coldCast("s"));
        System.out.println("fact cold-npe " + coldCast(null));

        long a = 0, b = 0, c = 0, d = 0;
        for (int r = 0; r < REPS; r++) {
            a += castTwo(two);
            b += castFive(five);
            c += castArg(two);
            d += decoy(decoys);
        }
        sumFact("sum-cast-two", a, expectSides(two, 0));
        sumFact("sum-cast-five", b, expectSides(five, 0));
        sumFact("sum-cast-arg", c, expectSides(two, 1));
        sumFact("sum-decoy", d, expectSides(five, -16384));

        // Every tenth slot a non-Shape: 3 kinds of CCE, one null, in a fixed cycle.
        Object[] bad = two.clone();
        Object[] intruders = {new Shape[1], new int[1], "s", null, new Shape[1]};
        long wantSum = 0;
        long wantCce = 0;
        long wantNpe = 0;
        for (int i = 0; i < N; i++) {
            if (i % 10 == 9) {
                bad[i] = intruders[(i / 10) % intruders.length];
                if (bad[i] == null) {
                    wantNpe++;
                } else {
                    wantCce++;
                }
            } else {
                wantSum += ((Shape) bad[i]).sides();
            }
        }
        long[] m = new long[3];
        for (int r = 0; r < REPS; r++) {
            long[] x = mixed(bad);
            m[0] += x[0];
            m[1] += x[1];
            m[2] += x[2];
        }
        sumFact("hot-mixed", m[0], wantSum * REPS);
        System.out.println("fact hot-cce " + m[1] + (m[1] == wantCce * REPS ? "" : " BAD"));
        System.out.println("fact hot-npe " + m[2] + (m[2] == wantNpe * REPS ? "" : " BAD"));

        if (time) {
            for (String row : new String[] {"cast-two", "cast-five"}) {
                Object[] os = row.equals("cast-two") ? two : five;
                long[] ns = new long[5];
                long sink = 0;
                for (int rep = 0; rep < 5; rep++) {
                    long t0 = System.nanoTime();
                    for (int r = 0; r < REPS; r++) {
                        sink += row.equals("cast-two") ? castTwo(os) : castFive(os);
                    }
                    ns[rep] = System.nanoTime() - t0;
                }
                java.util.Arrays.sort(ns);
                System.out.println("time " + row + " " + (ns[2] / ((double) REPS * N)) + " sink=" + (sink & 1));
            }
        }
    }
}
