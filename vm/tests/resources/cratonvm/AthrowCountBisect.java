package cratonvm;

public class AthrowCountBisect {
    // Exactly ONE athrow, no string concat, one exception-table entry.
    static int oneThrow(int x) {
        try {
            if (x == 0) {
                throw new RuntimeException();
            }
            return x;
        } catch (RuntimeException e) {
            return -1;
        }
    }

    // Exactly TWO athrow instructions (both simple, no-arg constructor, no
    // string concat), each with its OWN separate (non-nested, sequential)
    // try/catch — two exception-table entries.
    static int twoThrowsSequential(int x) {
        int a;
        try {
            if (x == 0) {
                throw new RuntimeException();
            }
            a = 1;
        } catch (RuntimeException e) {
            a = -1;
        }
        int b;
        try {
            if (x == 1) {
                throw new IllegalStateException();
            }
            b = 2;
        } catch (IllegalStateException e) {
            b = -2;
        }
        return a + b;
    }

    public static int oneThrowChecksum() {
        int sum = 0;
        for (int i = 0; i < 20000; i++) {
            sum += oneThrow(i % 2);
        }
        return sum;
    }

    public static int twoThrowsChecksum() {
        int sum = 0;
        for (int i = 0; i < 20000; i++) {
            sum += twoThrowsSequential(i % 3);
        }
        return sum;
    }
}
