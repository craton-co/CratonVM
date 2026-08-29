import java.util.Random;

/**
 * Every `Random` below is garbage the moment the loop moves on, and every round
 * ends with a full GC. A Java heap that stays flat while the PROCESS keeps
 * growing says the retained bytes are not on the Java heap.
 */
public class RandomLeak {
    static long sink;
    public static void main(String[] a) throws Exception {
        int chunk = Integer.getInteger("chunk", 200_000);
        int rounds = Integer.getInteger("rounds", 10);
        Runtime rt = Runtime.getRuntime();
        long made = 0;
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < chunk; i++) sink += new Random(i).nextInt();
            made += chunk;
            System.gc();
            Thread.sleep(50);
            long used = rt.totalMemory() - rt.freeMemory();
            System.out.printf("CK leak randoms=%,10d  javaHeapUsed=%,8d KB%n", made, used / 1024);
        }
        System.out.println("CK sink=" + (sink == 0 ? 0 : 1));
    }
}
