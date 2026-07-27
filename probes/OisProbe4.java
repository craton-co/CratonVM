import java.io.*;
import java.math.BigInteger;
public class OisProbe4 {
  static ClassLoader lud() {
    try { Class<?> vm = Class.forName("jdk.internal.misc.VM");
          var m = vm.getDeclaredMethod("latestUserDefinedLoader"); m.setAccessible(true);
          return (ClassLoader) m.invoke(null); }
    catch (Throwable t) { System.out.println("    lud() failed: " + t); return null; }
  }
  static class Spy extends ObjectInputStream {
    Spy(InputStream in) throws IOException { super(in); }
    @Override protected Class<?> resolveClass(ObjectStreamClass desc) throws IOException, ClassNotFoundException {
      String n = desc.getName();
      ClassLoader l = lud();
      System.out.println("    latestUserDefinedLoader (from resolveClass) = " + l);
      try { System.out.println("    forName(n,false,lud) -> " + Class.forName(n, false, l)); }
      catch (Throwable t) { System.out.println("    forName(n,false,lud) threw " + t.getClass().getSimpleName()); }
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
