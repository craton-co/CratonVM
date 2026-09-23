import java.nio.*;
public class CB5 {
    static void t(String n, Runnable r) {
        try { r.run(); System.out.println(n + " OK"); }
        catch (Throwable e) { System.out.println(n + " THREW " + e); }
    }
    public static void main(String[] a) {
        t("ByteBuffer.put(ByteBuffer)", () -> {
            ByteBuffer s = ByteBuffer.allocate(128); for (int i=0;i<100;i++) s.put((byte)i); s.flip();
            ByteBuffer d = ByteBuffer.allocate(256); d.put(s);
            if (d.position()!=100) throw new RuntimeException("moved "+d.position());
        });
        t("CharBuffer.put(char[])", () -> {
            char[] src = new char[100]; java.util.Arrays.fill(src,'q');
            CharBuffer d = CharBuffer.allocate(256); d.put(src);
            if (d.position()!=100) throw new RuntimeException("moved "+d.position());
        });
        t("CharBuffer.get(char[])", () -> {
            CharBuffer s = CharBuffer.allocate(128); for (int i=0;i<100;i++) s.put('z'); s.flip();
            char[] out = new char[100]; s.get(out);
            if (out[99]!='z') throw new RuntimeException("bad");
        });
        t("CharBuffer.put(CharBuffer)", () -> {
            CharBuffer s = CharBuffer.allocate(128); for (int i=0;i<100;i++) s.put('w'); s.flip();
            CharBuffer d = CharBuffer.allocate(256); d.put(s);
            if (d.position()!=100) throw new RuntimeException("moved "+d.position());
        });
        t("CharBuffer.put(String)", () -> {
            CharBuffer d = CharBuffer.allocate(256); d.put("hello world");
            if (d.position()!=11) throw new RuntimeException("moved "+d.position());
        });
    }
}
