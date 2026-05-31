import org.apache.tomcat.util.buf.*;
import java.util.Arrays;
public class MbChar {
    public static void main(String[] a) {
        char[] EXPECTED = "expected".toCharArray();
        MessageBytes mb = MessageBytes.newInstance();
        mb.setChars(EXPECTED, 0, EXPECTED.length);
        mb.toChars();
        CharChunk cc = mb.getCharChunk();
        char[] got = cc.getChars();
        System.out.println("getChars().length="+(got==null?"null":got.length)+" start="+cc.getStart()+" end="+cc.getEnd()+" len="+cc.getLength());
        System.out.println("arrayEquals(EXPECTED, getChars())="+Arrays.equals(EXPECTED, got));
        System.out.println("got="+(got==null?"null":Arrays.toString(got)));
        // toString
        System.out.println("toString='"+mb.toString()+"'");
        // toBytes path
        MessageBytes mb2 = MessageBytes.newInstance();
        mb2.setChars(EXPECTED, 0, EXPECTED.length);
        mb2.toBytes();
        ByteChunk bc = mb2.getByteChunk();
        System.out.println("byteChunk len="+bc.getLength()+" equals EXPECTED_BYTES="+bc.equals("expected".getBytes(java.nio.charset.StandardCharsets.ISO_8859_1),0,8));
    }
}
