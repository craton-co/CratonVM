import java.util.function.*;
import java.util.*;

public class LambdaCorrectnessProbe {

    interface Thrower { int apply(int x) throws Exception; }

    private static int addOne(int x) { return x + 1; }

    // A lambda body that THROWS on some inputs — the fast path must still be
    // correct after the impl gets JIT-compiled (exception routing).
    private static final IntUnaryOperator maybeThrow = v -> {
        if (v == 999) throw new RuntimeException("boom-" + v);
        return v * 2;
    };

    private static final Function<Integer, Integer> capturing(int k) {
        return v -> v + k;
    }

    public static void main(String[] args) throws Exception {
        // 1) plain non-capturing lambda, many calls to force JIT nomination
        IntUnaryOperator plus1 = v -> v + 1;
        long sum = 0;
        for (int i = 0; i < 2000; i++) sum += plus1.applyAsInt(i);
        System.out.println("sum1=" + sum + " expected=" + expectedSum(2000, 1));

        // 2) capturing lambda
        Function<Integer, Integer> capAdd = capturing(7);
        long sum2 = 0;
        for (int i = 0; i < 2000; i++) sum2 += capAdd.apply(i);
        System.out.println("sum2=" + sum2 + " expected=" + expectedSum(2000, 7));

        // 3) method reference
        IntUnaryOperator mref = LambdaCorrectnessProbe::addOne;
        long sum3 = 0;
        for (int i = 0; i < 2000; i++) sum3 += mref.applyAsInt(i);
        System.out.println("sum3=" + sum3 + " expected=" + expectedSum(2000, 1));

        // 4) exception-throwing lambda body, after warmup (compiled + uncompiled
        // both exercised — this checks that a caught exception mid-loop, after
        // the impl body is likely already compiled, still produces correct
        // recovery and does not corrupt later calls).
        int caught = 0;
        long sum4 = 0;
        for (int i = 0; i < 2000; i++) {
            int v = (i == 999 || i == 1999) ? 999 : i;
            try {
                sum4 += maybeThrow.applyAsInt(v);
            } catch (RuntimeException e) {
                caught++;
                if (!e.getMessage().equals("boom-999")) {
                    System.out.println("WRONG EXCEPTION MESSAGE: " + e.getMessage());
                }
            }
        }
        System.out.println("sum4=" + sum4 + " caught=" + caught + " expected_caught=2");

        // 5) default method on the functional interface (not the SAM) still
        // dispatches correctly through a lambda receiver.
        IntUnaryOperator base = v -> v + 100;
        IntUnaryOperator composed = base.andThen(v -> v * 2);
        long sum5 = 0;
        for (int i = 0; i < 2000; i++) sum5 += composed.applyAsInt(i);
        System.out.println("sum5=" + sum5 + " expected=" + expectedComposedSum(2000));

        // 6) instance-method-reference lambda (InvokeVirtual impl kind), many calls
        List<String> names = new ArrayList<>();
        for (int i = 0; i < 500; i++) names.add("n" + i);
        StringBuilder sb = new StringBuilder();
        Consumer<String> collector = sb::append;
        for (String n : names) collector.accept(n);
        System.out.println("sb.length=" + sb.length() + " expected=" + names.stream().mapToInt(String::length).sum());

        System.out.println("ALL-DONE");
    }

    private static long expectedSum(int n, int add) {
        long s = 0;
        for (int i = 0; i < n; i++) s += i + add;
        return s;
    }

    private static long expectedComposedSum(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) s += (i + 100) * 2;
        return s;
    }
}
