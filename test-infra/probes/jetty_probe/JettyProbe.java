import org.eclipse.jetty.util.Jetty;
public class JettyProbe {
    public static void main(String[] args) {
        System.out.println("Jetty version: " + Jetty.VERSION);
        System.out.println("Powered by: " + Jetty.POWERED_BY);
        System.out.println("OK");
    }
}
