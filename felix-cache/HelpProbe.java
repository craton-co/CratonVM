import joptsimple.OptionParser;
import java.io.ByteArrayOutputStream;
public class HelpProbe {
  public static void main(String[] a) throws Exception {
    OptionParser p = new OptionParser(false);
    p.accepts("override", "Optional property that should override values set in server.properties file").withRequiredArg().ofType(String.class);
    p.accepts("version", "Print version information and exit.");
    System.out.println("=== printHelpOn(System.out) ===");
    p.printHelpOn(System.out);
    System.out.println("=== via ByteArrayOutputStream ===");
    ByteArrayOutputStream bos = new ByteArrayOutputStream();
    p.printHelpOn(bos);
    String s = bos.toString();
    System.out.println("len="+s.length());
    System.out.println("["+s+"]");
    System.out.println("=== done ===");
  }
}
