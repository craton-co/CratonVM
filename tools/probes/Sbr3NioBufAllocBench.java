import java.nio.*;

/** SBR3 NIOBUF: cost of allocateDirect with the Bits accounting retired (real Bits) or native. ns per allocation. */
public class Sbr3NioBufAllocBench {
    static long sink;
    public static void main(String[] a) {
        int n = Integer.getInteger("n", 200_000);
        for (int i = 0; i < n / 10; i++) sink += ByteBuffer.allocateDirect(1024).capacity();
        for (int r = 0; r < 3; r++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < n; i++) sink += ByteBuffer.allocateDirect(1024).capacity();
            long t1 = System.nanoTime();
            System.out.println("allocateDirect.1k " + (t1 - t0) / n + " ns/op");
        }
    }
}
