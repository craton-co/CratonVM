// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// j27 wave 8 (lane W8UPC): a settled REF_invokeStatic handle called with a first argument of every
// kind the static native-callback memo keys on (`native_callee_memo::static_first_arg_key`): an
// object of several classes, an array (declined), a lambda proxy (declined), null and a primitive
// (not-an-object). Every answer must be the same with and without
// CRATONVM_NATIVE_CALLBACK_MEMO_STATIC_KEY_VALIDATE_ONCE=1, in both modes, JIT and --nojit.
//   java J27W8UpcKeyRows        (the FFM up-call rows are J27FfmUpcallRows' and R22FfmUpcallCost's)
// Expected HotSpot 25 output:
//   fact tag-seq 1234012340
//   fact tag-sum 20000
//   fact twice-sum 99990000
//   fact copy-last false/true/5001
//   fact lambda-runs 3000
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.Arrays;

public class J27W8UpcKeyRows {
    static int runs;

    static int tag(Object o) {
        if (o == null) return 0;
        if (o instanceof String) return 1;
        if (o instanceof Integer) return 2;
        if (o instanceof int[]) return 3;
        if (o instanceof Runnable) return 4;
        return 5;
    }

    static long twice(int x) {
        return 2L * x;
    }

    static int[] copy(int[] a) {
        return a.clone();
    }

    static int run(Runnable r) {
        r.run();
        return runs;
    }

    static void fact(String name, Object value) {
        System.out.println("fact " + name + " " + value);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        MethodHandle tag = lk.findStatic(J27W8UpcKeyRows.class, "tag",
            MethodType.methodType(int.class, Object.class));
        MethodHandle twice = lk.findStatic(J27W8UpcKeyRows.class, "twice",
            MethodType.methodType(long.class, int.class));
        MethodHandle copy = lk.findStatic(J27W8UpcKeyRows.class, "copy",
            MethodType.methodType(int[].class, int[].class));
        MethodHandle run = lk.findStatic(J27W8UpcKeyRows.class, "run",
            MethodType.methodType(int.class, Runnable.class));

        Runnable lambda = () -> runs++;
        Object[] firsts = {"s", Integer.valueOf(7), new int[] {1}, lambda, null};
        StringBuilder seq = new StringBuilder();
        int sum = 0;
        for (int i = 0; i < 10_000; i++) {
            int t = (int) tag.invokeExact(firsts[i % 5]);
            if (i < 10) seq.append(t);
            sum += t;
        }
        fact("tag-seq", seq);
        fact("tag-sum", sum);

        long tw = 0;
        for (int i = 0; i < 10_000; i++) tw += (long) twice.invokeExact(i);
        fact("twice-sum", tw);

        int[] a = null;
        int[] c = null;
        for (int i = 0; i < 5_000; i++) {
            a = new int[] {i, i + 1, i + 2};
            c = (int[]) copy.invokeExact(a);
        }
        fact("copy-last", (c == a) + "/" + Arrays.equals(c, a) + "/" + c[2]);

        int last = 0;
        for (int i = 0; i < 3_000; i++) {
            final int k = i;
            Runnable r = (k & 1) == 0 ? lambda : () -> runs++;
            last = (int) run.invokeExact(r);
        }
        fact("lambda-runs", last);
    }
}
