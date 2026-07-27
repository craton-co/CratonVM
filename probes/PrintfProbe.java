import java.io.*;
public class PrintfProbe {
  public static void main(String[] a) {
    StringWriter sw = new StringWriter();
    PrintWriter w = new PrintWriter(sw, true);
    try { w.printf("%18s = %s%n", "Type", null); System.out.println("1 printf(null) ok -> [" + sw.toString().replace("\n","\n") + "]"); }
    catch (Throwable t) { System.out.println("1 printf(null) threw " + t); }
    try { System.out.println("2 format Class -> [" + String.format("%18s = %s%n", "Type", String.class).replace("\n","\n") + "]"); }
    catch (Throwable t) { System.out.println("2 threw " + t); }
    StringWriter s2 = new StringWriter();
    PrintWriter w2 = new PrintWriter(s2, true);
    try { w2.printf("%s:%n", "MockHttpServletResponse"); w2.flush(); System.out.println("3 printf header -> [" + s2.toString().replace("\n","\n") + "]"); }
    catch (Throwable t) { System.out.println("3 threw " + t); }
  }
}
