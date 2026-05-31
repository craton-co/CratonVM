import java.nio.*;
import java.nio.charset.*;
public class DecTest2 {
    public static void main(String[] a) throws Exception {
        CharsetDecoder dec = StandardCharsets.US_ASCII.newDecoder();
        ByteBuffer in = ByteBuffer.allocate(1);
        int firstFail = -1;
        for (int i = 0; i < 128; i++) {
            in.clear(); in.put((byte)i); in.flip();
            CharBuffer out;
            try { out = dec.decode(in); }
            catch (CharacterCodingException e) { System.out.println("i="+i+" CCE"); firstFail=i; break; }
            if (out.remaining() == 0) { System.out.println("i="+i+" UNDERFLOW remaining=0 pos="+out.position()+" limit="+out.limit()); firstFail=i; break; }
            char c = out.get();
            if (c != i) { System.out.println("i="+i+" MISMATCH got="+(int)c); firstFail=i; break; }
        }
        System.out.println(firstFail<0 ? "ALL 128 OK" : "FAILED at "+firstFail);
    }
}
