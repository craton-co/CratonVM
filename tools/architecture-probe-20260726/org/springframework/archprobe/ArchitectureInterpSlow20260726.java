package org.springframework.archprobe;

/** Spring-prefixed copy: forced onto CratonVM's second interpreter path. */
public final class ArchitectureInterpSlow20260726 {
    static long kernel(int iterations) {
        int[] state = {3, 5, 7, 11, 13, 17, 19, 23};
        long sum = 0;
        for (int i = 0; i < iterations; i++) {
            int slot = i & 7;
            int value = state[slot];
            if ((i & 1) == 0) {
                value = value * 33 + i;
                sum += value ^ (value >>> 5);
            } else {
                value = value * 17 - i;
                sum -= value ^ (value << 3);
            }
            state[(slot + 3) & 7] = value;
        }
        for (int value : state) {
            sum += value;
        }
        return sum;
    }

    public static void main(String[] args) {
        int iterations = Integer.parseInt(args[0]);
        kernel(Math.min(iterations / 10, 100_000));
        long started = System.nanoTime();
        long checksum = kernel(iterations);
        long elapsed = System.nanoTime() - started;
        System.out.println("interp-slow\t" + iterations + "\t" + elapsed + "\t" + checksum);
    }
}
