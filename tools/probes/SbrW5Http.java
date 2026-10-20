// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import com.sun.net.httpserver.HttpContext;
import com.sun.net.httpserver.HttpExchange;
import com.sun.net.httpserver.HttpServer;
import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.HttpURLConnection;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.URI;
import java.net.URL;
import java.net.URLConnection;
import java.net.http.HttpClient;
import java.net.http.HttpHeaders;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.time.Duration;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;
import java.util.function.Supplier;

/**
 * SBR wave 5, lane {@code http}: plain HTTP over loopback, both ends JDK API.
 *
 * <p>Server: {@code com.sun.net.httpserver.HttpServer} with fixed-length,
 * chunked, echo (POST body + request headers), redirect, error, HEAD and
 * no-content contexts. Clients: {@code java.net.HttpURLConnection} (GET, POST
 * fixed-length and chunked streaming, redirect followed and not followed,
 * error stream, 404 from an unmapped path, HEAD, keep-alive reuse) and
 * {@code java.net.http.HttpClient} (the same shapes, plus request/publisher
 * API rows and {@code sendAsync}). Plus the {@code jrt:} and {@code file:}
 * {@code URLConnection}s.
 *
 * <p>Every row is evaluated ONCE and printed (the first-run value: a native
 * that wins from the first call shows here), then re-evaluated in a hot loop
 * and compared with that first value; {@code drift} counts mismatches, so cold
 * == warm is asserted. Ports and dates are normalised so the whole output
 * diffs exactly against HotSpot 25. Implementation class names are printed on
 * their own {@code cls} rows (they are the retirement's own witness).
 *
 * <pre>
 *   java SbrW5Http [iters]                  (HotSpot oracle)
 *   cratonvm --jdk-only -cp . SbrW5Http [iters]
 * </pre>
 */
public class SbrW5Http {
    static final List<String> LABELS = new ArrayList<>();
    static final List<Supplier<String>> ROWS = new ArrayList<>();
    static int port;
    static String base;

    static void row(String label, Supplier<String> s) {
        LABELS.add(label);
        ROWS.add(s);
    }

    interface X<T> {
        T get() throws Exception;
    }

    static <T> T x(X<T> f) {
        try {
            return f.get();
        } catch (RuntimeException e) {
            throw e;
        } catch (Exception e) {
            throw new RuntimeException(e.getClass().getName() + ": " + e.getMessage());
        }
    }

    static String safe(Supplier<String> s) {
        try {
            return norm(s.get());
        } catch (Throwable t) {
            return "!" + t.getClass().getName() + ": " + norm(t.getMessage());
        }
    }

    static String norm(String s) {
        if (s == null) return "null";
        if (port != 0) s = s.replace(":" + port, ":PORT");
        return s;
    }

    static byte[] read(InputStream in) throws IOException {
        if (in == null) return null;
        try (in) {
            ByteArrayOutputStream b = new ByteArrayOutputStream();
            byte[] buf = new byte[1024];
            int n;
            while ((n = in.read(buf)) >= 0) b.write(buf, 0, n);
            return b.toByteArray();
        }
    }

    static String str(InputStream in) throws IOException {
        byte[] b = read(in);
        return b == null ? "null" : new String(b, StandardCharsets.UTF_8);
    }

    /** Header map without the Date value (the key stays), keys sorted, null key first. */
    static String hdrs(Map<String, List<String>> m) {
        TreeMap<String, List<String>> t = new TreeMap<>((a, b) -> {
            if (a == null) return b == null ? 0 : -1;
            if (b == null) return 1;
            return a.compareTo(b);
        });
        for (Map.Entry<String, List<String>> e : m.entrySet()) {
            String k = e.getKey();
            t.put(k, k != null && k.equalsIgnoreCase("date") ? List.of("DATE") : e.getValue());
        }
        return t.toString();
    }

    static void send(HttpExchange ex, int code, String body) throws IOException {
        byte[] b = body.getBytes(StandardCharsets.UTF_8);
        ex.sendResponseHeaders(code, b.length == 0 ? -1 : b.length);
        try (OutputStream o = ex.getResponseBody()) {
            o.write(b);
        }
    }

    static HttpURLConnection huc(String path) throws IOException {
        return (HttpURLConnection) new URL(base + path).openConnection();
    }

    public static void main(String[] args) throws Exception {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 20;

        // ---- server ----------------------------------------------------------
        HttpServer server = HttpServer.create(new InetSocketAddress(InetAddress.getByName("127.0.0.1"), 0), 0);
        HttpContext hello = server.createContext("/hello", ex -> {
            ex.getResponseHeaders().add("X-Sbr", "w5");
            ex.getResponseHeaders().add("X-Multi", "a");
            ex.getResponseHeaders().add("X-Multi", "b");
            ex.getResponseHeaders().set("Content-Type", "text/plain; charset=utf-8");
            send(ex, 200, "hi there");
        });
        server.createContext("/echo", ex -> {
            String body = str(ex.getRequestBody());
            String info = ex.getRequestMethod() + " " + ex.getRequestURI() + " " + ex.getProtocol()
                    + " ctx=" + ex.getHttpContext().getPath()
                    + " x-a=" + ex.getRequestHeaders().get("X-A")
                    + " ct=" + ex.getRequestHeaders().getFirst("Content-Type")
                    + " te=" + ex.getRequestHeaders().getFirst("Transfer-Encoding")
                    + " cl=" + ex.getRequestHeaders().getFirst("Content-Length")
                    + " body=" + body;
            send(ex, 200, info);
        });
        server.createContext("/chunked", ex -> {
            ex.sendResponseHeaders(200, 0);
            try (OutputStream o = ex.getResponseBody()) {
                for (int i = 0; i < 3; i++) {
                    o.write(("part" + i + ";").getBytes(StandardCharsets.UTF_8));
                    o.flush();
                }
            }
        });
        server.createContext("/redirect", ex -> {
            ex.getResponseHeaders().set("Location", "/hello");
            send(ex, 302, "");
        });
        server.createContext("/err", ex -> send(ex, 500, "boom"));
        server.createContext("/nocontent", ex -> send(ex, 204, ""));
        server.createContext("/conn", ex -> send(ex, 200, "" + ex.getRemoteAddress().getPort()));
        server.createContext("/head", ex -> {
            ex.getResponseHeaders().set("X-Head", "yes");
            if (ex.getRequestMethod().equals("HEAD")) {
                ex.sendResponseHeaders(200, -1);
                ex.close();
            } else {
                send(ex, 200, "body-for-get");
            }
        });
        server.createContext("/exch", ex -> {
            ex.setAttribute("k", "v");
            String info = "local=" + ex.getLocalAddress().getAddress().getHostAddress()
                    + " remote=" + ex.getRemoteAddress().getAddress().getHostAddress()
                    + " attr=" + ex.getAttribute("k")
                    + " srv=" + (ex.getHttpContext().getServer() == server)
                    + " q=" + ex.getRequestURI().getQuery();
            send(ex, 200, info);
        });
        server.start();
        port = server.getAddress().getPort();
        base = "http://127.0.0.1:" + port;

        // ---- server-side API -------------------------------------------------
        System.out.println("cls server = " + server.getClass().getName());
        row("srv.address", () -> server.getAddress().getAddress().getHostAddress() + " bound=" + (server.getAddress().getPort() > 0));
        row("srv.ctx", () -> hello.getPath() + " same-server=" + (hello.getServer() == server)
                + " attrs=" + hello.getAttributes().size() + " filters=" + hello.getFilters().size());
        row("srv.executor", () -> server.getExecutor() == null ? "null" : server.getExecutor().getClass().getName());
        row("srv.removeContext", () -> x(() -> {
            HttpContext c = server.createContext("/tmp", ex -> send(ex, 200, "tmp"));
            String before = "" + huc("/tmp").getResponseCode();
            server.removeContext(c);
            String after = "" + huc("/tmp").getResponseCode();
            return before + "->" + after;
        }));
        row("srv.duplicateContext", () -> x(() -> {
            try {
                server.createContext("/hello", ex -> {});
                return "no throw";
            } catch (IllegalArgumentException e) {
                return "IAE " + e.getMessage();
            }
        }));
        row("srv.exchange", () -> x(() -> str(huc("/exch?a=1&b=2").getInputStream())));

        // ---- HttpURLConnection ----------------------------------------------
        System.out.println("cls huc = " + safe(() -> x(() -> huc("/hello").getClass().getName())));
        row("huc.get", () -> x(() -> {
            HttpURLConnection c = huc("/hello");
            int code = c.getResponseCode();
            return "code=" + code + " msg=" + c.getResponseMessage() + " body=" + str(c.getInputStream())
                    + " x-sbr=" + c.getHeaderField("X-Sbr") + " multi=" + c.getHeaderFields().get("X-multi")
                    + " type=" + c.getContentType() + " len=" + c.getContentLength() + "/" + c.getContentLengthLong()
                    + " method=" + c.getRequestMethod();
        }));
        row("huc.headers", () -> x(() -> {
            HttpURLConnection c = huc("/hello");
            String h = hdrs(c.getHeaderFields());
            StringBuilder idx = new StringBuilder();
            for (int i = 0; i < 12; i++) {
                String k = c.getHeaderFieldKey(i);
                String v = c.getHeaderField(i);
                if (k == null && v == null && i > 0) break;
                idx.append(i).append(':').append(k).append('=')
                        .append(k != null && k.equalsIgnoreCase("date") ? "DATE" : v).append(' ');
            }
            str(c.getInputStream());
            return h + " | " + idx.toString().trim();
        }));
        row("huc.post.fixed", () -> x(() -> {
            HttpURLConnection c = huc("/echo?x=1");
            c.setRequestMethod("POST");
            c.setDoOutput(true);
            c.setRequestProperty("X-A", "1");
            c.addRequestProperty("X-A", "2");
            c.setRequestProperty("Content-Type", "text/plain");
            byte[] b = "posted-body".getBytes(StandardCharsets.UTF_8);
            c.setFixedLengthStreamingMode(b.length);
            try (OutputStream o = c.getOutputStream()) {
                o.write(b);
            }
            return c.getResponseCode() + " " + str(c.getInputStream());
        }));
        row("huc.post.chunked", () -> x(() -> {
            HttpURLConnection c = huc("/echo");
            c.setRequestMethod("POST");
            c.setDoOutput(true);
            c.setChunkedStreamingMode(4);
            try (OutputStream o = c.getOutputStream()) {
                o.write("chunk-one-".getBytes(StandardCharsets.UTF_8));
                o.write("chunk-two".getBytes(StandardCharsets.UTF_8));
            }
            return c.getResponseCode() + " " + str(c.getInputStream());
        }));
        row("huc.post.buffered", () -> x(() -> {
            HttpURLConnection c = huc("/echo");
            c.setDoOutput(true);
            try (OutputStream o = c.getOutputStream()) {
                o.write("buffered".getBytes(StandardCharsets.UTF_8));
            }
            return c.getResponseCode() + " " + c.getRequestMethod() + " " + str(c.getInputStream());
        }));
        row("huc.chunked.response", () -> x(() -> {
            HttpURLConnection c = huc("/chunked");
            return c.getResponseCode() + " te=" + c.getHeaderField("Transfer-encoding") + " len="
                    + c.getContentLength() + " body=" + str(c.getInputStream());
        }));
        row("huc.redirect.follow", () -> x(() -> {
            HttpURLConnection c = huc("/redirect");
            return c.getResponseCode() + " url=" + norm(c.getURL().toString()) + " body=" + str(c.getInputStream());
        }));
        row("huc.redirect.nofollow", () -> x(() -> {
            HttpURLConnection c = huc("/redirect");
            c.setInstanceFollowRedirects(false);
            int code = c.getResponseCode();
            String loc = c.getHeaderField("Location");
            c.disconnect();
            return code + " loc=" + loc + " follow=" + c.getInstanceFollowRedirects();
        }));
        row("huc.500", () -> x(() -> {
            HttpURLConnection c = huc("/err");
            int code = c.getResponseCode();
            String in;
            try {
                in = str(c.getInputStream());
            } catch (IOException e) {
                in = e.getClass().getName() + ": " + norm(e.getMessage());
            }
            return code + " " + c.getResponseMessage() + " in=" + in + " err=" + str(c.getErrorStream());
        }));
        row("huc.404", () -> x(() -> {
            HttpURLConnection c = huc("/nope");
            int code = c.getResponseCode();
            String in;
            try {
                in = str(c.getInputStream());
            } catch (IOException e) {
                in = e.getClass().getName();
            }
            return code + " " + c.getResponseMessage() + " in=" + in + " err=" + str(c.getErrorStream());
        }));
        row("huc.head", () -> x(() -> {
            HttpURLConnection c = huc("/head");
            c.setRequestMethod("HEAD");
            return c.getResponseCode() + " x-head=" + c.getHeaderField("X-Head") + " body=" + str(c.getInputStream());
        }));
        row("huc.204", () -> x(() -> {
            HttpURLConnection c = huc("/nocontent");
            return c.getResponseCode() + " len=" + c.getContentLength() + " body=" + str(c.getInputStream());
        }));
        row("huc.keepalive", () -> x(() -> {
            String p1 = str(huc("/conn").getInputStream());
            String p2 = str(huc("/conn").getInputStream());
            return "reused=" + p1.equals(p2);
        }));
        row("huc.badMethod", () -> x(() -> {
            try {
                huc("/hello").setRequestMethod("BREW");
                return "no throw";
            } catch (java.net.ProtocolException e) {
                return "ProtocolException " + e.getMessage();
            }
        }));
        row("huc.props", () -> x(() -> {
            HttpURLConnection c = huc("/hello");
            c.setConnectTimeout(1234);
            c.setReadTimeout(4321);
            c.setUseCaches(false);
            c.setRequestProperty("X-B", "1");
            c.addRequestProperty("X-B", "2");
            String s = "ct=" + c.getConnectTimeout() + " rt=" + c.getReadTimeout() + " uc=" + c.getUseCaches()
                    + " doIn=" + c.getDoInput() + " doOut=" + c.getDoOutput() + " xb=" + c.getRequestProperty("X-B")
                    + " props=" + c.getRequestProperties().get("X-B") + " proxy=" + c.usingProxy();
            c.connect();
            String t;
            try {
                c.setRequestProperty("X-C", "1");
                t = "no throw";
            } catch (IllegalStateException e) {
                t = "ISE " + e.getMessage();
            }
            str(c.getInputStream());
            return s + " afterConnect=" + t;
        }));
        row("huc.refused", () -> x(() -> {
            HttpURLConnection c = (HttpURLConnection) new URL("http://127.0.0.1:1/x").openConnection();
            try {
                c.getResponseCode();
                return "no throw";
            } catch (IOException e) {
                return e.getClass().getName() + ": " + e.getMessage();
            }
        }));
        row("url.openStream", () -> x(() -> str(new URL(base + "/hello").openStream())));

        // ---- java.net.http --------------------------------------------------
        HttpClient client = HttpClient.newBuilder()
                .version(HttpClient.Version.HTTP_1_1)
                .followRedirects(HttpClient.Redirect.NORMAL)
                .connectTimeout(Duration.ofSeconds(5))
                .build();
        HttpClient plain = HttpClient.newHttpClient();
        System.out.println("cls client = " + client.getClass().getName());
        row("hc.config", () -> client.version() + " " + client.followRedirects() + " " + client.connectTimeout()
                + " proxy=" + client.proxy().isPresent() + " auth=" + client.authenticator().isPresent()
                + " | plain " + plain.version() + " " + plain.followRedirects() + " " + plain.connectTimeout());
        row("hc.request.api", () -> x(() -> {
            HttpRequest r = HttpRequest.newBuilder(URI.create(base + "/echo"))
                    .POST(HttpRequest.BodyPublishers.ofString("abc"))
                    .header("X-A", "1").header("X-A", "2").timeout(Duration.ofSeconds(3)).build();
            return r.method() + " " + r.uri() + " " + r.headers().map() + " len="
                    + r.bodyPublisher().get().contentLength() + " t=" + r.timeout() + " exp=" + r.expectContinue()
                    + " ver=" + r.version();
        }));
        row("hc.publishers", () -> "str=" + HttpRequest.BodyPublishers.ofString("héllo").contentLength()
                + " bytes=" + HttpRequest.BodyPublishers.ofByteArray(new byte[7]).contentLength()
                + " slice=" + HttpRequest.BodyPublishers.ofByteArray(new byte[7], 2, 3).contentLength()
                + " none=" + HttpRequest.BodyPublishers.noBody().contentLength()
                + " get=" + HttpRequest.newBuilder(URI.create(base)).build().bodyPublisher().isPresent());
        row("hc.request.bad", () -> {
            try {
                HttpRequest.newBuilder(URI.create("ftp://x/")).build();
                return "no throw";
            } catch (IllegalArgumentException e) {
                return "IAE " + e.getMessage();
            }
        });
        row("hc.get", () -> x(() -> {
            HttpResponse<String> r = client.send(HttpRequest.newBuilder(URI.create(base + "/hello")).build(),
                    HttpResponse.BodyHandlers.ofString());
            return r.statusCode() + " " + r.version() + " body=" + r.body() + " x-sbr="
                    + r.headers().firstValue("x-sbr").orElse("-") + " multi=" + r.headers().allValues("X-Multi")
                    + " len=" + r.headers().firstValueAsLong("content-length").orElse(-1) + " uri=" + r.uri();
        }));
        row("hc.headers", () -> x(() -> {
            HttpResponse<Void> r = client.send(HttpRequest.newBuilder(URI.create(base + "/hello")).build(),
                    HttpResponse.BodyHandlers.discarding());
            HttpHeaders h = r.headers();
            return r.statusCode() + " " + hdrs(h.map()) + " body=" + r.body();
        }));
        row("hc.post.string", () -> x(() -> {
            HttpResponse<String> r = client.send(HttpRequest.newBuilder(URI.create(base + "/echo?y=2"))
                    .header("X-A", "hc").header("Content-Type", "text/plain")
                    .POST(HttpRequest.BodyPublishers.ofString("from-client")).build(),
                    HttpResponse.BodyHandlers.ofString());
            return r.statusCode() + " " + r.body();
        }));
        row("hc.post.bytes", () -> x(() -> {
            HttpResponse<byte[]> r = client.send(HttpRequest.newBuilder(URI.create(base + "/echo"))
                    .PUT(HttpRequest.BodyPublishers.ofByteArray("put-bytes".getBytes(StandardCharsets.UTF_8))).build(),
                    HttpResponse.BodyHandlers.ofByteArray());
            return r.statusCode() + " " + new String(r.body(), StandardCharsets.UTF_8);
        }));
        row("hc.chunked", () -> x(() -> {
            HttpResponse<String> r = client.send(HttpRequest.newBuilder(URI.create(base + "/chunked")).build(),
                    HttpResponse.BodyHandlers.ofString());
            return r.statusCode() + " te=" + r.headers().firstValue("transfer-encoding").orElse("-") + " body=" + r.body();
        }));
        row("hc.redirect.follow", () -> x(() -> {
            HttpResponse<String> r = client.send(HttpRequest.newBuilder(URI.create(base + "/redirect")).build(),
                    HttpResponse.BodyHandlers.ofString());
            return r.statusCode() + " uri=" + r.uri() + " prev=" + r.previousResponse().map(HttpResponse::statusCode)
                    + " body=" + r.body();
        }));
        row("hc.redirect.never", () -> x(() -> {
            HttpResponse<String> r = plain.send(HttpRequest.newBuilder(URI.create(base + "/redirect")).build(),
                    HttpResponse.BodyHandlers.ofString());
            return r.statusCode() + " loc=" + r.headers().firstValue("location").orElse("-") + " body=[" + r.body() + "]";
        }));
        row("hc.500", () -> x(() -> {
            HttpResponse<String> r = client.send(HttpRequest.newBuilder(URI.create(base + "/err")).build(),
                    HttpResponse.BodyHandlers.ofString());
            return r.statusCode() + " body=" + r.body();
        }));
        row("hc.404", () -> x(() -> {
            HttpResponse<String> r = client.send(HttpRequest.newBuilder(URI.create(base + "/nope")).build(),
                    HttpResponse.BodyHandlers.ofString());
            return r.statusCode() + " body=" + r.body();
        }));
        row("hc.head", () -> x(() -> {
            HttpResponse<String> r = client.send(HttpRequest.newBuilder(URI.create(base + "/head"))
                    .method("HEAD", HttpRequest.BodyPublishers.noBody()).build(), HttpResponse.BodyHandlers.ofString());
            return r.statusCode() + " x-head=" + r.headers().firstValue("x-head").orElse("-") + " body=[" + r.body() + "]";
        }));
        row("hc.204", () -> x(() -> {
            HttpResponse<String> r = client.send(HttpRequest.newBuilder(URI.create(base + "/nocontent")).build(),
                    HttpResponse.BodyHandlers.ofString());
            return r.statusCode() + " body=[" + r.body() + "]";
        }));
        row("hc.keepalive", () -> x(() -> {
            String p1 = client.send(HttpRequest.newBuilder(URI.create(base + "/conn")).build(),
                    HttpResponse.BodyHandlers.ofString()).body();
            String p2 = client.send(HttpRequest.newBuilder(URI.create(base + "/conn")).build(),
                    HttpResponse.BodyHandlers.ofString()).body();
            return "reused=" + p1.equals(p2);
        }));
        row("hc.async", () -> x(() -> {
            HttpResponse<String> r = client.sendAsync(HttpRequest.newBuilder(URI.create(base + "/hello")).build(),
                    HttpResponse.BodyHandlers.ofString()).join();
            return r.statusCode() + " " + r.body();
        }));
        row("hc.inputStream", () -> x(() -> {
            HttpResponse<InputStream> r = client.send(HttpRequest.newBuilder(URI.create(base + "/chunked")).build(),
                    HttpResponse.BodyHandlers.ofInputStream());
            return r.statusCode() + " " + str(r.body());
        }));
        row("hc.refused", () -> x(() -> {
            try {
                client.send(HttpRequest.newBuilder(URI.create("http://127.0.0.1:1/x")).build(),
                        HttpResponse.BodyHandlers.ofString());
                return "no throw";
            } catch (IOException e) {
                return e.getClass().getName();
            }
        }));

        // ---- jrt: / file: URLConnection -------------------------------------
        URL jrt = new URL("jrt:/java.base/java/lang/Object.class");
        System.out.println("cls jrt = " + safe(() -> x(() -> jrt.openConnection().getClass().getName())));
        row("jrt.conn", () -> x(() -> {
            URLConnection c = jrt.openConnection();
            c.setUseCaches(false);
            byte[] b = read(c.getInputStream());
            return "len=" + b.length + " magic=" + Integer.toHexString(((b[0] & 0xff) << 24) | ((b[1] & 0xff) << 16)
                    | ((b[2] & 0xff) << 8) | (b[3] & 0xff)) + " cl=" + c.getContentLengthLong() + " uc=" + c.getUseCaches();
        }));
        row("jrt.openStream", () -> x(() -> "len=" + read(jrt.openStream()).length));
        row("jrt.missing", () -> x(() -> {
            try {
                read(new URL("jrt:/java.base/no/such/Thing.class").openStream());
                return "no throw";
            } catch (IOException e) {
                return e.getClass().getName();
            }
        }));
        File tmp = File.createTempFile("sbrw5http", ".txt");
        tmp.deleteOnExit();
        Files.writeString(tmp.toPath(), "file-body");
        row("file.conn", () -> x(() -> {
            URLConnection c = tmp.toURI().toURL().openConnection();
            return "type=" + c.getContentType() + " len=" + c.getContentLength() + " body=" + str(c.getInputStream());
        }));

        // ---- first run, then the hot loop ---------------------------------
        String[] first = new String[ROWS.size()];
        for (int i = 0; i < ROWS.size(); i++) {
            first[i] = safe(ROWS.get(i));
            System.out.println("row " + LABELS.get(i) + " = " + first[i]);
        }
        int drift = 0;
        boolean trace = System.getenv("SBR_TRACE") != null;
        for (int k = 0; k < iters; k++) {
            for (int i = 0; i < ROWS.size(); i++) {
                if (trace) System.err.println("TRACE iter " + k + " " + LABELS.get(i));
                String v = safe(ROWS.get(i));
                if (!v.equals(first[i])) {
                    if (drift < 10) {
                        System.out.println("DRIFT at iter " + k + " " + LABELS.get(i) + " = " + v);
                    }
                    drift++;
                }
            }
        }

        // ---- one-shot: stop -------------------------------------------------
        server.stop(0);
        System.out.println("oneshot afterStop = " + safe(() -> x(() -> {
            try {
                HttpURLConnection c = huc("/hello");
                c.setConnectTimeout(2000);
                c.setReadTimeout(2000);
                return "code=" + c.getResponseCode();
            } catch (IOException e) {
                return e.getClass().getName();
            }
        })));
        System.out.println("rows " + ROWS.size() + " drift " + drift);
        System.exit(0);
    }
}
