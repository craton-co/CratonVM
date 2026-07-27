import java.nio.*;
import java.nio.charset.*;
import java.util.*;
public class OffProbe {
  static void enc(String label, ByteBuffer out, byte[] arr) {
    CharsetEncoder e = StandardCharsets.ISO_8859_1.newEncoder();
    CharBuffer in = CharBuffer.wrap("\u00a3");
    CoderResult r = e.encode(in, out, true);
    System.out.println(label + " res=" + r + " pos=" + out.position() + " arrayOffset=" + (out.hasArray()? out.arrayOffset() : -1)
        + " backing=" + Arrays.toString(arr));
  }
  public static void main(String[] a) {
    byte[] arr = new byte[12];
    enc("wrap(0)   ", ByteBuffer.wrap(arr), arr);
    Arrays.fill(arr, (byte) 0);
    enc("wrap(4,8) ", ByteBuffer.wrap(arr, 4, 8).slice(), arr);
    Arrays.fill(arr, (byte) 0);
    ByteBuffer b = ByteBuffer.wrap(arr); b.position(6);
    enc("slice@6   ", b.slice(), arr);
  }
}
