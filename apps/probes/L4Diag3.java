import java.nio.*;

/** Which of the two moved wrong: the cursor, or the bulk read? */
public class L4Diag3 {
    static void show(String k, CharBuffer b) {
        System.out.println(k + " p=" + b.position() + " l=" + b.limit() + " c=" + b.capacity());
    }
    static void run(String k, CharBuffer b) {
        b.clear();
        b.put(new char[]{'a', 'b', 'c', 'd'}, 0, 4);
        show(k + " after put", b);
        b.flip();
        show(k + " after flip", b);
        char one = b.get();
        System.out.println(k + " get() = " + one);
        show(k + " after get()", b);
        char[] out = new char[4];
        b.get(out, 1, 2);
        System.out.println(k + " get(out,1,2) = " + new String(out).replace('\0', '.'));
        show(k + " after bulk get", b);
        // and the same on the absolute accessors, which do not move
        b.clear();
        System.out.println(k + " charAt(0)=" + b.charAt(0) + " charAt(1)=" + b.charAt(1));
        b.position(1);
        System.out.println(k + " after position(1), charAt(0)=" + b.charAt(0));
        System.out.println(k + " toString=" + b.toString());
    }
    public static void main(String[] a) {
        run("alloc", CharBuffer.allocate(4));
        run("view ", ByteBuffer.allocate(8).asCharBuffer());
        // the same question for the other view types
        IntBuffer iv = ByteBuffer.allocate(16).asIntBuffer();
        iv.put(new int[]{1, 2, 3, 4}, 0, 4);
        iv.flip();
        int i1 = iv.get();
        int[] io = new int[4];
        iv.get(io, 1, 2);
        System.out.println("int view get()=" + i1 + " bulk=" + java.util.Arrays.toString(io));
        ShortBuffer sv = ByteBuffer.allocate(8).asShortBuffer();
        sv.put(new short[]{1, 2, 3, 4}, 0, 4);
        sv.flip();
        short s1 = sv.get();
        short[] so = new short[4];
        sv.get(so, 1, 2);
        System.out.println("short view get()=" + s1 + " bulk=" + java.util.Arrays.toString(so));
        System.out.println("DONE L4Diag3");
    }
}
