import org.eclipse.jetty.server.Server;
import org.eclipse.jetty.server.handler.AbstractHandler;
import org.eclipse.jetty.server.Request;
import jakarta.servlet.http.HttpServletRequest;
import jakarta.servlet.http.HttpServletResponse;

public class JettyFuncProbe {
    public static void main(String[] args) throws Exception {
        Server server = new Server(0);
        server.setHandler(new AbstractHandler() {
            public void handle(String t, Request br, HttpServletRequest q, HttpServletResponse r) {
                br.setHandled(true);
            }
        });
        try {
            server.start();
            System.out.println("Jetty Server.start() returned");
            System.out.println("isStarted=" + server.isStarted());
            try { System.out.println("URI=" + server.getURI()); }
            catch (Throwable t) { System.out.println("URI lookup soft-fail: " + t.getClass().getSimpleName()); }
        } catch (Throwable t) {
            String msg = String.valueOf(t.getMessage());
            if (msg != null && (msg.contains("channel not bound")
                    || msg.contains("Address already in use")
                    || msg.contains("Permission denied")
                    || msg.contains("Could not bind"))) {
                System.out.println("Server.start() reached socket bind (soft): " + t.getClass().getSimpleName());
            } else {
                t.printStackTrace();
                System.exit(1);
            }
        }
        try { server.stop(); } catch (Throwable ignore) {}
        System.out.println("OK");
        // Jetty leaves non-daemon thread pools behind that keep the VM
        // alive; force exit so the probe's exit code is observable.
        System.exit(0);
    }
}
