/** Allocation and live-reference battery for the ParallelGCThreads launcher gate. */
public class G1ParallelGcThreadsProbe {
    static volatile byte[][] live = new byte[64][];

    public static void main(String[] args) {
        long sum = 0;
        for (int round = 0; round < 6; round++) {
            for (int i = 0; i < 12000; i++) {
                byte[] value = new byte[2048];
                value[0] = (byte) i;
                value[2047] = (byte) (i >>> 3);
                live[i & 63] = value;
                sum += value[0] + value[2047];
            }
            System.gc();
            for (int i = 0; i < live.length; i++) {
                sum += live[i][0] + live[i][2047];
            }
        }
        System.out.println("G1-THREADS-OK checksum=" + sum);
    }
}
