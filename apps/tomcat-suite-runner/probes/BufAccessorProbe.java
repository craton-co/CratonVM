import java.nio.ByteBuffer;
import java.nio.CharBuffer;

/** Per-call cost of the single-char/byte NIO buffer accessors the Tomcat
 *  Utf8Encoder slow path uses (CharBuffer.get() / ByteBuffer.put(byte)). */
public class BufAccessorProbe {

    static final int N = 2_000_000;

    static long bench(String label, Runnable r) {
        // warm
        r.run();
        long best = Long.MAX_VALUE;
        for (int i = 0; i < 3; i++) {
            long t = System.nanoTime();
            r.run();
            long d = System.nanoTime() - t;
            if (d < best) {
                best = d;
            }
        }
        System.out.printf("%-34s %8.1f ms  %7.1f ns/op%n", label, Double.valueOf(best / 1e6),
                Double.valueOf((double) best / N));
        return best;
    }

    public static void main(String[] args) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 1024; i++) {
            sb.append('x');
        }
        final String msg = sb.toString();

        final CharBuffer wrapped = CharBuffer.wrap(msg);
        System.out.println("CharBuffer.wrap(String) class=" + wrapped.getClass().getName()
                + " hasArray=" + wrapped.hasArray());
        final CharBuffer arrCb = CharBuffer.allocate(1024);
        System.out.println("CharBuffer.allocate  class=" + arrCb.getClass().getName()
                + " hasArray=" + arrCb.hasArray());
        final ByteBuffer heapBb = ByteBuffer.allocate(4096);
        System.out.println("ByteBuffer.allocate  class=" + heapBb.getClass().getName()
                + " hasArray=" + heapBb.hasArray());

        bench("StringCharBuffer.get()", () -> {
            int acc = 0;
            for (int i = 0; i < N; i++) {
                if (!wrapped.hasRemaining()) {
                    wrapped.position(0);
                }
                acc += wrapped.get();
            }
            if (acc == -1) {
                System.out.print("");
            }
        });

        bench("HeapCharBuffer.get()", () -> {
            int acc = 0;
            for (int i = 0; i < N; i++) {
                if (!arrCb.hasRemaining()) {
                    arrCb.position(0);
                }
                acc += arrCb.get();
            }
            if (acc == -1) {
                System.out.print("");
            }
        });

        bench("String.charAt(i)", () -> {
            int acc = 0;
            for (int i = 0; i < N; i++) {
                acc += msg.charAt(i & 1023);
            }
            if (acc == -1) {
                System.out.print("");
            }
        });

        bench("HeapByteBuffer.put(byte)", () -> {
            for (int i = 0; i < N; i++) {
                if (!heapBb.hasRemaining()) {
                    heapBb.position(0);
                }
                heapBb.put((byte) i);
            }
        });

        bench("byte[] store", () -> {
            byte[] a = new byte[4096];
            for (int i = 0; i < N; i++) {
                a[i & 4095] = (byte) i;
            }
        });
    }
}
