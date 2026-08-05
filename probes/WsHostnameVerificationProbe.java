/*
 * WsHostnameVerificationProbe — why did `TestSecurity2018.testCVE_2018_8034`
 * pass?
 *
 * The JUnit test is written as `@Test(expected = DeploymentException.class)`,
 * so it reports PASS for ANY DeploymentException — including one thrown
 * because the wss:// connection never completed at all. That makes it useless
 * as evidence on its own: a VM in which hostname verification does not exist
 * still scores `OK (1 test)` as long as the connect fails for some other
 * reason. (It did: before `fedb11592 fix(tls): SSLEngine.wrap must not consume
 * app data until FINISHED is reported`, every wss:// connect died in the
 * upgrade, and this test passed vacuously in ~68s against HotSpot's 2.4s.)
 *
 * This probe runs the same scenario and PRINTS the exception chain, so the
 * verdict says which of the two happened:
 *
 *   PROBE-RESULT: REJECTED-BY-HOSTNAME-VERIFICATION  ... (correct)
 *   PROBE-RESULT: REJECTED-FOR-ANOTHER-REASON        ... (vacuous pass)
 *   PROBE-RESULT: ACCEPTED                           ... (the CVE-2018-8034 bypass)
 *
 * It is a JUnit class, not a `main`, because Tomcat's `LoggingBaseTest` builds
 * its CATALINA_BASE from a JUnit `TestName` rule — driving `setUp()` by hand
 * NPEs on a null `catalinaBase`. Run it exactly like the test class — same
 * classpath, same -Dtomcat.test.* system properties:
 *
 *   <vm> -cp <tomcat suite cp>:<dir with this probe> \
 *        org.junit.runner.JUnitCore WsHostnameVerificationProbe
 */

import java.io.File;
import java.net.URI;

import javax.net.ssl.SSLContext;
import javax.net.ssl.TrustManager;

import jakarta.websocket.ClientEndpointConfig;
import jakarta.websocket.ContainerProvider;
import jakarta.websocket.WebSocketContainer;

import org.apache.catalina.Context;
import org.apache.catalina.servlets.DefaultServlet;
import org.apache.catalina.startup.Tomcat;
import org.apache.tomcat.util.net.TesterKeystoreGenerator;
import org.apache.tomcat.util.net.TesterSupport;
import org.apache.tomcat.websocket.TesterEchoServer;
import org.apache.tomcat.websocket.TesterMessageCountClient;
import org.apache.tomcat.websocket.WebSocketBaseTest;

public class WsHostnameVerificationProbe extends WebSocketBaseTest {

    @org.junit.Test
    public void probeHostnameVerification() throws Exception {
        File keystoreFile =
                TesterKeystoreGenerator.generateKeystore("localhost", "tomcat", new String[] { "localhost" }, null);

        Tomcat tomcat = getTomcatInstance();
        TesterSupport.initSsl(tomcat, keystoreFile.getAbsolutePath(), false);

        Context ctx = getProgrammaticRootContext();
        ctx.addApplicationListener(TesterEchoServer.Config.class.getName());
        Tomcat.addServlet(ctx, "default", new DefaultServlet());
        ctx.addServletMappingDecoded("/", "default");
        tomcat.start();

        WebSocketContainer wsContainer = ContainerProvider.getWebSocketContainer();
        SSLContext sslContext = SSLContext.getInstance("TLS");
        // Accepts every chain — so anything that rejects this connection can
        // ONLY be endpoint identification, never chain trust.
        sslContext.init(null, new TrustManager[] { new TesterSupport.TrustAllCerts() }, null);
        ClientEndpointConfig config = ClientEndpointConfig.Builder.create().sslContext(sslContext).build();

        // A certificate issued for `localhost` only, reached at `127.0.0.1`.
        URI target = new URI("wss://127.0.0.1:" + getPort() + TesterEchoServer.Config.PATH_ASYNC);

        long start = System.nanoTime();
        try {
            wsContainer.connectToServer(TesterMessageCountClient.TesterProgrammaticEndpoint.class, config, target);
            System.out.println("PROBE-RESULT: ACCEPTED — the connection succeeded, hostname verification did not run");
        } catch (Exception e) {
            StringBuilder chain = new StringBuilder();
            for (Throwable t = e; t != null; t = t.getCause()) {
                if (chain.length() > 0) {
                    chain.append(" <- ");
                }
                chain.append(t.getClass().getName()).append(": ").append(t.getMessage());
                if (t.getCause() == t) {
                    break;
                }
            }
            String text = chain.toString();
            String lower = text.toLowerCase(java.util.Locale.ROOT);
            boolean hostname = lower.contains("endpoint identification") || lower.contains("hostname") ||
                    lower.contains("no subject alternative") || lower.contains("does not match host");
            System.out.println("PROBE-RESULT: " +
                    (hostname ? "REJECTED-BY-HOSTNAME-VERIFICATION" : "REJECTED-FOR-ANOTHER-REASON") + " — " + text);
        }
        System.out.println("PROBE-ELAPSED-MS: " + (System.nanoTime() - start) / 1_000_000L);
    }
}
