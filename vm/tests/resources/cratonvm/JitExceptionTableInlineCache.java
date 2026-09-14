package cratonvm;

public final class JitExceptionTableInlineCache {
    private static final int ITERATIONS = 300_000;
    private static int calls;

    interface Access {
        int get(int[] values, int index);
    }

    static final class CatchingAccess implements Access {
        @Override
        public int get(int[] values, int index) {
            calls++;
            try {
                return values[index];
            } catch (ArrayIndexOutOfBoundsException expected) {
                return -17;
            }
        }
    }

    private static int invoke(Access access, int[] values, int index) {
        return access.get(values, index);
    }

    public static long run() {
        calls = 0;
        Access access = new CatchingAccess();
        int[] values = { 3, 5, 7, 11 };
        long sum = 0;
        for (int i = 0; i < ITERATIONS; i++) {
            int index = (i % 19 == 0) ? 9 : (i & 3);
            int expected = index == 9 ? -17 : values[index];
            int actual = invoke(access, values, index);
            if (actual != expected) {
                throw new AssertionError("result=" + actual + " expected=" + expected + " at " + i);
            }
            sum += actual;
        }
        if (calls != ITERATIONS) {
            throw new AssertionError("callee re-executed: calls=" + calls + " expected=" + ITERATIONS);
        }
        return sum;
    }

    public static void main(String[] args) {
        long sum = run();
        System.out.println("OK exception-table-inline-cache sum=" + sum + " calls=" + calls);
    }
}
