// Interpreter round i1, lane L2 — decoded-opcode conformance probe.
//
// Compare stdout against HotSpot 25 (`java L2ArrayCastSemantics`). Run CratonVM
// twice: once normally, and once on the DECODED path the `interp-decoded`
// difftest axis uses (`--compatible --noverify`, CRATONVM_DISABLE_JIT=1),
// because several rows below exercise `opcodes.rs` arms that the raw-bytecode
// loop answers on its own in the default configuration.
//
// Expected HotSpot 25 output, row by row (verified shapes, not recalled):
//   r01..r04  AIOOBE "Index -1 out of bounds for length 3"   <- fixed in the
//             decoded arms (was "Index 2147483647 ..." there)
//   r05       AIOOBE "Index 3 out of bounds for length 3"
//   r06       NegativeArraySizeException "-1"
//   r07       NPE prefix "Cannot load from int array"
//   r08       NPE prefix "Cannot store to long array"
//   r09       NPE prefix "Cannot read the array length"
//   r10       ArrayStoreException "java.lang.Integer"
//   r11       ClassCastException "class java.lang.String cannot be cast to
//             class java.lang.Integer (java.lang.String and java.lang.Integer
//             are in module java.base of loader 'bootstrap')"
//   r12..r19  instanceof on arrays: true false true true false true true true
//   r20       ClassCastException for (String[]) new Object[1]
//             (matches HotSpot in every mode since interpreter round i1
//             wave 10, which retired the lenient Object[] -> T[] cast:
//             docs/internal/fixed-bugs/interpreter-L2-lenient-object-array-cast-admits-illegal-casts-FIXED-20260925.md)
//   r21       ArrayStoreException "[Ljava.lang.Object;" for String[][] <- Object[]
//             (same fix)
//   r22       rotating-receiver cast site: "hits=200000 cce=100000"
//             (exercises the cast-site receiver memo across a polymorphic site)
//   r23       anewarray of a user class in a loop: "len=7 comp=L2ArrayCastSemantics$A"
//   r24       ldc class literal identity: true
public class L2ArrayCastSemantics {
    interface I {}
    static class A implements I {}
    static class B implements I {}
    static class C {}

    static String prefix(String m) {
        if (m == null) return "null";
        int i = m.indexOf(" because");
        return i < 0 ? m : m.substring(0, i);
    }

    static void row(String id, Runnable r) {
        try {
            r.run();
            System.out.println(id + " ok");
        } catch (Throwable t) {
            String m = t.getMessage();
            if (t instanceof NullPointerException) m = prefix(m);
            System.out.println(id + " " + t.getClass().getName() + ": " + m);
        }
    }

    static int idx(int i) { return i; } // defeats constant folding of indices

    public static void main(String[] args) {
        int[] ia = new int[3];
        long[] la = new long[3];
        double[] da = new double[3];
        Object[] oa = new Object[3];
        row("r01", () -> { int x = ia[idx(-1)]; });
        row("r02", () -> { ia[idx(-1)] = 1; });
        row("r03", () -> { la[idx(-1)] = 1L; });
        row("r04", () -> { da[idx(-1)] = 1.0; });
        row("r05", () -> { Object x = oa[idx(3)]; });
        row("r06", () -> { int[] x = new int[idx(-1)]; });
        int[] nullInts = args.length > 99 ? ia : null;
        long[] nullLongs = args.length > 99 ? la : null;
        row("r07", () -> { int x = nullInts[0]; });
        row("r08", () -> { nullLongs[0] = 2L; });
        row("r09", () -> { int x = nullInts.length; });
        row("r10", () -> { Object[] s = new String[1]; s[0] = Integer.valueOf(1); });
        row("r11", () -> { Object o = "s"; Integer i = (Integer) o; });

        Object strs = new String[1];
        Object ints = new int[1];
        Object ints2 = new int[1][1];
        Object nums = new Integer[1];
        Object objs = new Object[1];
        System.out.println("r12 " + (strs instanceof Object[]));
        System.out.println("r13 " + (ints instanceof Object[]));
        System.out.println("r14 " + (ints2 instanceof Object[]));
        System.out.println("r15 " + (nums instanceof Number[]));
        System.out.println("r16 " + (objs instanceof String[]));
        System.out.println("r17 " + (ints instanceof Cloneable));
        System.out.println("r18 " + (ints instanceof java.io.Serializable));
        System.out.println("r19 " + (strs instanceof Comparable[]));
        row("r20", () -> { String[] s = (String[]) objs; });
        row("r21", () -> { Object[][] s = new String[1][]; s[0] = new Object[1]; });

        Object[] rot = { new A(), new B(), new C() };
        int hits = 0, cce = 0;
        for (int k = 0; k < 300000; k++) {
            try {
                I i = (I) rot[k % 3];
                if (i != null) hits++;
            } catch (ClassCastException e) {
                cce++;
            }
        }
        System.out.println("r22 hits=" + hits + " cce=" + cce);

        A[] last = null;
        for (int k = 0; k < 1000; k++) last = new A[7];
        System.out.println("r23 len=" + last.length + " comp=" + last.getClass().getComponentType().getName());
        Class<?> c1 = A.class, c2 = A.class;
        System.out.println("r24 " + (c1 == c2 && c1 == new A().getClass()));
    }
}
