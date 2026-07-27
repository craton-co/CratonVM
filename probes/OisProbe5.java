import java.io.*;
import java.lang.reflect.*;
import java.math.BigInteger;
public class OisProbe5 {
  static class Spy extends ObjectInputStream {
    Spy(InputStream in) throws IOException { super(in); }
    @Override protected Class<?> resolveClass(ObjectStreamClass desc) throws IOException, ClassNotFoundException {
      String n = desc.getName();
      try {
        Method m = ObjectInputStream.class.getDeclaredMethod("latestUserDefinedLoader");
        m.setAccessible(true);
        ClassLoader l = (ClassLoader) m.invoke(null);
        System.out.println("    OIS.latestUserDefinedLoader = " + l);
        try { System.out.println("    forName(n,false,thatLoader) -> " + Class.forName(n, false, l)); }
        catch (Throwable t) { System.out.println("    forName(n,false,thatLoader) threw " + t.getClass().getSimpleName()); }
        if (l != null) {
          try { System.out.println("    l.loadClass(n) -> " + l.loadClass(n)); }
          catch (Throwable t) { System.out.println("    l.loadClass(n) threw " + t.getClass().getSimpleName()); }
        }
      } catch (Throwable t) { System.out.println("    reflect failed: " + t); }
      return super.resolveClass(desc);
    }
  }
  public static void main(String[] a) {
    BigInteger FOO = new BigInteger(
      "-9702942423549012526722364838327831379660941553432801565505143675386108883970811292563757558516603356009681061" +
      "5697574744209306031461371833798723505120163874786203211176873686513374052845353833564048");
    try { System.out.println("got " + new Spy(new ByteArrayInputStream(FOO.toByteArray())).readObject()); }
    catch (Throwable t) { System.out.println("final: " + t.getClass().getName()); }
  }
}
