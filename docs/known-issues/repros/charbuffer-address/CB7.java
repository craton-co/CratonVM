import java.lang.reflect.*;
import java.nio.*;
public class CB7 {
    static Field ADDR;
    static long addr(Buffer b) { try { return ADDR.getLong(b); } catch (Exception e) { return -999; } }
    public static void main(String[] a) throws Exception {
        ADDR = Buffer.class.getDeclaredField("address"); ADDR.setAccessible(true);
        CharBuffer c = CharBuffer.allocate(128);
        System.out.println("after allocate      : " + addr(c));
        c.put('x');
        System.out.println("after put(char)     : " + addr(c));
        c.put("yz");
        System.out.println("after put(String)   : " + addr(c));
        c.flip();
        System.out.println("after flip()        : " + addr(c));
        c.position(0);
        System.out.println("after position(0)   : " + addr(c));
        c.limit(3);
        System.out.println("after limit(3)      : " + addr(c));
        c.rewind();
        System.out.println("after rewind()      : " + addr(c));
        c.clear();
        System.out.println("after clear()       : " + addr(c));
        char[] arr = new char[4];
        c.limit(3); c.get(arr, 0, 3);
        System.out.println("after get(char[])   : " + addr(c));
        CharBuffer w = CharBuffer.wrap(new char[16]);
        System.out.println("wrap(char[])        : " + addr(w));
    }
}
