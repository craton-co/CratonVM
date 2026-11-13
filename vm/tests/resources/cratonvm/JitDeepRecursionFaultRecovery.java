package cratonvm;

public final class JitDeepRecursionFaultRecovery {
    private JitDeepRecursionFaultRecovery() {}

    public static int nonTail(int n) {
        if (n <= 0) {
            return 0;
        }
        return nonTail(n - 1) + 1;
    }

    public static int warmupNonTail() {
        int sum = 0;
        for (int i = 0; i < 16; i++) {
            sum += nonTail(8);
        }
        return sum;
    }

    public static int catchOverflowAfterWarmup() {
        try {
            nonTail(100000000);
            return -1;
        } catch (StackOverflowError expected) {
            return 42;
        }
    }

    public static void main(String[] args) {
        System.out.println(catchOverflowAfterWarmup());
    }
}
