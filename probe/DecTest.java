import java.nio.*;
import java.nio.charset.*;
public class DecTest {
    public static void main(String[] a) throws Exception {
        for (Charset cs : new Charset[]{StandardCharsets.US_ASCII, StandardCharsets.ISO_8859_1, StandardCharsets.UTF_8}) {
            CharsetDecoder dec = cs.newDecoder();
            ByteBuffer in = ByteBuffer.allocate(1);
            in.clear(); in.put((byte)65); in.flip();   // 'A'
            CharBuffer out = dec.decode(in);
            System.out.println(cs.name()+": remaining="+out.remaining()+" pos="+out.position()+" limit="+out.limit()
                + " cap="+out.capacity() + " -> " + (out.remaining()>0 ? "'"+out.get()+"'" : "UNDERFLOW"));
        }
    }
}
