import java.io.*;
public class SysOutProbe {
  public static void main(String[] a) throws Exception {
    PrintStream orig = System.out;
    ByteArrayOutputStream bos = new ByteArrayOutputStream();
    System.setOut(new PrintStream(bos, true));
    System.out.println("captured-line");
    System.out.print("captured-print");
    System.out.flush();
    System.setOut(orig);
    System.out.println("captured bytes = " + bos.size() + " content=[" + bos.toString().trim() + "]");
    // and the indirect form Spring uses: a PrintWriter over System.out captured later
    ByteArrayOutputStream b2 = new ByteArrayOutputStream();
    PrintStream ps = new PrintStream(b2, true);
    PrintStream saved = System.out;
    System.setOut(ps);
    Runnable r = () -> System.out.println("lambda-line");
    r.run();
    System.setOut(saved);
    System.out.println("lambda capture = " + b2.size() + " content=[" + b2.toString().trim() + "]");
  }
}
