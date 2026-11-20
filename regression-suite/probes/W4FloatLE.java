import java.nio.*;
public class W4FloatLE {
    static void one(String tag, boolean direct, ByteOrder o) {
        ByteBuffer p = (direct ? ByteBuffer.allocateDirect(16) : ByteBuffer.allocate(16)).order(o);
        FloatBuffer f = p.asFloatBuffer();
        System.out.println("CK " + tag + ".viewClass " + f.getClass().getName());
        System.out.println("CK " + tag + ".viewOrder " + f.order());
        System.out.println("CK " + tag + ".viewCap " + f.capacity());
        f.put(0, 1.0f);
        System.out.println("CK " + tag + ".readBack " + f.get(0));
        System.out.println("CK " + tag + ".parentFloat " + p.getFloat(0));
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 8; i++) sb.append(String.format("%02x", p.get(i)));
        System.out.println("CK " + tag + ".bytes " + sb);
    }
    public static void main(String[] a) {
        one("heap.be", false, ByteOrder.BIG_ENDIAN);
        one("heap.le", false, ByteOrder.LITTLE_ENDIAN);
        one("direct.be", true, ByteOrder.BIG_ENDIAN);
        one("direct.le", true, ByteOrder.LITTLE_ENDIAN);
        System.out.println("PASS W4FloatLE");
        System.out.flush();
        Runtime.getRuntime().halt(0);
    }
}
