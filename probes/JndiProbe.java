import javax.naming.InitialContext;
public class JndiProbe {
  public static void main(String[] a) {
    try { Object env = new InitialContext().getEnvironment(); System.out.println("getEnvironment -> " + env); }
    catch (Throwable t) { System.out.println("getEnvironment threw " + t.getClass().getName() + ": " + t.getMessage()); }
  }
}
