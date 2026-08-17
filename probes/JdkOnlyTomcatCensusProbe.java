import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.net.HttpURLConnection;
import java.net.URI;
import org.apache.catalina.Context;
import org.apache.catalina.startup.Tomcat;

/**
 * G60-1 N3 — the census workload that is an APPLICATION rather than a vector.
 *
 * <p>G60-1 §4's first caveat is the whole limitation of that record: its 81
 * shadow rows came from one reflection/boxing vector, which is a floor for the
 * population and not the population. §5 N3 asks for a run against a real
 * application, and APP-READINESS-20260812.md §0 names the one that is measured
 * to survive {@code --jdk-only} end to end — embedded Tomcat booting, serving,
 * and shutting down.
 *
 * <p>So this boots Tomcat on an ephemeral port, serves a GET and a 404 through a
 * real {@code HttpURLConnection}, and stops. It is deliberately NOT a
 * correctness probe: the assertions are only enough to prove the server really
 * served, because what the run is FOR is
 * {@code --jdk-only --jdk-only-report}'s violation list over a workload that
 * touches the JDK the way an application does.
 *
 * <p>Read the report's {@code observation_sink} object before reading its
 * {@code violations[]} as a population. That is the point of the workload: a
 * vector fits inside the 256-row sink and an application need not, and until the
 * sink reported its own saturation the two were indistinguishable in the JSON.
 *
 * <p>Classpath is composed by hand from Tomcat's built output — see
 * APP-READINESS-20260812.md §1, which explains why that is the cheap route and
 * what it does not buy.
 */
public final class JdkOnlyTomcatCensusProbe {

    public static void main(String[] args) throws Exception {
        String baseDir = System.getProperty("java.io.tmpdir") + "/jdk-only-tomcat-census";
        Tomcat tomcat = new Tomcat();
        tomcat.setBaseDir(baseDir);
        tomcat.setPort(0);
        // Force the connector into existence before start() so the port is bound
        // by the time we ask for it.
        tomcat.getConnector();

        Context ctx = tomcat.addContext("", baseDir);
        Tomcat.addServlet(ctx, "hello", new jakarta.servlet.http.HttpServlet() {
            @Override
            protected void doGet(jakarta.servlet.http.HttpServletRequest req,
                    jakarta.servlet.http.HttpServletResponse resp) throws java.io.IOException {
                resp.setContentType("text/plain");
                resp.getWriter().print("hello from " + req.getRequestURI());
            }
        });
        ctx.addServletMappingDecoded("/hello", "hello");

        tomcat.start();
        int port = tomcat.getConnector().getLocalPort();
        System.out.println("CK tomcat.started port>0=" + (port > 0));

        System.out.println("CK get.body=" + get("http://127.0.0.1:" + port + "/hello"));
        System.out.println("CK missing.status=" + status("http://127.0.0.1:" + port + "/nope"));

        tomcat.stop();
        tomcat.destroy();
        System.out.println("PASS JdkOnlyTomcatCensusProbe");
    }

    static String get(String url) throws Exception {
        HttpURLConnection c = (HttpURLConnection) URI.create(url).toURL().openConnection();
        c.setRequestMethod("GET");
        try (BufferedReader r = new BufferedReader(new InputStreamReader(c.getInputStream()))) {
            StringBuilder sb = new StringBuilder();
            for (String line = r.readLine(); line != null; line = r.readLine()) {
                sb.append(line);
            }
            return sb.toString();
        } finally {
            c.disconnect();
        }
    }

    static int status(String url) throws Exception {
        HttpURLConnection c = (HttpURLConnection) URI.create(url).toURL().openConnection();
        c.setRequestMethod("GET");
        try {
            return c.getResponseCode();
        } finally {
            c.disconnect();
        }
    }
}
