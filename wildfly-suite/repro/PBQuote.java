import java.util.*;
public class PBQuote {
  public static void main(String[] a) throws Exception {
    String java = "\"" + System.getProperty("java.home") + "/bin/java\"";  // QUOTED, no .exe (like WildFly launcher)
    System.out.println("program=[" + java + "]");
    try { Process p = new ProcessBuilder(Arrays.asList(java, "-version")).redirectErrorStream(true).start();
      int rc = p.waitFor(); System.out.println("rc=" + rc); }
    catch (Throwable t) { System.out.println("FAILED: " + t); }
  }
}
