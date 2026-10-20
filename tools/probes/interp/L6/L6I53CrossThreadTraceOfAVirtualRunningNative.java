// Interpreter round i2, the i53 retirement: another thread's
// `Thread.getStackTrace()` of a VIRTUAL thread running a long native
// (docs/internal/fixed-bugs/interpreter-L6-a-thread-suspended-in-a-running-native-lists-no-frames-FIXED-20261007.md).
//
// The virtual thread `worker` compresses 16 MiB at level 9 in one
// `Deflater.deflate` call, in a loop (`deflateBytesBytes` runs for seconds,
// in C, never blocking). `main` reads its trace twice, printing the frames
// from the top down to the probe's `body` (the JDK frames between two probe
// frames counted, not named), and whether the read came back promptly (one
// call runs ~3 s; HotSpot answers in milliseconds). A handshake pause did
// not excuse a virtual thread's native, so the read waited for the native
// to return and listed the thread where it stood then, its caller on top.
//
// Expected output (HotSpot 25.0.3, default flags, the Windows box):
//
//   round 1
//   prompt: true
//   trace: deflateBytesBytes(native) < 2 JDK < body
//   round 2
//   prompt: true
//   trace: deflateBytesBytes(native) < 2 JDK < body
//   done
import java.util.zip.Deflater;

public class L6I53CrossThreadTraceOfAVirtualRunningNative {
    static volatile boolean inCall;
    static volatile boolean stop;

    static void body() {
        byte[] in = new byte[16 << 20];
        long seed = 12345;
        for (int i = 0; i < (1 << 20); i++) {
            seed = seed * 6364136223846793005L + 1442695040888963407L;
            in[i] = (byte) ('a' + (int) ((seed >>> 33) & 7));
        }
        for (int k = 1; k < 16; k++) {
            System.arraycopy(in, 0, in, k << 20, 1 << 20);
        }
        byte[] out = new byte[in.length + (1 << 20)];
        Deflater d = new Deflater(Deflater.BEST_COMPRESSION);
        while (!stop) {
            d.reset();
            d.setInput(in);
            d.finish();
            inCall = true;
            d.deflate(out);
        }
        d.end();
    }

    static String trace(Thread t) {
        StackTraceElement[] st = t.getStackTrace();
        StringBuilder sb = new StringBuilder("trace:");
        int jdk = 0;
        boolean first = true;
        for (StackTraceElement e : st) {
            String c = e.getClassName();
            if (!first && (c.startsWith("java.") || c.startsWith("jdk."))) {
                jdk++;
                continue;
            }
            if (jdk > 0) {
                sb.append(" < ").append(jdk).append(" JDK");
                jdk = 0;
            }
            sb.append(first ? " " : " < ").append(e.getMethodName());
            if (e.isNativeMethod()) {
                sb.append("(native)");
            }
            first = false;
            if (e.getMethodName().equals("body")) {
                break;
            }
        }
        return sb.toString();
    }

    public static void main(String[] args) throws Exception {
        Thread worker = Thread.ofVirtual().name("worker").start(L6I53CrossThreadTraceOfAVirtualRunningNative::body);
        while (!inCall) {
            Thread.sleep(1);
        }
        for (int round = 1; round <= 2; round++) {
            Thread.sleep(300);
            System.out.println("round " + round);
            long start = System.nanoTime();
            String trace = trace(worker);
            System.out.println("prompt: " + ((System.nanoTime() - start) / 1_000_000 < 1500));
            System.out.println(trace);
        }
        stop = true;
        worker.join();
        System.out.println("done");
    }
}
