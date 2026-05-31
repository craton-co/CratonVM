import java.nio.*;
import java.nio.charset.*;
public class EncArr2 {
    public static void main(String[] a) throws Exception {
        CharBuffer cb = CharBuffer.wrap("expected".toCharArray());
        System.out.println("input cb: pos="+cb.position()+" limit="+cb.limit()+" remaining="+cb.remaining());
        CharsetEncoder enc = StandardCharsets.ISO_8859_1.newEncoder();
        ByteBuffer bb = enc.encode(cb);
        System.out.println("out bb: limit="+bb.limit()+" cap="+bb.capacity()+" hasArray="+bb.hasArray());
        byte[] arr = bb.array();
        System.out.println("arr.len="+arr.length+" content="+new String(arr, StandardCharsets.ISO_8859_1));
        // also test Charset.encode (not encoder)
        ByteBuffer bb2 = StandardCharsets.ISO_8859_1.encode(CharBuffer.wrap("hello".toCharArray()));
        System.out.println("Charset.encode: limit="+bb2.limit()+" content="+new String(bb2.array(),0,bb2.limit(),StandardCharsets.ISO_8859_1));
    }
}
