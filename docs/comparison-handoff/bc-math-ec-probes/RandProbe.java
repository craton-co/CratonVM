import java.util.Random;

public class RandProbe {
    public static void main(String[] a) {
        Random r = new Random(0xBCEC1234L);
        for (int i = 0; i < 6; i++) System.out.println("L[" + i + "]=" + r.nextLong());
        Random r2 = new Random(42);
        for (int i = 0; i < 6; i++) System.out.println("I[" + i + "]=" + r2.nextInt());
        System.out.println("seed0.nextLong=" + new Random(0).nextLong());
        System.out.println("seed0.nextInt=" + new Random(0).nextInt());
        Random r3 = new Random(123456789L);
        System.out.println("d=" + r3.nextDouble());
        System.out.println("bnd=" + new Random(7).nextInt(1000000));
    }
}
