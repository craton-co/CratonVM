import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.net.HttpURLConnection;
import java.net.URI;

import mockwebserver3.MockResponse;
import mockwebserver3.MockWebServer;
import mockwebserver3.RecordedRequest;

/**
 * The headline symptom of
 * `docs/known-issues/tomcat/serversocket-bind-socketaddress-noop-localport-zero.md`,
 * reproduced against the REAL okhttp library rather than a hand-written stand-in:
 * `MockWebServer` binds with `new ServerSocket().bind(addr)`, so when that bind
 * was a no-op `getPort()` returned 0 and every caller built
 * `http://localhost:0`.
 *
 * Asserts the whole path, not just the number: a real port, a real request that
 * the server records, a real response body, and a clean shutdown (the close()
 * that used to leave okhttp's accept queue waiting).
 */
public class MockWebServerPortProbe {

    public static void main(String[] args) throws Exception {
        MockWebServer server = new MockWebServer();
        server.enqueue(new MockResponse.Builder().code(200).body("hello from mockwebserver").build());
        server.start();

        int port = server.getPort();
        System.out.println("port.positive=" + (port > 0));
        String baseUrl = "http://localhost:" + port + "/probe";
        System.out.println("url.hasRealPort=" + !baseUrl.contains(":0/"));
        System.out.println("hostName.nonEmpty=" + (server.getHostName() != null && !server.getHostName().isEmpty()));

        HttpURLConnection connection = (HttpURLConnection) URI.create(baseUrl).toURL().openConnection();
        connection.setConnectTimeout(10000);
        connection.setReadTimeout(10000);
        System.out.println("response.code=" + connection.getResponseCode());
        String body;
        try (BufferedReader reader = new BufferedReader(
                new InputStreamReader(connection.getInputStream(), "UTF-8"))) {
            body = reader.readLine();
        }
        System.out.println("response.body=" + body);

        RecordedRequest recorded = server.takeRequest();
        System.out.println("recorded.path=" + (recorded == null ? "<none>" : recorded.getUrl().encodedPath()));

        long start = System.nanoTime();
        server.close();
        long closeMillis = (System.nanoTime() - start) / 1_000_000L;
        // close() waits for okhttp's accept TaskRunner queue to drain and throws
        // "Gave up waiting for queue to shut down" after 5 s if a blocked
        // accept() never woke.
        System.out.println("close.prompt=" + (closeMillis < 4000));
        System.out.println("PROBE-DONE");
    }
}
