import java.nio.*;

/** SBR3 NIOBUF: cost of the buffer session rows. Prints ns/op per phase; run interleaved A/B with CRATONVM_UNRETIRE_NATIVE_SHADOW. */
public class Sbr3NioBufBench {
    static long sink;
    static long heap(ByteBuffer b, int n) { b.clear(); long s = 0; for (int i = 0; i < n; i++) { b.putInt((i & 255) << 2, i); s += b.getInt((i & 255) << 2); } return s; }
    static long rel(ByteBuffer b, int n) { long s = 0; for (int i = 0; i < n; i++) { b.clear(); b.putLong(i); b.flip(); s += b.getLong(); } return s; }
    static long bytes(ByteBuffer b, int n) { b.clear(); long s = 0; for (int i = 0; i < n; i++) { b.put(i & 1023, (byte) i); s += b.get((i * 7) & 1023); } return s; }
    static long view(IntBuffer v, int n) { long s = 0; for (int i = 0; i < n; i++) { v.put(i & 255, i); s += v.get(i & 255); } return s; }
    public static void main(String[] a) {
        int n = Integer.getInteger("n", 3_000_000);
        ByteBuffer h = ByteBuffer.allocate(4096), d = ByteBuffer.allocateDirect(4096);
        IntBuffer hv = h.asIntBuffer(), dv = d.asIntBuffer();
        for (int w = 0; w < 2; w++) { sink += heap(h, n / 10) + heap(d, n / 10) + rel(h, n / 10) + rel(d, n / 10) + bytes(h, n / 10) + bytes(d, n / 10) + view(hv, n / 10) + view(dv, n / 10); }
        String[] nm = {"heap.int", "direct.int", "heap.rel", "direct.rel", "heap.byte", "direct.byte", "heap.view", "direct.view"};
        for (int p = 0; p < 8; p++) {
            long t0 = System.nanoTime();
            long r = switch (p) { case 0 -> heap(h, n); case 1 -> heap(d, n); case 2 -> rel(h, n); case 3 -> rel(d, n); case 4 -> bytes(h, n); case 5 -> bytes(d, n); case 6 -> view(hv, n); default -> view(dv, n); };
            long t1 = System.nanoTime(); sink += r;
            System.out.println(nm[p] + " " + (t1 - t0) / n + " ns/op");
        }
    }
}
