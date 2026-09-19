import java.nio.ByteBuffer;

/** Is ByteBuffer.slice(int,int) served on this VM? Its siblings are registered; it is not. */
public class SliceProbe {
    static void t(String label, Call c) {
        String out;
        try { Object v = c.run(); out = String.valueOf(v); }
        catch (Throwable e) { out = e.getClass().getName() + ": " + e.getMessage(); }
        System.out.println(label + " => " + out);
    }
    interface Call { Object run() throws Exception; }

    public static void main(String[] a) {
        byte[] backing = new byte[16];
        for (int i = 0; i < 16; i++) backing[i] = (byte) (i * 7);

        ByteBuffer heap = ByteBuffer.wrap(backing);
        t("heap.slice(4,8).capacity",   () -> heap.slice(4, 8).capacity());
        t("heap.slice(4,8).arrayOffset",() -> heap.slice(4, 8).arrayOffset());
        t("heap.slice(4,8).get(0)",     () -> heap.slice(4, 8).get(0));
        t("heap.slice(4,8).hasArray",   () -> heap.slice(4, 8).hasArray());
        t("heap.slice(0,16).capacity",  () -> heap.slice(0, 16).capacity());

        ByteBuffer direct = ByteBuffer.allocateDirect(16);
        for (int i = 0; i < 16; i++) direct.put(i, (byte) (i * 7));
        t("direct.slice(4,8).capacity", () -> direct.slice(4, 8).capacity());
        t("direct.slice(4,8).get(0)",   () -> direct.slice(4, 8).get(0));
        t("direct.slice(4,8).isDirect", () -> direct.slice(4, 8).isDirect());

        // The siblings, as the control: these ARE registered.
        t("int.slice(1,2).capacity",    () -> java.nio.IntBuffer.allocate(8).slice(1, 2).capacity());
        t("char.slice(1,2).capacity",   () -> java.nio.CharBuffer.allocate(8).slice(1, 2).capacity());

        // Bounds, which the JDK specifies as IndexOutOfBounds.
        t("heap.slice(-1,2)",           () -> heap.slice(-1, 2).capacity());
        t("heap.slice(10,10)",          () -> heap.slice(10, 10).capacity());
        System.out.println("RESULT done");
    }
}
