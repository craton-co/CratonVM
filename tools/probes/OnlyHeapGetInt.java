import java.nio.ByteBuffer;
import java.nio.ByteOrder;
/** Exactly 200000 heap getInt(int) calls and nothing else, so a native
 *  invocation census divides cleanly by the accessor count. */
public final class OnlyHeapGetInt {
  public static void main(String[] a) {
    int n = a.length > 0 ? Integer.parseInt(a[0]) : 200000;
    ByteBuffer b = ByteBuffer.allocate(65536).order(ByteOrder.LITTLE_ENDIAN);
    long s = 0;
    for (int i = 0; i < n; i++) s += b.getInt((i * 7) & 65520);
    System.out.println("sink=" + s + " n=" + n);
  }
}
