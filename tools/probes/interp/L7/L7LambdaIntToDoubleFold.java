// Interpreter round i1 wave 10, lane L5 -- the `jit_lambda_int_to_double`
// adapter fold is armed by the constant pool now, not by the Elasticsearch
// `org/elasticsearch/tdigest/Dist` class name, so ANY class with the shape
// `((Double) f.apply(Integer.valueOf(i))).doubleValue()` gets it once the
// method compiles (both tiers). This probe runs that shape hot in a non-ES
// class through every receiver kind the helper distinguishes: the one lambda
// shape it fast-paths (a `get(I)D` method reference with one capture), a
// generic lambda, a `Function` returning a non-`Double` (ClassCastException at
// the `checkcast`), one returning null (NullPointerException at
// `doubleValue()`), and a null `Function` (NullPointerException at `apply`).
// `sumBoxedIndex` has the same opcode run with another boxing call at the
// `invokestatic` (`box`, which adds one), so the fold must NOT take it: taken,
// it would box with `Integer.valueOf` and print 523776.0.
//
// HotSpot 25 prints exactly (compile with plain `javac`, no `-g`):
//   table  1049088.0
//   lambda 524032.0
//   boxed  524800.0
//   cce    java.lang.ClassCastException: class java.lang.Integer cannot be cast to class java.lang.Double (java.lang.Integer and java.lang.Double are in module java.base of loader 'bootstrap')
//   npe-result java.lang.NullPointerException: Cannot invoke "java.lang.Double.doubleValue()" because the return value of "java.util.function.Function.apply(Object)" is null
//   npe-function java.lang.NullPointerException: Cannot invoke "java.util.function.Function.apply(Object)" because "<parameter1>" is null
//   table  1049088.0
//
// Compare `--compatible` with and without `--nojit`: identical stdout. The two
// NPE lines are the ones most likely to differ (the single-pass fold raises
// the null-`Function` NPE from its helper at the `invokestatic`'s bci; see
// docs/internal/fixed-bugs/interpreter-L6-jit-crate-tdigest-dist-name-peepholes-FIXED-20260925.md).
import java.util.function.Function;

public class L7LambdaIntToDoubleFold {
    static final class Table {
        final double[] v;

        Table(int n) {
            v = new double[n];
            for (int i = 0; i < n; i++) {
                v[i] = i * 2.0 + (i & 3);
            }
        }

        double get(int i) {
            return v[i];
        }
    }

    // aload_0; iload; invokestatic Integer.valueOf; invokeinterface
    // Function.apply; checkcast Double; invokevirtual Double.doubleValue.
    static double sum(Function<Integer, Double> f, int n) {
        double s = 0;
        for (int i = 0; i < n; i++) {
            s += f.apply(i);
        }
        return s;
    }

    static Integer box(int i) {
        return Integer.valueOf(i + 1);
    }

    // Same opcode run, but the boxing call is `box`, not `Integer.valueOf`.
    static double sumBoxedIndex(Function<Integer, Double> f, int n) {
        double s = 0;
        for (int i = 0; i < n; i++) {
            s += f.apply(box(i));
        }
        return s;
    }

    static String describe(Throwable t) {
        return t.getClass().getName() + ": " + t.getMessage();
    }

    public static void main(String[] a) {
        Table t = new Table(1024);
        Function<Integer, Double> table = t::get;
        Function<Integer, Double> lambda = i -> i + 0.5 * (i & 1);
        double tableSum = 0;
        double lambdaSum = 0;
        double boxedSum = 0;
        for (int round = 0; round < 3000; round++) {
            tableSum = sum(table, 1024);
            lambdaSum = sum(lambda, 1024);
            boxedSum = sumBoxedIndex(i -> (double) i, 1024);
        }
        System.out.println("table  " + tableSum);
        System.out.println("lambda " + lambdaSum);
        System.out.println("boxed  " + boxedSum);
        @SuppressWarnings({"unchecked", "rawtypes"})
        Function<Integer, Double> wrong = (Function) (Function<Integer, Object>) i -> i;
        try {
            sum(wrong, 4);
            System.out.println("cce    none");
        } catch (ClassCastException e) {
            System.out.println("cce    " + describe(e));
        }
        try {
            sum(i -> null, 4);
            System.out.println("npe-result none");
        } catch (NullPointerException e) {
            System.out.println("npe-result " + describe(e));
        }
        try {
            sum(null, 4);
            System.out.println("npe-function none");
        } catch (NullPointerException e) {
            System.out.println("npe-function " + describe(e));
        }
        System.out.println("table  " + sum(table, 1024));
    }
}
