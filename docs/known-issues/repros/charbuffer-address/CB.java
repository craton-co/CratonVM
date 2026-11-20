import java.nio.*;
/** Mirrors com.sun.tools.javac.file.BaseFileManager.decode's grow-and-copy:
 *  it allocates a CharBuffer, decodes into it, flips, and puts it into a
 *  bigger one. javac hit AIOOBE inside CharBuffer.putBuffer doing this. */
public class CB {
    static void t(String label, int srcCap, int used, int dstCap) {
        try {
            CharBuffer src = CharBuffer.allocate(srcCap);
            for (int i = 0; i < used; i++) src.put((char) ('a' + (i % 26)));
            src.flip();
            CharBuffer dst = CharBuffer.allocate(dstCap);
            dst.put(src);
            dst.flip();
            int n = dst.remaining();
            String s = dst.toString();
            System.out.println(label + " OK moved=" + n + " len=" + s.length()
                + (n == used && s.length() == used ? "" : "  <-- WRONG, expected " + used));
        } catch (Throwable e) {
            System.out.println(label + " THREW " + e);
        }
    }
    public static void main(String[] a) {
        t("small", 128, 100, 256);
        t("exact", 100, 100, 100);
        t("grow-2x", 2048, 2048, 4096);
        t("javac-ish", 8192, 8192, 16384);
        t("big", 1 << 16, 1 << 16, 1 << 17);
        // Sliced/positioned source, which decode() also produces.
        try {
            CharBuffer src = CharBuffer.allocate(64);
            for (int i = 0; i < 64; i++) src.put('x');
            src.position(10); src.limit(40);
            CharBuffer sl = src.slice();
            CharBuffer dst = CharBuffer.allocate(64);
            dst.put(sl);
            System.out.println("slice OK moved=" + dst.position() + (dst.position() == 30 ? "" : " <-- WRONG, expected 30"));
        } catch (Throwable e) { System.out.println("slice THREW " + e); }
    }
}
