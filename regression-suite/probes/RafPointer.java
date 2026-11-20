// H8-1 §5.3: the one behavioural probe for H8-B, and it needs a flag the
// regression corpus does not set. `getFilePointer()J` is the one
// RandomAccessFile method that both the gated synthetic block and the
// unconditional bridge registrar declared, so under CRATONVM_SYNTHETIC_RAF=1
// the bridge answered a constant 0 before H8-B gated it correctly.
//
// H8-1-three-declines-that-were-not-declines-20260820-RETIRED-20260921.md
import java.io.*;
public class RafPointer {
  public static void main(String[] a) throws Exception {
    File f = File.createTempFile("rafp", ".bin"); f.deleteOnExit();
    try (RandomAccessFile r = new RandomAccessFile(f, "rw")) {
      r.writeInt(0xdeadbeef); r.writeLong(42L);
      System.out.println("afterWrite=" + r.getFilePointer());  // expect 12
      r.seek(4);
      System.out.println("afterSeek=" + r.getFilePointer());   // expect 4
    }
  }
}
