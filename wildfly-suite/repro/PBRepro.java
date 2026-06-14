import java.util.*;
public class PBRepro {
  public static void main(String[] a) throws Exception {
    String java = System.getProperty("java.home") + "/bin/java.exe";
    List<String> cmd = Arrays.asList(java, "-version");
    System.out.println("launching: " + java);
    try {
      Process p = new ProcessBuilder(cmd).redirectErrorStream(true).start();
      byte[] out = p.getInputStream().readAllBytes();
      int rc = p.waitFor();
      System.out.println("rc=" + rc + " outlen=" + out.length);
    } catch (Throwable t) { System.out.println("FAILED: " + t); }
  }
}
