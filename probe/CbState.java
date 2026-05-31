import java.nio.*;
public class CbState {
    public static void main(String[] a) {
        CharBuffer cb = CharBuffer.wrap("expected".toCharArray());
        System.out.println("hasArray="+cb.hasArray()+" pos="+cb.position()+" limit="+cb.limit()+" remaining="+cb.remaining());
        // direct read
        System.out.println("get(0)="+cb.get(0)+" toString="+cb.toString());
    }
}
