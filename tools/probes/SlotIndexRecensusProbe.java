import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.channels.AsynchronousSocketChannel;
import java.util.Set;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.TimeUnit;

import com.sun.net.httpserver.HttpServer;

import javax.security.auth.Subject;

/**
 * Paired probe for the "synthetic slot index on a real JDK object" species
 * (W4-4-slot-index-species-sweep.md), re-censused in
 * W7-49-slot-index-recensus.md.
 *
 * <p>The trap this probe is built to avoid is the one that index of vacuous
 * greens is full of: a native writes slot <i>k</i>, a test reads slot <i>k</i>
 * back through the SAME native, and the pair agrees no matter how wrong
 * <i>k</i> is. Every read below therefore goes through a REAL JDK accessor —
 * a method whose body is JDK bytecode, reading the field by its own declared
 * index — never through the native that wrote it.
 *
 * <p>Three sections, and they are not all expected to be green. Each prints
 * outcomes as values, so the four transcripts (host JDK / CratonVM) x (before /
 * after the W7-49 fixes) diff directly.
 *
 * <ol>
 *   <li><b>CompletableFuture from {@code sendAsync}</b> — FIXED by W7-49.
 *       {@code HttpClientImpl.sendAsync} fabricated the future out of four slot
 *       indices while the real class declares two fields, {@code result} and
 *       the reference {@code stack}; the {@code done} Int went into
 *       {@code stack}. {@code getNumberOfDependents()} is the read that cannot
 *       be faked: its JDK body WALKS {@code stack} as a {@code Completion}
 *       chain, so it observes precisely the slot the native wrote, through the
 *       JDK's own index, and no native of ours is registered on it. Same for
 *       {@code thenApply}, which pushes onto that chain.</li>
 *   <li><b>{@code Subject.getPrincipals()}</b> — FIXED by W7-49 on the branch
 *       that fabricates a set. {@code size()} and {@code isEmpty()} are real
 *       {@code HashSet} bytecode dereferencing the {@code map} field, the one
 *       field the class declares; the old three-slot form left it null. NOTE
 *       the fabricating branch needs principals recorded Rust-side, which a
 *       plain probe cannot arrange — a fresh {@code Subject} takes the
 *       real-backing-set branch instead. This section therefore exercises the
 *       shape, not the repaired branch, and is honest about that rather than
 *       claiming a green it did not earn.</li>
 *   <li><b>{@code AsynchronousSocketChannel.provider()}</b> — NOT fixed, and
 *       printed so the next lane has its red. Real
 *       {@code AsynchronousSocketChannel} declares exactly one instance field,
 *       {@code provider}; both slot maps for that class (native-builtins' and
 *       native-io's, which disagree with each other) put an Int in it.
 *       {@code provider()} is real JDK bytecode returning that field.</li>
 * </ol>
 */
public class SlotIndexRecensusProbe {

    static String show(ThrowingSupplier<?> s) {
        try {
            Object v = s.get();
            return String.valueOf(v);
        } catch (Throwable t) {
            return t.getClass().getName() + (t.getMessage() == null ? "" : ": " + t.getMessage());
        }
    }

    interface ThrowingSupplier<T> {
        T get() throws Throwable;
    }

    public static void main(String[] args) throws Exception {
        completableFutureSection();
        subjectSection();
        asyncChannelSection();
    }

    // -- 1 --------------------------------------------------------------------

    static void completableFutureSection() {
        HttpServer server = null;
        try {
            server = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
            server.createContext("/probe", exchange -> {
                byte[] body = "ok".getBytes("UTF-8");
                exchange.sendResponseHeaders(200, body.length);
                try (OutputStream out = exchange.getResponseBody()) {
                    out.write(body);
                }
            });
            server.start();
            int port = server.getAddress().getPort();

            HttpClient client = HttpClient.newHttpClient();
            HttpRequest request = HttpRequest.newBuilder()
                    .uri(URI.create("http://127.0.0.1:" + port + "/probe"))
                    .GET()
                    .build();
            CompletableFuture<HttpResponse<String>> future =
                    client.sendAsync(request, HttpResponse.BodyHandlers.ofString());

            System.out.println("cf.class=" + show(() -> future.getClass().getName()));
            System.out.println("cf.isDone=" + show(future::isDone));
            // The load-bearing line: JDK bytecode walking `stack`, the field the
            // old four-slot map wrote an Int into.
            System.out.println("cf.numberOfDependents=" + show(future::getNumberOfDependents));
            System.out.println("cf.thenApply=" + show(() -> {
                CompletableFuture<Integer> chained = future.thenApply(r -> 1);
                return chained.get(10, TimeUnit.SECONDS);
            }));
            System.out.println("cf.status=" + show(() -> {
                HttpResponse<String> r = future.get(10, TimeUnit.SECONDS);
                return r == null ? "null-response" : Integer.toString(r.statusCode());
            }));
        } catch (Throwable t) {
            System.out.println("cf.section=" + t.getClass().getName()
                    + (t.getMessage() == null ? "" : ": " + t.getMessage()));
        } finally {
            if (server != null) {
                server.stop(0);
            }
        }
    }

    // -- 2 --------------------------------------------------------------------

    static void subjectSection() {
        System.out.println("subject.new=" + show(() -> {
            Subject s = new Subject();
            return s.getClass().getName();
        }));
        System.out.println("subject.principals.class=" + show(() -> {
            Subject s = new Subject();
            Set<?> p = s.getPrincipals();
            return p == null ? "null" : p.getClass().getName();
        }));
        // Real HashSet bytecode dereferencing `map` — the field the three-slot
        // form left null.
        System.out.println("subject.principals.size=" + show(() -> {
            Subject s = new Subject();
            return s.getPrincipals().size();
        }));
        System.out.println("subject.principals.isEmpty=" + show(() -> {
            Subject s = new Subject();
            return s.getPrincipals().isEmpty();
        }));
        System.out.println("subject.principals.iterator=" + show(() -> {
            Subject s = new Subject();
            return s.getPrincipals().iterator().hasNext();
        }));
        System.out.println("subject.publicCredentials.size=" + show(() -> {
            Subject s = new Subject();
            return s.getPublicCredentials().size();
        }));
    }

    // -- 3 --------------------------------------------------------------------

    static void asyncChannelSection() {
        System.out.println("asc.open=" + show(() -> {
            try (AsynchronousSocketChannel ch = AsynchronousSocketChannel.open()) {
                return ch.getClass().getName();
            }
        }));
        System.out.println("asc.isOpen=" + show(() -> {
            try (AsynchronousSocketChannel ch = AsynchronousSocketChannel.open()) {
                return ch.isOpen();
            }
        }));
        // Real JDK bytecode returning the ONE field the class declares — the
        // slot both of CratonVM's disagreeing maps write an Int into.
        System.out.println("asc.provider=" + show(() -> {
            try (AsynchronousSocketChannel ch = AsynchronousSocketChannel.open()) {
                Object p = ch.provider();
                return p == null ? "null" : p.getClass().getName();
            }
        }));
    }
}
