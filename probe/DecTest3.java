import java.nio.*;
import java.nio.charset.*;
public class DecTest3 {
    public static void main(String[] a) throws Exception {
        // EXACTLY mirror CharsetUtil.isAsciiSuperset
        Charset charset = StandardCharsets.US_ASCII;
        CharsetDecoder decoder = charset.newDecoder();
        ByteBuffer inBytes = ByteBuffer.allocate(1);
        CharBuffer outChars;
        for (int i = 0; i < 128; i++) {
            inBytes.clear();
            inBytes.put((byte) i);
            inBytes.flip();
            try { outChars = decoder.decode(inBytes); }
            catch (CharacterCodingException e) { System.out.println("i="+i+" return false (CCE)"); return; }
            try {
                char got = outChars.get();
                if (got != i) { System.out.println("i="+i+" mismatch got="+(int)got); return; }
            } catch (BufferUnderflowException e) { System.out.println("i="+i+" BufferUnderflow -> return false"); return; }
        }
        System.out.println("isAsciiSuperset=true (ALL OK)");
    }
}
