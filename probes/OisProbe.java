import java.io.*;
import java.math.BigInteger;
public class OisProbe {
  public static void main(String[] a) {
    BigInteger FOO = new BigInteger(
      "-9702942423549012526722364838327831379660941553432801565505143675386108883970811292563757558516603356009681061" +
      "5697574744209306031461371833798723505120163874786203211176873686513374052845353833564048");
    byte[] b = FOO.toByteArray();
    System.out.println("first bytes = " + String.format("%02x %02x %02x %02x", b[0], b[1], b[2], b[3]));
    try {
      Object o = new ObjectInputStream(new ByteArrayInputStream(b)).readObject();
      System.out.println("no throw, got " + o);
    } catch (Throwable t) {
      System.out.println("threw " + t.getClass().getName() + ": " + t.getMessage());
      for (StackTraceElement e : t.getStackTrace()) System.out.println("    at " + e);
      Throwable c = t.getCause();
      while (c != null) { System.out.println("  caused by " + c.getClass().getName() + ": " + c.getMessage()); c = c.getCause(); }
    }
  }
}
