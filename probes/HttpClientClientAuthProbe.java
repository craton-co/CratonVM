import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.KeyStore;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.atomic.AtomicBoolean;
import javax.net.ssl.KeyManagerFactory;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLServerSocket;
import javax.net.ssl.SSLServerSocketFactory;
import javax.net.ssl.SSLSocket;
import javax.net.ssl.TrustManagerFactory;

/**
 * Reproduction harness for
 * docs/known-issues/springboot/jdkclienthttprequestfactory-certificaterequired-alert-20260806.md
 *
 * The shape of {@code AbstractClientHttpRequestFactoryBuilderTests.connectWithSslBundle}
 * reduced to its client half: a TLS server that DEMANDS a client certificate
 * (the test's connector sets {@code Ssl.ClientAuth.NEED}) and CratonVM's own
 * {@code java.net.http.HttpClient} configured with an SSLContext built from the
 * same JKS, driven through {@code sendAsync(...).get()} exactly as Spring's
 * {@code JdkClientHttpRequest.executeInternal} does.
 *
 * A run in which the client fails to present its certificate shows up as the
 * server's {@code CertificateRequired} alert reaching the client. One iteration
 * here is one secure request; the suite produces roughly four per class run and
 * reported one alert in sixteen class runs, so a few hundred iterations is
 * already an order of magnitude more exposure than the original report.
 *
 * Args: {@code <keystore.jks> <password> [iterations] [mode]}
 * where mode is {@code fresh} (a new SSLContext per iteration, matching a new
 * {@code SslBundle} per test method) or {@code shared}.
 */
public class HttpClientClientAuthProbe {

    private static volatile Object sink;

    public static void main(String[] args) throws Exception {
        String jks = args.length > 0 ? args[0] : "test.jks";
        char[] password = (args.length > 1 ? args[1] : "password").toCharArray();
        int iterations = args.length > 2 ? Integer.parseInt(args[2]) : 200;
        String mode = args.length > 3 ? args[3] : "fresh";

        byte[] keystore = Files.readAllBytes(Path.of(jks));
        SSLContext serverContext = newContext(keystore, password);

        SSLServerSocketFactory ssf = serverContext.getServerSocketFactory();
        SSLServerSocket server = (SSLServerSocket) ssf.createServerSocket(0, 50, InetAddress.getLoopbackAddress());
        server.setNeedClientAuth(true);
        int port = server.getLocalPort();
        AtomicBoolean stop = new AtomicBoolean(false);
        Thread acceptor = new Thread(() -> serve(server, stop), "probe-acceptor");
        acceptor.setDaemon(true);
        acceptor.start();

        URI uri = URI.create("https://localhost:" + port + "/");
        SSLContext sharedClientContext = newContext(keystore, password);

        Map<String, Integer> failures = new LinkedHashMap<>();
        int ok = 0;
        for (int i = 0; i < iterations; i++) {
            // `nocert` is the POSITIVE CONTROL: a client whose SSLContext has
            // no KeyManagers at all. It must FAIL on every iteration, on
            // HotSpot too — otherwise the server is not really demanding a
            // client certificate and every green above is vacuous.
            SSLContext clientContext = switch (mode) {
                case "shared" -> sharedClientContext;
                case "nocert" -> trustOnlyContext(keystore, password);
                default -> newContext(keystore, password);
            };
            // Allocation pressure between the SSLContext's construction and its
            // use: every identity-keyed side table in the TLS layer has to
            // survive a young-generation move happening in this window.
            churn();
            HttpClient client = HttpClient.newBuilder().sslContext(clientContext).build();
            HttpRequest request = HttpRequest.newBuilder(uri).GET().build();
            try {
                HttpResponse<String> response = client
                        .sendAsync(request, HttpResponse.BodyHandlers.ofString())
                        .get();
                if (response.statusCode() == 200) {
                    ok++;
                }
                else {
                    record(failures, "status=" + response.statusCode());
                }
            }
            catch (Throwable t) {
                Throwable root = t;
                while (root.getCause() != null) {
                    root = root.getCause();
                }
                record(failures, root.getClass().getName() + ": " + root.getMessage());
            }
            if ((i + 1) % 25 == 0) {
                System.out.println("  ... " + (i + 1) + "/" + iterations + " ok=" + ok);
            }
        }
        stop.set(true);
        try {
            server.close();
        }
        catch (IOException ignored) {
        }

        System.out.println("PROBE-RESULT mode=" + mode + " iterations=" + iterations
                + " ok=" + ok + " failed=" + (iterations - ok));
        for (Map.Entry<String, Integer> e : failures.entrySet()) {
            System.out.println("PROBE-FAILURE x" + e.getValue() + " :: " + e.getKey());
        }
        System.out.println(iterations == ok ? "PROBE-OK" : "PROBE-FAIL");
    }

    private static void record(Map<String, Integer> failures, String key) {
        failures.merge(key, 1, Integer::sum);
        System.out.println("PROBE-ITER-FAILURE " + key);
    }

    private static void churn() {
        List<byte[]> keep = new ArrayList<>();
        for (int i = 0; i < 64; i++) {
            keep.add(new byte[16 * 1024]);
        }
        sink = keep;
        sink = null;
    }

    private static SSLContext newContext(byte[] keystoreBytes, char[] password) throws Exception {
        KeyStore ks = KeyStore.getInstance("JKS");
        // `JksSslStoreDetails.forLocation("classpath:test.jks")` carries no
        // store password — only the key password ("password"), which is what
        // `SslBundleKey.of` supplies. Load the store unauthenticated, exactly
        // as `JksSslStoreBundle` does.
        ks.load(new java.io.ByteArrayInputStream(keystoreBytes), null);
        KeyManagerFactory kmf = KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm());
        kmf.init(ks, password);
        TrustManagerFactory tmf = TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
        tmf.init(ks);
        SSLContext context = SSLContext.getInstance("TLS");
        context.init(kmf.getKeyManagers(), tmf.getTrustManagers(), null);
        return context;
    }

    /** Same trust material, no key material — see the `nocert` mode above. */
    private static SSLContext trustOnlyContext(byte[] keystoreBytes, char[] password) throws Exception {
        KeyStore ks = KeyStore.getInstance("JKS");
        ks.load(new java.io.ByteArrayInputStream(keystoreBytes), null);
        TrustManagerFactory tmf = TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
        tmf.init(ks);
        SSLContext context = SSLContext.getInstance("TLS");
        context.init(null, tmf.getTrustManagers(), null);
        return context;
    }

    private static void serve(SSLServerSocket server, AtomicBoolean stop) {
        while (!stop.get()) {
            try (SSLSocket socket = (SSLSocket) server.accept()) {
                socket.setNeedClientAuth(true);
                InputStream in = socket.getInputStream();
                ByteArrayOutputStream head = new ByteArrayOutputStream();
                int b;
                while ((b = in.read()) != -1) {
                    head.write(b);
                    byte[] seen = head.toByteArray();
                    int n = seen.length;
                    if (n >= 4 && seen[n - 4] == '\r' && seen[n - 3] == '\n' && seen[n - 2] == '\r'
                            && seen[n - 1] == '\n') {
                        break;
                    }
                }
                OutputStream out = socket.getOutputStream();
                out.write("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                        .getBytes(StandardCharsets.UTF_8));
                out.flush();
            }
            catch (Throwable t) {
                if (!stop.get()) {
                    System.out.println("PROBE-SERVER-ERROR " + t);
                }
            }
        }
    }
}
