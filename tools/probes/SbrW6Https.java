// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import com.sun.net.httpserver.HttpExchange;
import com.sun.net.httpserver.HttpsConfigurator;
import com.sun.net.httpserver.HttpsParameters;
import com.sun.net.httpserver.HttpsServer;
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.ProxySelector;
import java.net.URI;
import java.net.URL;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.charset.StandardCharsets;
import java.security.KeyPair;
import java.security.KeyPairGenerator;
import java.security.KeyStore;
import java.security.PrivateKey;
import java.security.PublicKey;
import java.security.Signature;
import java.security.cert.X509Certificate;
import java.security.cert.CertificateFactory;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;
import java.util.function.Supplier;
import javax.net.ssl.HostnameVerifier;
import javax.net.ssl.HttpsURLConnection;
import javax.net.ssl.KeyManagerFactory;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLSocketFactory;
import javax.net.ssl.TrustManagerFactory;

/**
 * SBR wave 6, lane {@code httpstack}: HTTPS over loopback, both ends JDK API.
 *
 * <p>Server: {@code com.sun.net.httpserver.HttpsServer} over a self-signed RSA-2048
 * certificate for {@code localhost}. Clients: {@code HttpsURLConnection} (GET, POST, headers,
 * cipher/certificate/principal accessors, the default and per-connection factories and hostname
 * verifiers, an untrusting factory, a wrong-host URL) and {@code java.net.http.HttpClient} over the
 * same {@code SSLContext} (HTTP/1.1), plus {@code ProxySelector.getDefault().select}.
 *
 * <p>Every row is evaluated once and printed, then re-checked {@code iters} times ({@code drift}),
 * so cold == warm. Ports and dates are normalised so the output diffs exactly against HotSpot 25.
 *
 * <pre>
 *   java SbrW6Https [iters]
 *   cratonvm --jdk-only -cp . SbrW6Https [iters]
 * </pre>
 */
public class SbrW6Https {
    static final char[] PW = "sbr-w6".toCharArray();
    static final List<String> LABELS = new ArrayList<>();
    static final List<Supplier<String>> ROWS = new ArrayList<>();
    static int port;
    static SSLContext serverCtx, clientCtx, untrustingCtx;
    static X509Certificate leaf;

    static byte[] tlv(int tag, byte[] body) {
        ByteArrayOutputStream o = new ByteArrayOutputStream();
        o.write(tag);
        int n = body.length;
        if (n < 128) o.write(n);
        else if (n < 256) { o.write(0x81); o.write(n); }
        else { o.write(0x82); o.write((n >> 8) & 0xff); o.write(n & 0xff); }
        o.write(body, 0, n);
        return o.toByteArray();
    }
    static byte[] cat(byte[]... ps) { ByteArrayOutputStream o = new ByteArrayOutputStream(); for (byte[] p : ps) o.write(p, 0, p.length); return o.toByteArray(); }
    static byte[] seq(byte[]... ps) { return tlv(0x30, cat(ps)); }
    static byte[] oid(int... e) { byte[] b = new byte[e.length]; for (int i = 0; i < e.length; i++) b[i] = (byte) e[i]; return tlv(0x06, b); }
    static byte[] ascii(int tag, String s) { return tlv(tag, s.getBytes(StandardCharsets.US_ASCII)); }

    static X509Certificate selfSigned(PrivateKey k, PublicKey p, String cn, int serial) throws Exception {
        byte[] alg = seq(oid(0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b), new byte[] {0x05, 0x00});
        byte[] name = seq(
                tlv(0x31, seq(oid(0x55, 0x04, 0x06), ascii(0x13, "US"))),
                tlv(0x31, seq(oid(0x55, 0x04, 0x0a), ascii(0x13, "Craton HTTPS"))),
                tlv(0x31, seq(oid(0x55, 0x04, 0x03), ascii(0x13, cn))));
        byte[] validity = seq(ascii(0x17, "200101000000Z"), ascii(0x17, "491231235959Z"));
        byte[] san = seq(ascii(0x82, "localhost"));
        byte[] exts = tlv(0xa3, seq(seq(oid(0x55, 0x1d, 0x11), tlv(0x04, san))));
        byte[] tbs = seq(tlv(0xa0, new byte[] {0x02, 0x01, 0x02}), new byte[] {0x02, 0x01, (byte) serial},
                alg, name, validity, name, p.getEncoded(), exts);
        Signature s = Signature.getInstance("SHA256withRSA");
        s.initSign(k);
        s.update(tbs);
        byte[] der = seq(tbs, alg, tlv(0x03, cat(new byte[] {0}, s.sign())));
        return (X509Certificate) CertificateFactory.getInstance("X.509").generateCertificate(new ByteArrayInputStream(der));
    }

    static SSLContext ctx(KeyStore keys, KeyStore trust) throws Exception {
        javax.net.ssl.KeyManager[] kms = null;
        javax.net.ssl.TrustManager[] tms = null;
        if (keys != null) {
            KeyManagerFactory kmf = KeyManagerFactory.getInstance("PKIX");
            kmf.init(keys, PW);
            kms = kmf.getKeyManagers();
        }
        if (trust != null) {
            TrustManagerFactory tmf = TrustManagerFactory.getInstance("PKIX");
            tmf.init(trust);
            tms = tmf.getTrustManagers();
        }
        SSLContext c = SSLContext.getInstance("TLS");
        c.init(kms, tms, null);
        return c;
    }

    static void row(String label, Supplier<String> s) {
        LABELS.add(label);
        ROWS.add(s);
    }

    interface X<T> { T get() throws Exception; }

    static <T> T x(X<T> f) {
        try {
            return f.get();
        } catch (RuntimeException e) {
            throw e;
        } catch (Exception e) {
            throw new RuntimeException(e.getClass().getName() + ": " + e.getMessage());
        }
    }

    static String norm(String s) {
        if (s == null) return "null";
        if (port != 0) s = s.replace(":" + port, ":PORT");
        return s;
    }

    static String safe(Supplier<String> s) {
        try {
            return norm(s.get());
        } catch (Throwable t) {
            return "!" + t.getClass().getName() + ": " + norm(t.getMessage());
        }
    }

    static String str(InputStream in) throws IOException {
        if (in == null) return "null";
        try (in) {
            return new String(in.readAllBytes(), StandardCharsets.UTF_8);
        }
    }

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

    static String cls(Object o) { return o == null ? "null" : o.getClass().getName(); }

    static HttpsURLConnection https(String host, String path) throws IOException {
        return (HttpsURLConnection) new URL("https://" + host + ":" + port + path).openConnection();
    }

    static HttpsURLConnection trusted(String path) throws IOException {
        HttpsURLConnection c = https("localhost", path);
        c.setSSLSocketFactory(clientCtx.getSocketFactory());
        return c;
    }

    public static void main(String[] args) throws Exception {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 6;
        KeyPairGenerator kpg = KeyPairGenerator.getInstance("RSA");
        kpg.initialize(2048);
        KeyPair kp = kpg.generateKeyPair();
        leaf = selfSigned(kp.getPrivate(), kp.getPublic(), "localhost", 42);
        X509Certificate other = selfSigned(kp.getPrivate(), kp.getPublic(), "Other Root", 7);
        KeyStore ks = KeyStore.getInstance("PKCS12");
        ks.load(null, null);
        ks.setKeyEntry("k", kp.getPrivate(), PW, new java.security.cert.Certificate[] {leaf});
        KeyStore ts = KeyStore.getInstance("PKCS12");
        ts.load(null, null);
        ts.setCertificateEntry("ca", leaf);
        KeyStore bad = KeyStore.getInstance("PKCS12");
        bad.load(null, null);
        bad.setCertificateEntry("o", other);
        serverCtx = ctx(ks, null);
        clientCtx = ctx(null, ts);
        untrustingCtx = ctx(null, bad);

        HttpsServer server = HttpsServer.create(new InetSocketAddress(InetAddress.getByName("127.0.0.1"), 0), 0);
        server.setHttpsConfigurator(new HttpsConfigurator(serverCtx) {
            @Override
            public void configure(HttpsParameters p) {
                p.setSSLParameters(getSSLContext().getDefaultSSLParameters());
            }
        });
        server.createContext("/hello", ex -> {
            ex.getResponseHeaders().add("X-Sbr", "w6");
            ex.getResponseHeaders().set("Content-Type", "text/plain; charset=utf-8");
            send(ex, 200, "hi tls");
        });
        server.createContext("/echo", ex -> {
            String body = str(ex.getRequestBody());
            send(ex, 200, ex.getRequestMethod() + " " + ex.getRequestURI() + " " + ex.getProtocol()
                    + " ct=" + ex.getRequestHeaders().getFirst("Content-Type")
                    + " cl=" + ex.getRequestHeaders().getFirst("Content-Length") + " body=" + body);
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
        server.createContext("/err", ex -> send(ex, 500, "boom"));
        server.createContext("/redirect", ex -> {
            ex.getResponseHeaders().set("Location", "/hello");
            send(ex, 302, "");
        });
        server.start();
        port = server.getAddress().getPort();
        System.out.println("cls server = " + server.getClass().getName());

        System.out.println("cls huc = " + safe(() -> x(() -> cls(https("localhost", "/hello")))));
        row("hucs.get", () -> x(() -> {
            HttpsURLConnection c = trusted("/hello");
            int code = c.getResponseCode();
            return "code=" + code + " msg=" + c.getResponseMessage() + " body=" + str(c.getInputStream())
                    + " x-sbr=" + c.getHeaderField("X-Sbr") + " type=" + c.getContentType()
                    + " len=" + c.getContentLength() + "/" + c.getContentLengthLong() + " method=" + c.getRequestMethod();
        }));
        row("hucs.headers", () -> x(() -> {
            HttpsURLConnection c = trusted("/hello");
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
        row("hucs.session", () -> x(() -> {
            HttpsURLConnection c = trusted("/hello");
            c.connect();
            String s = c.getCipherSuite() + " certs=" + c.getServerCertificates().length
                    + " subj=" + ((X509Certificate) c.getServerCertificates()[0]).getSubjectX500Principal().getName().contains("CN=localhost")
                    + " peer=" + c.getPeerPrincipal().getName().contains("CN=localhost")
                    + " local=" + c.getLocalCertificates() + " localP=" + c.getLocalPrincipal();
            str(c.getInputStream());
            return s;
        }));
        row("hucs.sslSession", () -> x(() -> {
            HttpsURLConnection c = trusted("/hello");
            c.connect();
            java.util.Optional<javax.net.ssl.SSLSession> s = c.getSSLSession();
            String r = "present=" + s.isPresent() + (s.isPresent() ? " proto=" + s.get().getProtocol() : "");
            str(c.getInputStream());
            return r;
        }));
        row("hucs.post", () -> x(() -> {
            HttpsURLConnection c = trusted("/echo?x=1");
            c.setRequestMethod("POST");
            c.setDoOutput(true);
            c.setRequestProperty("Content-Type", "text/plain");
            byte[] b = "posted-tls".getBytes(StandardCharsets.UTF_8);
            c.setFixedLengthStreamingMode(b.length);
            try (OutputStream o = c.getOutputStream()) {
                o.write(b);
            }
            return c.getResponseCode() + " " + str(c.getInputStream());
        }));
        row("hucs.post.buffered", () -> x(() -> {
            HttpsURLConnection c = trusted("/echo");
            c.setRequestMethod("POST");
            c.setDoOutput(true);
            try (OutputStream o = c.getOutputStream()) {
                o.write("buffered".getBytes(StandardCharsets.UTF_8));
            }
            return c.getResponseCode() + " " + str(c.getInputStream());
        }));
        row("hucs.chunked", () -> x(() -> {
            HttpsURLConnection c = trusted("/chunked");
            return c.getResponseCode() + " te=" + c.getHeaderField("Transfer-Encoding") + " len=" + c.getContentLength()
                    + " body=" + str(c.getInputStream());
        }));
        row("hucs.error", () -> x(() -> {
            HttpsURLConnection c = trusted("/err");
            int code = c.getResponseCode();
            String in;
            try {
                c.getInputStream();
                in = "no throw";
            } catch (IOException e) {
                in = e.getClass().getName();
            }
            return code + " in=" + in + " err=" + str(c.getErrorStream());
        }));
        row("hucs.redirect", () -> x(() -> {
            HttpsURLConnection c = trusted("/redirect");
            return c.getResponseCode() + " follow=" + c.getInstanceFollowRedirects() + " url=" + c.getURL().getPath() + " body=" + str(c.getInputStream());
        }));
        row("hucs.redirect.nofollow", () -> x(() -> {
            HttpsURLConnection c = trusted("/redirect");
            c.setInstanceFollowRedirects(false);
            return c.getResponseCode() + " loc=" + c.getHeaderField("Location") + " follow=" + c.getInstanceFollowRedirects();
        }));
        row("hucs.props", () -> x(() -> {
            HttpsURLConnection c = trusted("/hello");
            c.setConnectTimeout(1234);
            c.setReadTimeout(4321);
            c.setUseCaches(false);
            c.setRequestProperty("X-A", "1");
            c.addRequestProperty("X-A", "2");
            String s = "ct=" + c.getConnectTimeout() + " rt=" + c.getReadTimeout() + " uc=" + c.getUseCaches()
                    + " props=" + c.getRequestProperties().get("X-A") + " rp=" + c.getRequestProperty("X-A")
                    + " proxy=" + c.usingProxy();
            c.connect();
            String t;
            try {
                c.setRequestProperty("X-B", "1");
                t = "no throw";
            } catch (IllegalStateException e) {
                t = "ISE " + e.getMessage();
            }
            str(c.getInputStream());
            return s + " afterConnect=" + t;
        }));
        row("hucs.factory.default", () -> x(() -> {
            HttpsURLConnection c = https("localhost", "/hello");
            SSLSocketFactory d = HttpsURLConnection.getDefaultSSLSocketFactory();
            return "same=" + (c.getSSLSocketFactory() == d) + " cls=" + cls(d) + " dhv=" + (HttpsURLConnection.getDefaultHostnameVerifier() == c.getHostnameVerifier());
        }));
        row("hucs.factory.set", () -> x(() -> {
            HttpsURLConnection c = https("localhost", "/hello");
            SSLSocketFactory f = clientCtx.getSocketFactory();
            c.setSSLSocketFactory(f);
            HostnameVerifier hv = (h, s) -> true;
            c.setHostnameVerifier(hv);
            return "sf=" + (c.getSSLSocketFactory() == f) + " hv=" + (c.getHostnameVerifier() == hv);
        }));
        row("hucs.untrusted", () -> x(() -> {
            HttpsURLConnection c = https("localhost", "/hello");
            c.setSSLSocketFactory(untrustingCtx.getSocketFactory());
            try {
                c.getResponseCode();
                return "no throw";
            } catch (IOException e) {
                return e.getClass().getName();
            }
        }));
        row("hucs.hostname.reject", () -> x(() -> {
            HttpsURLConnection c = https("127.0.0.1", "/hello");
            c.setSSLSocketFactory(clientCtx.getSocketFactory());
            try {
                c.getResponseCode();
                return "no throw";
            } catch (IOException e) {
                return e.getClass().getName() + ": " + e.getMessage();
            }
        }));
        row("hucs.hostname.verifier", () -> x(() -> {
            HttpsURLConnection c = https("127.0.0.1", "/hello");
            c.setSSLSocketFactory(clientCtx.getSocketFactory());
            final String[] seen = new String[1];
            c.setHostnameVerifier((h, s) -> { seen[0] = h; return true; });
            return c.getResponseCode() + " verified=" + seen[0] + " body=" + str(c.getInputStream());
        }));
        row("hucs.refused", () -> x(() -> {
            HttpsURLConnection c = (HttpsURLConnection) new URL("https://localhost:1/x").openConnection();
            try {
                c.getResponseCode();
                return "no throw";
            } catch (IOException e) {
                return e.getClass().getName();
            }
        }));
        row("hucs.openStream", () -> x(() -> {
            HttpsURLConnection.setDefaultSSLSocketFactory(clientCtx.getSocketFactory());
            try {
                return str(new URL("https://localhost:" + port + "/hello").openStream());
            } finally {
                HttpsURLConnection.setDefaultSSLSocketFactory(SSLContext.getDefault().getSocketFactory());
            }
        }));

        // ---- java.net.http over TLS ------------------------------------------
        HttpClient hc = HttpClient.newBuilder().sslContext(clientCtx).version(HttpClient.Version.HTTP_1_1).build();
        HttpClient untrusting = HttpClient.newBuilder().sslContext(untrustingCtx).version(HttpClient.Version.HTTP_1_1).build();
        System.out.println("cls hc = " + cls(hc));
        row("hcs.get", () -> x(() -> {
            HttpResponse<String> r = hc.send(HttpRequest.newBuilder(URI.create("https://localhost:" + port + "/hello")).build(), HttpResponse.BodyHandlers.ofString());
            return r.statusCode() + " " + r.version() + " body=" + r.body() + " x-sbr=" + r.headers().firstValue("x-sbr").orElse("-")
                    + " ssl=" + r.sslSession().isPresent() + " uri=" + r.uri();
        }));
        row("hcs.post", () -> x(() -> {
            HttpResponse<String> r = hc.send(HttpRequest.newBuilder(URI.create("https://localhost:" + port + "/echo"))
                    .POST(HttpRequest.BodyPublishers.ofString("hc-tls")).header("Content-Type", "text/plain").build(), HttpResponse.BodyHandlers.ofString());
            return r.statusCode() + " " + r.body();
        }));
        row("hcs.async", () -> x(() -> {
            HttpResponse<String> r = hc.sendAsync(HttpRequest.newBuilder(URI.create("https://localhost:" + port + "/hello")).build(), HttpResponse.BodyHandlers.ofString()).get();
            return r.statusCode() + " " + r.body();
        }));
        row("hcs.ctx", () -> "sslContext-same=" + (hc.sslContext() == clientCtx) + " version=" + hc.version());
        row("hcs.untrusted", () -> x(() -> {
            try {
                untrusting.send(HttpRequest.newBuilder(URI.create("https://localhost:" + port + "/hello")).build(), HttpResponse.BodyHandlers.ofString());
                return "no throw";
            } catch (IOException e) {
                Throwable t = e;
                StringBuilder chain = new StringBuilder();
                while (t != null && chain.length() < 200) { chain.append(t.getClass().getName()).append('>'); t = t.getCause(); }
                return chain.toString();
            }
        }));
        row("proxy.select", () -> x(() -> {
            ProxySelector ps = ProxySelector.getDefault();
            return cls(ps) + " https=" + ps.select(URI.create("https://localhost:" + port + "/")) + " http=" + ps.select(URI.create("http://example.invalid/"));
        }));
        row("proxy.connectFailed", () -> x(() -> {
            ProxySelector.getDefault().connectFailed(URI.create("http://example.invalid/"), new InetSocketAddress("127.0.0.1", 1), new IOException("x"));
            return "ok";
        }));

        String[] first = new String[ROWS.size()];
        for (int i = 0; i < ROWS.size(); i++) {
            first[i] = safe(ROWS.get(i));
            System.out.println("row " + LABELS.get(i) + " = " + first[i]);
        }
        int drift = 0;
        for (int k = 0; k < iters; k++) {
            for (int i = 0; i < ROWS.size(); i++) {
                String v = safe(ROWS.get(i));
                if (!v.equals(first[i])) {
                    if (drift < 10) System.out.println("DRIFT at iter " + k + " " + LABELS.get(i) + " = " + v);
                    drift++;
                }
            }
        }
        server.stop(0);
        System.out.println("rows " + ROWS.size() + " drift " + drift);
        System.exit(0);
    }
}
