/**
 * SBR3 UNSAFECAS: cost of the IR tier's int sign-extension at calls and returns (`gp_load_abi_value`).
 * Recursive and non-inlinable int-argument calls; prints ms per kernel to stderr, checksums to stdout.
 */
public class Sbr3UnsafecasAbiBench {
    interface Op { int ap(int a, int b); }
    static final class Add implements Op { public int ap(int a, int b) { return a + b; } }
    static final class Sub implements Op { public int ap(int a, int b) { return a - b; } }
    static final class Mul implements Op { public int ap(int a, int b) { return a * b; } }
    static final class Xor implements Op { public int ap(int a, int b) { return a ^ b; } }

    static int fib(int n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }

    static int ack(int m, int n) {
        if (m == 0) return n + 1;
        if (n == 0) return ack(m - 1, 1);
        return ack(m - 1, ack(m, n - 1));
    }

    static int poly(Op[] ops, int rounds) {
        int acc = 1;
        for (int r = 0; r < rounds; r++) acc = ops[r & 3].ap(acc, r) + 1;
        return acc;
    }

    public static void main(String[] a) {
        Op[] ops = { new Add(), new Sub(), new Mul(), new Xor() };
        long t0 = System.nanoTime();
        int f = 0;
        for (int i = 0; i < 6; i++) f += fib(32);
        long t1 = System.nanoTime();
        int k = 0;
        for (int i = 0; i < 4; i++) k += ack(2, 3000) + ack(3, 7);
        long t2 = System.nanoTime();
        int p = 0;
        for (int i = 0; i < 10; i++) p += poly(ops, 5_000_000);
        long t3 = System.nanoTime();
        System.out.println("fib=" + f + " ack=" + k + " poly=" + p);
        System.err.println("MS fib=" + (t1 - t0) / 1_000_000 + " ack=" + (t2 - t1) / 1_000_000 + " poly=" + (t3 - t2) / 1_000_000);
    }
}
