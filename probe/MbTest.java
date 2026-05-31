import org.apache.tomcat.util.buf.MessageBytes;
public class MbTest {
    static void p(String n, boolean ok){ System.out.println((ok?"OK  ":"FAIL")+" "+n); }
    public static void main(String[] a) {
        // set CHAR then toBytes (the failing direction)
        MessageBytes mb = MessageBytes.newInstance();
        mb.setChars("expected".toCharArray(), 0, 8);
        mb.toBytes();
        p("CHAR->toBytes getByteChunk equals 'expected'",
          mb.getByteChunk().equals("expected".getBytes(java.nio.charset.StandardCharsets.ISO_8859_1), 0, 8));
        // set STRING then toChars
        MessageBytes mb2 = MessageBytes.newInstance();
        mb2.setString("expected");
        mb2.toChars();
        p("STRING->toChars getCharChunk = 'expected'",
          java.util.Arrays.equals("expected".toCharArray(), mb2.getCharChunk().getChars()==null?new char[0]:java.util.Arrays.copyOfRange(mb2.getCharChunk().getChars(), mb2.getCharChunk().getStart(), mb2.getCharChunk().getEnd())));
        // set BYTE then toString
        MessageBytes mb3 = MessageBytes.newInstance();
        mb3.setBytes("expected".getBytes(java.nio.charset.StandardCharsets.ISO_8859_1), 0, 8);
        p("BYTE->toString = 'expected'", "expected".equals(mb3.toString()));
        // set CHAR then toString
        MessageBytes mb4 = MessageBytes.newInstance();
        mb4.setChars("expected".toCharArray(), 0, 8);
        p("CHAR->toString = 'expected'", "expected".equals(mb4.toString()));
    }
}
