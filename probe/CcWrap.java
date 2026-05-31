import org.apache.tomcat.util.buf.CharChunk;
import java.nio.*;
import java.nio.charset.*;
public class CcWrap {
    public static void main(String[] a) throws Exception {
        CharChunk cc = new CharChunk();
        cc.setChars("expected".toCharArray(), 0, 8);
        System.out.println("CharChunk: length()="+cc.length()+" charAt(0)="+cc.charAt(0)+" toString="+cc.toString());
        CharBuffer cb = CharBuffer.wrap(cc);   // wrap(CharSequence)
        System.out.println("wrap(CharChunk): remaining="+cb.remaining()+" limit="+cb.limit()+" pos="+cb.position());
        CharsetEncoder enc = StandardCharsets.ISO_8859_1.newEncoder();
        ByteBuffer bb = enc.encode(cb);
        System.out.println("encoded: limit="+bb.limit()+" content='"+new String(bb.array(),0,bb.limit(),StandardCharsets.ISO_8859_1)+"'");
    }
}
