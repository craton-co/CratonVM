import java.nio.*;
import java.nio.charset.*;
public class EncArr {
    public static void main(String[] a) throws Exception {
        CharsetEncoder enc = StandardCharsets.ISO_8859_1.newEncoder();
        ByteBuffer bb = enc.encode(CharBuffer.wrap("expected".toCharArray()));
        System.out.println("hasArray="+bb.hasArray()+" limit="+bb.limit()+" arrayOffset(try)...");
        try {
            byte[] arr = bb.array();
            System.out.println("array() OK len="+arr.length+" offset="+bb.arrayOffset());
        } catch (UnsupportedOperationException e) {
            System.out.println("array() threw UnsupportedOperationException (BUG)");
        }
    }
}
