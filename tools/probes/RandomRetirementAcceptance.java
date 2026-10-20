import java.io.*;
import java.util.Random;

/** Random shadow retirement: subclass dispatch, serialization and shared CAS. */
public final class RandomRetirementAcceptance {
    static final class OverrideRandom extends Random {
        int calls;
        OverrideRandom() { super(7); }
        @Override protected int next(int bits) {
            calls++;
            return (calls * 0x1234567) >>> (32 - bits);
        }
    }
    static Random roundTrip(Random r) throws Exception {
        ByteArrayOutputStream b = new ByteArrayOutputStream();
        try (ObjectOutputStream out = new ObjectOutputStream(b)) { out.writeObject(r); }
        try (ObjectInputStream in = new ObjectInputStream(new ByteArrayInputStream(b.toByteArray()))) {
            return (Random) in.readObject();
        }
    }
    public static void main(String[] args) throws Exception {
        OverrideRandom child = new OverrideRandom();
        long subclass = 0;
        for (int i = 0; i < 10000; i++) subclass += child.nextInt();
        if (child.calls != 10000) throw new AssertionError("override calls " + child.calls);
        System.out.println("CK subclass sum=" + subclass + " calls=" + child.calls);

        Random r = new Random(42);
        r.nextGaussian(); // leave a cached second Gaussian in the serialized state
        Random copy = roundTrip(r);
        System.gc();
        long state = 0;
        for (int i = 0; i < 1000; i++) {
            int x = r.nextInt(), y = copy.nextInt();
            if (x != y) throw new AssertionError("serialized LCG " + i);
            state += y;
        }
        if (Double.doubleToLongBits(r.nextGaussian()) != Double.doubleToLongBits(copy.nextGaussian()))
            throw new AssertionError("serialized Gaussian cache");
        System.out.println("CK serialization sum=" + state + " gaussian=PASS");

        int each = Integer.getInteger("each", 50000);
        int threads = 4;
        Random expected = new Random(7);
        long wanted = 0;
        for (int i = 0; i < threads * each; i++) wanted += expected.nextInt();
        Random shared = new Random(7);
        long[] sums = new long[threads];
        Thread[] workers = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final int index = t;
            workers[t] = new Thread(() -> {
                long sum = 0;
                for (int i = 0; i < each; i++) sum += shared.nextInt();
                sums[index] = sum;
            });
            workers[t].start();
        }
        long actual = 0;
        for (int t = 0; t < threads; t++) { workers[t].join(); actual += sums[t]; }
        if (actual != wanted) throw new AssertionError("shared CAS sum " + actual + " expected " + wanted);
        if (shared.nextLong() != expected.nextLong()) throw new AssertionError("shared final state");
        System.out.println("CK shared threads=" + threads + " draws=" + (threads * each) + " sum=" + actual);
        System.out.println("CK RandomRetirementAcceptance PASS");
    }
}
