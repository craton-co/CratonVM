package io.netty.handler.codec.http;
import io.netty.util.AsciiString;
import java.nio.ByteBuffer;
public final class CatchRate {
    private static final IllegalArgumentException E = new IllegalArgumentException() {
        @Override public synchronized Throwable fillInStackTrace() { return this; }
    };
    private static int step(int state, char c) {
        if ((c & ~15) == 0) { switch (c) { case 0x0: throw E; case 0x0b: throw E; case '\f': throw E; default: break; } }
        switch (state) {
            case 0: switch (c) { case '\r': return 1; case '\n': return 2; default: break; } break;
            case 1: if (c == '\n') { return 2; } throw E;
            case 2: switch (c) { case '\t': case ' ': return 0; default: throw E; }
            default: break;
        }
        return state;
    }
    private static void old(CharSequence seq) {
        int state = 0;
        for (int i = 0; i < seq.length(); i++) { state = step(state, seq.charAt(i)); }
        if (state != 0) { throw E; }
    }
    public static void main(String[] a) {
        int n = Integer.parseInt(a[0]);
        int WINDOW = 65536;
        byte[] arr = new byte[4];
        ByteBuffer buffer = ByteBuffer.wrap(arr);
        AsciiString s = new AsciiString(buffer, false);
        long caught = 0, total = 0;
        int windows = Math.max(1, n / WINDOW);
        long windowStride = 4294967296L / windows;
        long t0 = System.nanoTime();
        for (int w = 0; w < windows; w++) {
            int i = (int) (Integer.MIN_VALUE + w * windowStride);
            for (int k = 0; k < WINDOW; k++) {
                buffer.putInt(0, i);
                try { old(s); } catch (IllegalArgumentException ig) { caught++; }
                total++; i++;
            }
        }
        long t1 = System.nanoTime();
        System.out.printf("no-assert loop %8.2f ns/iter  caught=%d/%d (%.2f%%)%n",
            (double)(t1-t0)/total, caught, total, 100.0*caught/total);
    }
}
