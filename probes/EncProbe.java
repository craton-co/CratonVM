import java.nio.*;
import java.nio.charset.*;
public class EncProbe {
  static void t(String label, ByteBuffer out) {
    CharsetEncoder enc = StandardCharsets.ISO_8859_1.newEncoder()
        .onMalformedInput(CodingErrorAction.REPLACE).onUnmappableCharacter(CodingErrorAction.REPLACE);
    CharBuffer in = CharBuffer.wrap("H\u00e9llo W\u00f6rld");
    int pos0 = out.position();
    CoderResult r = enc.encode(in, out, true);
    System.out.println(label + " result=" + r + " outPos " + pos0 + "->" + out.position()
        + " inRemaining=" + in.remaining() + " isDirect=" + out.isDirect());
    CoderResult r2 = enc.flush(out);
    System.out.println(label + " flush=" + r2 + " outPos=" + out.position());
  }
  public static void main(String[] a) {
    t("heap  ", ByteBuffer.allocate(64));
    t("direct", ByteBuffer.allocateDirect(64));
    ByteBuffer d = ByteBuffer.allocateDirect(64);
    d.position(8);
    ByteBuffer sl = d.slice();
    t("dslice", sl);
  }
}
