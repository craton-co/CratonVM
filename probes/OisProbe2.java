import java.io.*;
import java.math.BigInteger;
public class OisProbe2 {
  static class Spy extends ObjectInputStream {
    Spy(InputStream in) throws IOException { super(in); }
    @Override protected Class<?> resolveClass(ObjectStreamClass desc) throws IOException, ClassNotFoundException {
      System.out.println("  resolveClass(name=" + desc.getName() + ", svuid=" + desc.getSerialVersionUID() + ")");
      try { Class<?> c = super.resolveClass(desc); System.out.println("    -> " + c); return c; }
      catch (Throwable t) { System.out.println("    -> threw " + t.getClass().getName() + ": " + t.getMessage()); throw t; }
    }
  }
  public static void main(String[] a) {
    BigInteger FOO = new BigInteger(
      "-9702942423549012526722364838327831379660941553432801565505143675386108883970811292563757558516603356009681061" +
      "5697574744209306031461371833798723505120163874786203211176873686513374052845353833564048");
    try { Object o = new Spy(new ByteArrayInputStream(FOO.toByteArray())).readObject(); System.out.println("got " + o); }
    catch (Throwable t) { System.out.println("final: " + t.getClass().getName() + ": " + t.getMessage()); }
  }
}
