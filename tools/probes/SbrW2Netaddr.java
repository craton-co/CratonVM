// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import com.sun.net.httpserver.HttpServer;
import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.DatagramPacket;
import java.net.DatagramSocket;
import java.net.HttpURLConnection;
import java.net.Inet6Address;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.JarURLConnection;
import java.net.NetworkInterface;
import java.net.ServerSocket;
import java.net.Socket;
import java.net.URI;
import java.net.URL;
import java.net.URLClassLoader;
import java.net.URLConnection;
import java.net.URLStreamHandler;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.nio.charset.StandardCharsets;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import java.util.function.Supplier;
import java.util.jar.JarEntry;
import java.util.jar.JarOutputStream;

/**
 * SBR wave 2, lane {@code netaddr}: the retired {@code InetAddress} /
 * {@code Inet4Address} / {@code Inet6Address} surface and the retired
 * {@code java.net.URL} surface, over addresses and URLs from every producer
 * the VM has (the JDK's own factories, the genuine resolver natives, sockets,
 * {@code NetworkInterface}, class-loader resource lookups, code sources).
 *
 * <p>Every row is evaluated ONCE and printed (the first-run value -- a native
 * that wins from the first call shows here), then re-evaluated inside a hot
 * loop and compared with that first value; {@code drift} counts mismatches, so
 * cold == warm is asserted rather than assumed. Diff the whole output against
 * HotSpot 25. Host-dependent text (ports, the working directory) is
 * normalised so the diff is exact.
 *
 * <pre>
 *   java SbrW2Netaddr                       (HotSpot oracle)
 *   cratonvm --jdk-only -cp . SbrW2Netaddr
 * </pre>
 */
public class SbrW2Netaddr {
    static final List<String> LABELS = new ArrayList<>();
    static final List<Supplier<String>> ROWS = new ArrayList<>();
    static String cwd;
    static File jar;

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
        if (cwd != null) s = s.replace(cwd, "$CWD");
        return s.replaceAll(":[0-9]{4,5}(?=[/\\s|,\\]]|$)", ":PORT");
    }

    static String desc(InetAddress a) {
        return a.getClass().getSimpleName() + " str=" + a + " ha=" + a.getHostAddress()
                + " bytes=" + Arrays.toString(a.getAddress()) + " hash=" + a.hashCode()
                + " any=" + a.isAnyLocalAddress() + " lb=" + a.isLoopbackAddress()
                + " ll=" + a.isLinkLocalAddress() + " sl=" + a.isSiteLocalAddress()
                + " mc=" + a.isMulticastAddress() + " eqSelf=" + a.equals(a)
                + (a instanceof Inet6Address i6 ? " scope=" + i6.getScopeId() : "");
    }

    static String urlDesc(URL u) {
        return "str=" + u + " ext=" + u.toExternalForm() + " proto=" + u.getProtocol()
                + " auth=" + u.getAuthority() + " host=" + u.getHost() + " port=" + u.getPort()
                + " dport=" + u.getDefaultPort() + " file=" + u.getFile() + " ref=" + u.getRef()
                + " uri=" + x(u::toURI) + " eqCopy=" + u.equals(x(() -> new URL(u.toString())))
                + " hashCopy=" + (u.hashCode() == x(() -> new URL(u.toString())).hashCode())
                + " sameFile=" + u.sameFile(x(() -> new URL(u.toString())));
    }

    static byte[] read(InputStream in) throws Exception {
        try (in) {
            return in.readAllBytes();
        }
    }

    static String conn(URL u) {
        URLConnection c = x(u::openConnection);
        String body = new String(x(() -> read(c.getInputStream())), StandardCharsets.ISO_8859_1);
        return c.getClass().getName() + " len=" + body.length() + " head="
                + body.substring(0, Math.min(8, body.length())).replaceAll("[^ -~]", "?");
    }

    public static void main(String[] args) throws Exception {
        cwd = new File(".").getCanonicalFile().toURI().getPath();
        if (cwd.endsWith("/")) cwd = cwd.substring(0, cwd.length() - 1);
        jar = new File(System.getProperty("java.io.tmpdir"), "sbrw2netaddr-" + ProcessHandle.current().pid() + ".jar");
        jar.deleteOnExit();
        try (JarOutputStream jo = new JarOutputStream(new FileOutputStream(jar))) {
            jo.putNextEntry(new JarEntry("a/hello.txt"));
            jo.write("hello from a jar\n".getBytes(StandardCharsets.UTF_8));
            jo.closeEntry();
        }
        String jarPath = jar.getCanonicalFile().toURI().getPath();

        // ---- InetAddress factories over literal and named inputs -------------
        for (String in : new String[] {"127.0.0.1", "localhost", "::1", "0:0:0:0:0:0:0:1",
                "fe80::1%1", "10.1.2.3", "230.0.0.1", "169.254.1.1", "0.0.0.0", "::",
                "::ffff:127.0.0.1", "1.2.3", "[::1]", "ff02::1", "fec0::1", "192.168.0.1"}) {
            row("getByName(" + in + ")", () -> desc(x(() -> InetAddress.getByName(in))));
        }
        row("getByName(null)", () -> desc(x(() -> InetAddress.getByName(null))));
        row("getByName(\"\")", () -> desc(x(() -> InetAddress.getByName(""))));
        row("getByName(0x7f.0.0.1)", () -> desc(x(() -> InetAddress.getByName("0x7f.0.0.1"))));
        row("getByName(1::2::3)", () -> desc(x(() -> InetAddress.getByName("1::2::3"))));
        row("getByName(nx)", () -> desc(x(() -> InetAddress.getByName("no-such-host.invalid"))));
        row("getAllByName(localhost)", () -> {
            List<String> l = new ArrayList<>();
            for (InetAddress a : x(() -> InetAddress.getAllByName("localhost"))) l.add(a.toString());
            return l.toString();
        });
        row("getAllByName(127.0.0.1)", () -> Arrays.toString(x(() -> InetAddress.getAllByName("127.0.0.1"))));
        row("getByAddress(v4)", () -> desc(x(() -> InetAddress.getByAddress(new byte[] {127, 0, 0, 1}))));
        row("getByAddress(v6)", () -> desc(x(() -> InetAddress.getByAddress(
                new byte[] {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1}))));
        row("getByAddress(mapped)", () -> desc(x(() -> InetAddress.getByAddress(
                new byte[] {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, (byte) 255, (byte) 255, 10, 0, 0, 1}))));
        row("getByAddress(3)", () -> desc(x(() -> InetAddress.getByAddress(new byte[] {1, 2, 3}))));
        row("getByAddress(name,v4)", () -> desc(x(() -> InetAddress.getByAddress("h.example", new byte[] {10, 0, 0, 7}))));
        row("getByAddress(name,v6)", () -> desc(x(() -> InetAddress.getByAddress("h6.example",
                new byte[] {(byte) 0xfe, (byte) 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 9}))));
        row("getLoopbackAddress", () -> desc(InetAddress.getLoopbackAddress()) + " hn="
                + InetAddress.getLoopbackAddress().getHostName());
        row("getLocalHost", () -> {
            InetAddress a = x(InetAddress::getLocalHost);
            return a.getClass().getSimpleName() + " " + a + " hn=" + a.getHostName();
        });
        row("hostName(127.0.0.1)", () -> {
            InetAddress a = x(() -> InetAddress.getByName("127.0.0.1"));
            String before = a.toString();
            return before + " hn=" + a.getHostName() + " after=" + a;
        });
        row("canonical(127.0.0.1)", () -> x(() -> InetAddress.getByName("127.0.0.1")).getCanonicalHostName());
        row("hostName(localhost)", () -> x(() -> InetAddress.getByName("localhost")).getHostName());
        row("equals(v4 lit, name)", () -> x(() -> InetAddress.getByName("127.0.0.1"))
                .equals(x(() -> InetAddress.getByName("localhost"))) + "");
        row("equals(v4, bytes)", () -> x(() -> InetAddress.getByName("10.1.2.3"))
                .equals(x(() -> InetAddress.getByAddress(new byte[] {10, 1, 2, 3}))) + "");
        row("equals(v6, bytes)", () -> x(() -> InetAddress.getByName("::1"))
                .equals(InetAddress.getLoopbackAddress()) + "");
        row("isReachable(lo)", () -> "" + x(() -> InetAddress.getLoopbackAddress().isReachable(2000)));

        // ---- InetSocketAddress over real and minted addresses ----------------
        row("isa(getByName lit)", () -> {
            InetSocketAddress s = new InetSocketAddress(x(() -> InetAddress.getByName("127.0.0.1")), 80);
            return s + " hs=" + s.getHostString() + " a=" + s.getAddress() + " unres=" + s.isUnresolved();
        });
        row("isa(loopback)", () -> {
            InetSocketAddress s = new InetSocketAddress(InetAddress.getLoopbackAddress(), 81);
            return s + " hs=" + s.getHostString() + " a=" + s.getAddress();
        });
        row("isa(localhost,82)", () -> {
            InetSocketAddress s = new InetSocketAddress("localhost", 82);
            return s + " hs=" + s.getHostString() + " a=" + s.getAddress();
        });

        // ---- sockets: an address from real bytecode reaching the socket natives
        row("socket(127.0.0.1)", () -> x(() -> {
            InetAddress lo = InetAddress.getByName("127.0.0.1");
            try (ServerSocket ss = new ServerSocket(0, 50, lo);
                    Socket c = new Socket(lo, ss.getLocalPort());
                    Socket s = ss.accept()) {
                return "ss=" + ss.getInetAddress() + " peer=" + c.getInetAddress() + " local="
                        + c.getLocalAddress() + " acc=" + s.getInetAddress().getHostAddress()
                        + " rsa=" + c.getRemoteSocketAddress();
            }
        }));
        row("socket(isa lit)", () -> x(() -> {
            try (ServerSocket ss = new ServerSocket()) {
                ss.bind(new InetSocketAddress(InetAddress.getByName("127.0.0.1"), 0));
                try (Socket c = new Socket()) {
                    c.connect(new InetSocketAddress(InetAddress.getByName("127.0.0.1"), ss.getLocalPort()), 2000);
                    try (Socket s = ss.accept()) {
                        return "bound=" + ss.getLocalSocketAddress() + " conn=" + c.getRemoteSocketAddress();
                    }
                }
            }
        }));
        row("channel(loopback)", () -> x(() -> {
            try (ServerSocketChannel ss = ServerSocketChannel.open()) {
                ss.bind(new InetSocketAddress(InetAddress.getByName("127.0.0.1"), 0));
                int p = ((InetSocketAddress) ss.getLocalAddress()).getPort();
                try (SocketChannel c = SocketChannel.open(new InetSocketAddress(InetAddress.getLoopbackAddress(), p));
                        SocketChannel s = ss.accept()) {
                    return "remote=" + c.getRemoteAddress() + " acc=" + ((InetSocketAddress) s.getRemoteAddress()).getAddress();
                }
            }
        }));
        row("datagram(127.0.0.1)", () -> x(() -> {
            InetAddress lo = InetAddress.getByName("127.0.0.1");
            try (DatagramSocket d = new DatagramSocket(0, lo)) {
                byte[] b = "ping".getBytes(StandardCharsets.US_ASCII);
                d.send(new DatagramPacket(b, b.length, lo, d.getLocalPort()));
                DatagramPacket r = new DatagramPacket(new byte[16], 16);
                d.setSoTimeout(2000);
                d.receive(r);
                return "from=" + r.getAddress() + " local=" + d.getLocalAddress() + " data="
                        + new String(r.getData(), 0, r.getLength(), StandardCharsets.US_ASCII);
            }
        }));
        row("nif(lo)", () -> x(() -> {
            NetworkInterface lo = NetworkInterface.getByInetAddress(InetAddress.getByName("127.0.0.1"));
            List<String> l = new ArrayList<>();
            for (InetAddress a : Collections.list(lo.getInetAddresses())) l.add(a + "|" + a.getHostName());
            Collections.sort(l);
            return lo.getName() + " " + l;
        }));

        // ---- java.net.URL ----------------------------------------------------
        for (String s : new String[] {"http://localhost:8080/a/b?q=1#f", "https://localhost/x",
                "http://127.0.0.1/", "ftp://localhost/f.txt", "file:/tmp/a%20b/c", "file:///tmp/x",
                "jar:file:/tmp/x.jar!/a/b.class", "jrt:/java.base/java/lang/Object.class",
                "http://[::1]:9/p"}) {
            row("url(" + s + ")", () -> urlDesc(x(() -> new URL(s))));
        }
        row("url.equals(ref)", () -> "" + x(() -> new URL("http://localhost/a#x")).equals(x(() -> new URL("http://localhost/a#y"))));
        row("url.sameFile(ref)", () -> "" + x(() -> new URL("http://localhost/a#x")).sameFile(x(() -> new URL("http://localhost/a#y"))));
        row("url.equals(port)", () -> "" + x(() -> new URL("http://localhost:80/a")).equals(x(() -> new URL("http://localhost/a"))));
        row("url.toURI(space)", () -> "" + x(() -> new URL("http://localhost/a b").toURI()));
        row("getResource(self)", () -> urlDesc(SbrW2Netaddr.class.getResource("SbrW2Netaddr.class")));
        row("getResource(self).conn", () -> conn(SbrW2Netaddr.class.getResource("SbrW2Netaddr.class")));
        row("getSystemResource", () -> urlDesc(ClassLoader.getSystemResource("SbrW2Netaddr.class")));
        row("getResources.count", () -> "" + Collections.list(x(() -> ClassLoader.getSystemClassLoader()
                .getResources("SbrW2Netaddr.class"))).size());
        row("jrt(Object)", () -> urlDesc(Object.class.getResource("Object.class")));
        row("jrt(Object).conn", () -> conn(Object.class.getResource("Object.class")));
        row("codeSource", () -> {
            URL u = SbrW2Netaddr.class.getProtectionDomain().getCodeSource().getLocation();
            return urlDesc(u) + " path=" + Path.of(x(u::toURI));
        });
        row("path.toUri.toURL", () -> urlDesc(x(() -> Path.of("/tmp/a b/c").toUri().toURL())));
        row("uri.toURL", () -> urlDesc(x(() -> URI.create("file:/tmp/q.txt").toURL())));
        row("jar.url", () -> x(() -> {
            URL u = new URL("jar:file:" + jarPath + "!/a/hello.txt");
            URLConnection c = u.openConnection();
            String cls = c.getClass().getName();
            String extra = c instanceof JarURLConnection j
                    ? " entry=" + j.getEntryName() + " jfu=" + j.getJarFileURL().toString().replace(jarPath, "$JAR")
                    : "";
            return cls + extra + " body=" + new String(read(u.openStream()), StandardCharsets.UTF_8).trim();
        }));
        row("ucl(jar)", () -> x(() -> {
            try (URLClassLoader l = new URLClassLoader(new URL[] {jar.toURI().toURL()}, null)) {
                URL r = l.getResource("a/hello.txt");
                return r.toString().replace(jarPath, "$JAR") + " dport=" + r.getDefaultPort() + " body="
                        + new String(read(r.openStream()), StandardCharsets.UTF_8).trim();
            }
        }));
        row("url.openStream(file)", () -> x(() -> "len=" + read(jar.toURI().toURL().openStream()).length));

        // ---- http: the JDK's own HttpURLConnection over a loopback server -----
        HttpServer server = HttpServer.create(new InetSocketAddress(InetAddress.getByName("127.0.0.1"), 0), 0);
        server.createContext("/hello", ex -> {
            byte[] b = "hi there".getBytes(StandardCharsets.UTF_8);
            ex.getResponseHeaders().add("X-Sbr", "w2");
            ex.sendResponseHeaders(200, b.length);
            try (OutputStream o = ex.getResponseBody()) {
                o.write(b);
            }
        });
        server.start();
        int port = server.getAddress().getPort();
        row("http.get", () -> x(() -> {
            URL u = new URL("http://127.0.0.1:" + port + "/hello");
            HttpURLConnection c = (HttpURLConnection) u.openConnection();
            int code = c.getResponseCode();
            String body = new String(read(c.getInputStream()), StandardCharsets.UTF_8);
            String h = c.getHeaderField("X-Sbr");
            c.disconnect();
            return c.getClass().getName() + " code=" + code + " body=" + body + " hdr=" + h;
        }));
        row("http.openStream", () -> x(() -> new String(read(new URL("http://127.0.0.1:" + port + "/hello").openStream()),
                StandardCharsets.UTF_8)));
        row("http.404", () -> x(() -> {
            HttpURLConnection c = (HttpURLConnection) new URL("http://127.0.0.1:" + port + "/nope").openConnection();
            int code = c.getResponseCode();
            c.disconnect();
            return "code=" + code;
        }));

        String[] first = new String[ROWS.size()];
        for (int i = 0; i < ROWS.size(); i++) {
            first[i] = safe(ROWS.get(i));
            System.out.println("row " + LABELS.get(i) + " = " + first[i]);
        }
        int drift = 0;
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 200;
        for (int k = 0; k < iters; k++) {
            for (int i = 0; i < ROWS.size(); i++) {
                String v = safe(ROWS.get(i));
                if (!v.equals(first[i])) {
                    if (drift < 10) {
                        System.out.println("DRIFT at iter " + k + " " + LABELS.get(i) + " = " + v);
                    }
                    drift++;
                }
            }
        }
        server.stop(0);

        // ---- one-shot: the factory install is process-wide, so it runs last --
        System.out.println("oneshot factory1 = " + safe(() -> {
            URL.setURLStreamHandlerFactory(p -> "sbr".equals(p) ? new URLStreamHandler() {
                @Override
                protected URLConnection openConnection(URL u) {
                    return new URLConnection(u) {
                        @Override
                        public void connect() {}

                        @Override
                        public InputStream getInputStream() {
                            return new java.io.ByteArrayInputStream(("sbr:" + u.getPath()).getBytes(StandardCharsets.UTF_8));
                        }
                    };
                }
            } : null);
            return "installed";
        }));
        System.out.println("oneshot factory2 = " + safe(() -> {
            URL.setURLStreamHandlerFactory(p -> null);
            return "installed twice";
        }));
        System.out.println("oneshot sbr.url = " + safe(() -> {
            URL u = x(() -> new URL("sbr://h/p/q"));
            return u + " dport=" + u.getDefaultPort() + " body="
                    + new String(x(() -> read(u.openStream())), StandardCharsets.UTF_8);
        }));
        System.out.println("rows " + ROWS.size() + " drift " + drift);
    }
}
