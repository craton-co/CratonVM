// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import com.sun.net.httpserver.HttpServer;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.HttpURLConnection;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.net.URI;
import java.net.URL;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.charset.StandardCharsets;

/**
 * Permanent gate for docs/known-issues/jdk-only/W7-24-httpserverloop-and-strict-fallbacks.md.
 *
 * Three residuals recorded there, all exercised here under {@code --jdk-only}:
 *
 *   1. {@code com.sun.net.httpserver.HttpServer.start()} used to die with
 *      {@code NoClassDefFoundError: CratonVM$HttpServerLoop} (the door defect,
 *      W7-24 section 1) -- a plain request/response round trip through it is
 *      the falsifier.
 *   2. The exchange's {@code getResponseBody()} carrier falls back to a real
 *      {@code java.io.ByteArrayOutputStream} under strict mode (section 3);
 *      the written bytes must reach the client unmodified.
 *   3. A custom {@code HttpResponse.BodyHandler} driven through
 *      {@code java.net.http.HttpClient} used to die with
 *      {@code NoClassDefFoundError: cratonvm/net/HttpBodyReplaySubscription}
 *      (section 4) -- this does not go through {@code HttpServer} at all, so
 *      it is exercised against a bare {@code ServerSocket} instead, exactly
 *      as the record's own falsifier does.
 */
public class RJdkHttpServer {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    static void httpServerRoundTrip() throws Exception {
        HttpServer server = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
        server.createContext("/", exchange -> {
            byte[] reqBody = exchange.getRequestBody().readAllBytes();
            String reqStr = new String(reqBody, StandardCharsets.UTF_8);
            String respStr = "echo:" + reqStr;
            byte[] respBytes = respStr.getBytes(StandardCharsets.UTF_8);
            // Carrier class name is DELIBERATELY not compared: HotSpot answers
            // sun.net.httpserver.PlaceholderOutputStream, CratonVM's real-JDK
            // fallback answers java.io.ByteArrayOutputStream (W7-24 section 3 --
            // neither name is the JDK's own Compatible-mode answer either, so the
            // fallback costs no fidelity that was there to lose). Only the bytes
            // that reach the client are a cross-VM contract.
            OutputStream body = exchange.getResponseBody();
            exchange.sendResponseHeaders(200, respBytes.length);
            body.write(respBytes);
            exchange.close();
        });
        server.start();
        try {
            int port = server.getAddress().getPort();

            URL url = new URL("http://127.0.0.1:" + port + "/");
            HttpURLConnection huc = (HttpURLConnection) url.openConnection();
            huc.setDoOutput(true);
            huc.setRequestMethod("POST");
            try (OutputStream os = huc.getOutputStream()) {
                os.write("hello".getBytes(StandardCharsets.UTF_8));
            }
            int rc = huc.getResponseCode();
            String body;
            try (InputStream is = huc.getInputStream()) {
                body = new String(is.readAllBytes(), StandardCharsets.UTF_8);
            }
            System.out.println("CK RJdkHttpServer huc.rc=" + rc + " huc.body=" + body);
            check(rc == 200, "HttpServer.start() must accept a connection under --jdk-only (door defect)");
            check("echo:hello".equals(body), "HttpExchange response body must round-trip through the ByteArrayOutputStream fallback");

            HttpClient client = HttpClient.newHttpClient();
            HttpRequest req = HttpRequest.newBuilder(URI.create("http://127.0.0.1:" + port + "/"))
                    .POST(HttpRequest.BodyPublishers.ofString("world"))
                    .build();
            HttpResponse<String> resp = client.send(req, HttpResponse.BodyHandlers.ofString());
            System.out.println("CK RJdkHttpServer httpclient.status=" + resp.statusCode()
                    + " httpclient.body=" + resp.body());
            check(resp.statusCode() == 200, "java.net.http.HttpClient must reach the same HttpServer");
            check("echo:world".equals(resp.body()), "and read the same fallback-carried response body");
        } finally {
            server.stop(0);
        }
    }

    static void customBodyHandlerOverRawSocket() throws Exception {
        ServerSocket ss = new ServerSocket(0, 0, InetAddress.getByName("127.0.0.1"));
        final String rawBody = "raw-body";
        Thread serverThread = new Thread(() -> {
            try (Socket s = ss.accept()) {
                InputStream in = s.getInputStream();
                StringBuilder headEnd = new StringBuilder();
                int c;
                while ((c = in.read()) != -1) {
                    headEnd.append((char) c);
                    int n = headEnd.length();
                    if (n >= 4 && headEnd.charAt(n - 4) == '\r' && headEnd.charAt(n - 3) == '\n'
                            && headEnd.charAt(n - 2) == '\r' && headEnd.charAt(n - 1) == '\n') {
                        break;
                    }
                }
                byte[] bodyBytes = rawBody.getBytes(StandardCharsets.UTF_8);
                String resp = "HTTP/1.1 200 OK\r\nContent-Length: " + bodyBytes.length
                        + "\r\nConnection: close\r\n\r\n" + rawBody;
                s.getOutputStream().write(resp.getBytes(StandardCharsets.UTF_8));
                s.getOutputStream().flush();
            } catch (IOException e) {
                throw new RuntimeException(e);
            }
        }, "RJdkHttpServer-raw-server");
        serverThread.start();
        try {
            int port = ss.getLocalPort();
            HttpClient client = HttpClient.newHttpClient();
            HttpRequest req = HttpRequest.newBuilder(URI.create("http://127.0.0.1:" + port + "/")).GET().build();
            // Same reasoning as above for HttpResponse.ResponseInfo's carrier
            // class: HotSpot answers jdk.internal.net.http.ResponseInfoImpl (an
            // internal class), CratonVM answers the public interface's own name.
            HttpResponse<String> resp = client.send(req,
                    info -> HttpResponse.BodySubscribers.ofString(StandardCharsets.UTF_8));
            System.out.println("CK RJdkHttpServer raw.status=" + resp.statusCode() + " raw.body=" + resp.body());
            check(resp.statusCode() == 200, "a custom BodyHandler must reach a bare ServerSocket response");
            check(rawBody.equals(resp.body()),
                    "and read the body back through HttpBodyReplaySubscription rather than NoClassDefFoundError");
        } finally {
            serverThread.join();
            ss.close();
        }
    }

    public static void main(String[] args) throws Exception {
        httpServerRoundTrip();
        customBodyHandlerOverRawSocket();
        System.out.println("CK RJdkHttpServer checks=" + checks);
        System.out.println("PASS RJdkHttpServer (" + checks + " checks)");
    }
}
