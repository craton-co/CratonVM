import java.util.Random;
/** The determinism contract: seeded Random must give the JDK's exact sequence. */
public class RandomSpec {
    public static void main(String[] a) {
        Random r = new Random(42);
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 5; i++) sb.append(r.nextInt()).append(',');
        System.out.println("CK seeded42.nextInt x5 = " + sb);
        Random r2 = new Random(12345);
        System.out.println("CK seeded12345.nextLong = " + r2.nextLong());
        System.out.println("CK seeded12345.nextDouble = " + r2.nextDouble());
        System.out.println("CK seeded12345.nextInt(100) = " + r2.nextInt(100));
        System.out.println("CK seeded12345.nextBoolean = " + r2.nextBoolean());
        System.out.println("CK seeded12345.nextGaussian = " + r2.nextGaussian());
        byte[] b = new byte[8];
        new Random(7).nextBytes(b);
        StringBuilder hex = new StringBuilder();
        for (byte x : b) hex.append(String.format("%02x", x));
        System.out.println("CK seeded7.nextBytes = " + hex);
        Random u1 = new Random(), u2 = new Random();
        System.out.println("CK unseeded-distinct = " + (u1.nextLong() != u2.nextLong()));
    }
}
