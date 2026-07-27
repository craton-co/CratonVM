/**
 * Exercises both controlled launcher exits for the JIT-statistics regression
 * test. The arithmetic loop ensures the tier manager observes a hot method.
 */
public final class ArchitectureJitStatsExitProbe20260726 {
    private ArchitectureJitStatsExitProbe20260726() {}

    static long hotLoop(int iterations) {
        long sum = 0;
        for (int i = 0; i < iterations; i++) {
            sum += (i * 31L) ^ (i >>> 3);
        }
        return sum;
    }

    public static void main(String[] args) {
        if (args.length != 1 || (!args[0].equals("return") && !args[0].equals("exit"))) {
            throw new IllegalArgumentException("usage: return|exit");
        }
        System.out.println(hotLoop(250_000));
        if (args[0].equals("exit")) {
            System.exit(0);
        }
    }
}
