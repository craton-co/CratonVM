// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import com.sun.net.httpserver.HttpServer;
import java.io.InputStream;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.reflect.Array;
import java.lang.reflect.Field;
import java.lang.reflect.GenericArrayType;
import java.lang.reflect.Type;
import java.net.InetSocketAddress;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.channels.FileChannel;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;
import java.util.ArrayList;
import java.util.Collections;
import java.util.HashSet;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Random;
import java.util.Set;
import java.util.concurrent.atomic.AtomicLong;
import java.util.stream.IntStream;
import java.util.stream.Stream;

/**
 * One row per defect behind the `--jdk-only` Spring Framework ABEND/FAIL set
 * (docs/internal/fixed-suite-bugs/spring/
 * spring-jdkonly-673-abends-and-failures-FIXED-20260918.md). Every row is
 * deterministic; run under HotSpot and under `--jdk-only` and every line must
 * match.
 *
 * <pre>
 *   java SpringJdkOnlyResidualsProbe
 *   cratonvm --jdk-only --java-home $JAVA_HOME -cp . SpringJdkOnlyResidualsProbe
 * </pre>
 */
public class SpringJdkOnlyResidualsProbe {

    static void row(String name, Object value) {
        System.out.println(name + " = " + value);
    }

    /** `o instanceof PP that && set.equals(that.set)`: the shape that hit the callback memo. */
    static final class PP {
        final Set<String> ex = new LinkedHashSet<>();

        PP(String... e) {
            Collections.addAll(ex, e);
        }

        @Override
        public boolean equals(Object o) {
            return this == o || (o instanceof PP that && ex.equals(that.ex));
        }

        @Override
        public int hashCode() {
            return ex.hashCode();
        }
    }

    static final class SearchPathLike extends LinkedHashSet<String> {
    }

    volatile List<String>[] genericArray;

    public enum Mode {
        A, B
    }

    @Retention(RetentionPolicy.RUNTIME)
    public @interface Anno {
        Mode mode() default Mode.A;

        Mode[] modes() default {};
    }

    @Anno(mode = Mode.B, modes = {Mode.A, Mode.B})
    public static class Tagged {
        public Mode[] field;

        public void take(Mode[] modes, Mode m) {
        }
    }

    /** Parent-last: defines its own copy of every `SpringJdkOnlyResidualsProbe$*` class. */
    static final class ParentLast extends ClassLoader {
        ParentLast(ClassLoader parent) {
            super(parent);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                if (name.startsWith("SpringJdkOnlyResidualsProbe$")) {
                    Class<?> c = findLoadedClass(name);
                    if (c == null) {
                        try (InputStream in = getParent().getResourceAsStream(name.replace('.', '/') + ".class")) {
                            byte[] b = in.readAllBytes();
                            c = defineClass(name, b, 0, b.length);
                        } catch (java.io.IOException e) {
                            throw new ClassNotFoundException(name, e);
                        }
                    }
                    return c;
                }
                return super.loadClass(name, resolve);
            }
        }
    }

    static int count(Iterable<?> it) {
        int n = 0;
        for (Object o : it) {
            n++;
        }
        return n;
    }

    public static void main(String[] args) throws Exception {
        // 1. The integer-key overlay: real HashMap iteration must see the entries.
        Set<Integer> ints = new HashSet<>();
        for (int i : new int[] {17, 3, 2, 1}) {
            ints.add(i);
        }
        row("hashset-integer size/iter", ints.size() + "/" + count(ints));
        Set<Character> chars = new HashSet<>(List.of('a', 'b'));
        row("hashset-character size/iter", chars.size() + "/" + count(chars));

        // 2. A LinkedHashSet subclass must iterate its LinkedHashMap (javac SearchPath).
        SearchPathLike sp = new SearchPathLike();
        sp.add("x");
        sp.add("y");
        row("linkedhashset-subclass size/iter", sp.size() + "/" + count(sp));

        // 3. The callback memo: same call site, first and later executions.
        PP a = new PP("(spring & framework) | (spring & java)");
        PP b = new PP("(spring & framework) | (spring & java)");
        StringBuilder eq = new StringBuilder();
        for (int i = 0; i < 4; i++) {
            eq.append(a.equals(b)).append(b.equals(a)).append(' ');
        }
        row("set-equals through pattern instanceof", eq.toString().trim());

        // 4. A parallel forEach must terminate and see every element (FJP self-fork).
        AtomicLong sum = new AtomicLong();
        IntStream.range(0, 20_000).parallel().forEach(sum::addAndGet);
        row("parallel forEach sum", sum.get());

        // 5. System.getProperties().remove(k) must reach System.getProperty(k).
        String key = "residuals.probe.flag";
        System.setProperty(key, "true");
        System.getProperties().remove(key);
        row("getProperties().remove -> getProperty", System.getProperty(key));

        // 6. Generic array reflection types are the JDK's own implementation.
        Field f = SpringJdkOnlyResidualsProbe.class.getDeclaredField("genericArray");
        Type t = f.getGenericType();
        row("generic array type name", t.getTypeName());
        row("generic array is GenericArrayType", t instanceof GenericArrayType);
        row("generic array component", ((GenericArrayType) t).getGenericComponentType().getTypeName());

        // 7. Stream.concat with an infinite operand is lazy (`b.spliterator()`).
        Stream<Long> cat = Stream.concat(Stream.of(-1L), new Random(7).longs(0, 10).boxed());
        row("stream.concat(infinite).limit(3).count", cat.limit(3).count());

        // 8. Real FileChannel.open over a temp file.
        Path tmp = Files.createTempFile("residuals", ".txt");
        try {
            Files.writeString(tmp, "channel-body");
            try (FileChannel ch = FileChannel.open(tmp, StandardOpenOption.READ)) {
                java.nio.ByteBuffer bb = java.nio.ByteBuffer.allocate(64);
                int n = ch.read(bb);
                row("FileChannel.open read", n + ":" + new String(bb.array(), 0, n, StandardCharsets.UTF_8));
            }
        } finally {
            Files.deleteIfExists(tmp);
        }

        // 9. Two loaders, one binary name: one array
        // class per component class (5.3.3), reached the same way by every path.
        ParentLast child = new ParentLast(SpringJdkOnlyResidualsProbe.class.getClassLoader());
        Class<?> tagged = child.loadClass(Tagged.class.getName());
        Class<?> mode = child.loadClass(Mode.class.getName());
        Class<?> anno = child.loadClass(Anno.class.getName());
        Object annotation = tagged.getAnnotation(anno.asSubclass(java.lang.annotation.Annotation.class));
        Object[] modes = (Object[]) anno.getMethod("modes").invoke(annotation);
        row("annotation modes() return type == value class", anno.getMethod("modes").getReturnType() == modes.getClass());
        Class<?> fieldArray = tagged.getField("field").getType();
        row("child field type: component is child's enum", fieldArray.getComponentType() == mode);
        row("child field type == arrayType() == newInstance().getClass()",
                fieldArray == mode.arrayType() && fieldArray == Array.newInstance(mode, 0).getClass());
        row("child param type == field type", tagged.getMethod("take", fieldArray, mode).getParameterTypes()[0] == fieldArray);
        row("parent's array is not child's array", fieldArray != Mode[].class);
        boolean parentArrayMatches = true;
        try {
            tagged.getMethod("take", Mode[].class, mode);
        } catch (NoSuchMethodException e) {
            parentArrayMatches = false;
        }
        row("getMethod with the parent's Mode[] finds child's take", parentArrayMatches);

        // 10. HttpClient with a custom BodyHandler, and HttpURLConnection request properties.
        HttpServer server = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
        server.createContext("/x", ex -> {
            byte[] body = ("h=" + ex.getRequestHeaders().getFirst("Framework-Name")).getBytes(StandardCharsets.UTF_8);
            ex.sendResponseHeaders(200, body.length);
            ex.getResponseBody().write(body);
            ex.close();
        });
        server.start();
        try {
            String url = "http://127.0.0.1:" + server.getAddress().getPort() + "/x";
            HttpClient client = HttpClient.newHttpClient();
            HttpRequest req = HttpRequest.newBuilder(URI.create(url)).header("Framework-Name", "Spring").build();
            HttpResponse<String> r = client.send(req, info -> HttpResponse.BodySubscribers.ofString(StandardCharsets.UTF_8));
            row("HttpClient custom BodyHandler", r.statusCode() + " " + r.body());
            java.net.HttpURLConnection c = (java.net.HttpURLConnection) new java.net.URL(url).openConnection();
            c.setRequestProperty("Framework-Name", "Spring");
            row("HttpURLConnection request property", new String(c.getInputStream().readAllBytes(), StandardCharsets.UTF_8));
        } finally {
            server.stop(0);
        }

        // 11. sendAsync must return before the response exists: here the thread that calls
        // it is the one that will accept the connection, so an inline send never returns.
        java.util.concurrent.CompletableFuture<String> late = new java.util.concurrent.CompletableFuture<>();
        Thread lateServer = new Thread(() -> {
            try (java.net.ServerSocket ss = new java.net.ServerSocket(0)) {
                HttpRequest lateReq = HttpRequest.newBuilder(URI.create("http://localhost:" + ss.getLocalPort() + "/"))
                        .POST(HttpRequest.BodyPublishers.noBody()).build();
                java.util.concurrent.CompletableFuture<HttpResponse<String>> sent =
                        HttpClient.newHttpClient().sendAsync(lateReq, HttpResponse.BodyHandlers.ofString());
                row("sendAsync returned before the response", !sent.isDone());
                try (java.net.Socket s = ss.accept()) {
                    s.getInputStream().read(new byte[4096]);
                    s.getOutputStream().write("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".getBytes(StandardCharsets.UTF_8));
                    s.getOutputStream().flush();
                    late.complete(sent.get(10, java.util.concurrent.TimeUnit.SECONDS).body());
                }
            } catch (Throwable e) {
                late.completeExceptionally(e);
            }
        });
        lateServer.start();
        try {
            row("sendAsync on the accepting thread", late.get(20, java.util.concurrent.TimeUnit.SECONDS));
        } catch (Exception e) {
            row("sendAsync on the accepting thread", e.getClass().getSimpleName());
        }
        System.exit(0);
    }
}
