import java.io.*;
import java.math.BigInteger;
public class OisProbe3 {
  static class Spy extends ObjectInputStream {
    Spy(InputStream in) throws IOException { super(in); }
    @Override protected Class<?> resolveClass(ObjectStreamClass desc) throws IOException, ClassNotFoundException {
      String n = desc.getName();
      System.out.println("  resolveClass(" + n + ")");
      try { Class<?> d = Class.forName(n, false, getClass().getClassLoader());
            System.out.println("    direct forName -> " + d); }
      catch (Throwable t) { System.out.println("    direct forName threw " + t.getClass().getSimpleName()); }
      try { Class<?> c = super.resolveClass(desc);
            System.out.println("    super -> " + c + " loader=" + (c==null?null:c.getClassLoader())
              + " synthetic=" + (c==null?"-":c.isSynthetic()) + " iface=" + (c==null?"-":c.isInterface()));
            return c; }
      catch (Throwable t) { System.out.println("    super threw " + t.getClass().getName()); throw t; }
    }
  }
  public static void main(String[] a) {
    BigInteger FOO = new BigInteger(
      "-9702942423549012526722364838327831379660941553432801565505143675386108883970811292563757558516603356009681061" +
      "5697574744209306031461371833798723505120163874786203211176873686513374052845353833564048");
    try { System.out.println("got " + new Spy(new ByteArrayInputStream(FOO.toByteArray())).readObject()); }
    catch (Throwable t) { System.out.println("final: " + t.getClass().getName() + ": " + t.getMessage()); }
  }
}
