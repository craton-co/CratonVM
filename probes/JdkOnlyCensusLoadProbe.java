import java.io.*;
import java.net.*;
import java.nio.*;
import java.nio.channels.*;
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.*;
import java.util.stream.*;

/**
 * A breadth-first workload for the schema-3 native census.
 *
 * It is NOT a benchmark and NOT a correctness probe. Its only job is to make
 * the census's `invocations` and `real_declaring_method` columns non-empty
 * across the registrar groups that carry a `JDK-ONLY-CLASSIFY: unknown --
 * needs census` verdict: collections and their interfaces, streams,
 * Properties, the io/nio/net stack, and the executor family.
 *
 * Every section is wrapped so one unsupported corner cannot truncate the run
 * and silently shrink the census -- a short run reads exactly like a clean one.
 */
public class JdkOnlyCensusLoadProbe {
    static int sections = 0, failed = 0;

    static void section(String name, Runnable body) {
        sections++;
        try {
            body.run();
        } catch (Throwable t) {
            failed++;
            System.out.println("SECTION-FAILED " + name + ": " + t);
        }
    }

    public static void main(String[] args) throws Exception {
        section("collections", JdkOnlyCensusLoadProbe::collections);
        section("interfaces", JdkOnlyCensusLoadProbe::interfaces);
        section("streams", JdkOnlyCensusLoadProbe::streams);
        section("properties", JdkOnlyCensusLoadProbe::properties);
        section("io", JdkOnlyCensusLoadProbe::io);
        section("nio", JdkOnlyCensusLoadProbe::nio);
        section("net", JdkOnlyCensusLoadProbe::net);
        section("concurrent", JdkOnlyCensusLoadProbe::concurrent);
        section("text", JdkOnlyCensusLoadProbe::text);
        System.out.println("CENSUSLOAD sections=" + sections + " failed=" + failed);
    }

    static void collections() {
        Map<String, Integer> hm = new HashMap<>();
        Map<String, Integer> lhm = new LinkedHashMap<>();
        Map<String, Integer> tm = new TreeMap<>();
        Map<String, Integer> chm = new ConcurrentHashMap<>();
        for (int i = 0; i < 500; i++) {
            String k = "k" + i;
            hm.put(k, i); lhm.put(k, i); tm.put(k, i); chm.put(k, i);
        }
        int sum = 0;
        for (Map.Entry<String, Integer> e : hm.entrySet()) sum += e.getValue();
        for (String k : tm.keySet()) sum += k.length();
        sum += lhm.size() + chm.size();
        List<Integer> al = new ArrayList<>(), ll = new LinkedList<>();
        for (int i = 0; i < 500; i++) { al.add(i); ll.add(i); }
        Collections.sort(al, Comparator.reverseOrder());
        Collections.reverse(ll);
        Set<Integer> hs = new HashSet<>(al), ts = new TreeSet<>(al);
        Deque<Integer> dq = new ArrayDeque<>(al);
        sum += hs.size() + ts.size() + dq.size() + al.indexOf(7) + (ll.contains(9) ? 1 : 0);
        Iterator<Integer> it = al.iterator();
        while (it.hasNext()) if (it.next() % 3 == 0) it.remove();
        System.out.println("collections sum=" + sum + " al=" + al.size());
    }

    /** Native-on-abstract-interface-method registrations intercept USER
     *  implementors too, which is the hazard `register_interface_natives`
     *  names. Exercise both a JDK implementor and a user-defined one. */
    static void interfaces() {
        Collection<String> user = new MyCollection();
        user.add("a"); user.add("b");
        int n = user.size() + (user.contains("a") ? 1 : 0) + user.stream().mapToInt(String::length).sum();
        List<String> jdk = new ArrayList<>(List.of("a", "b", "c"));
        n += jdk.size() + jdk.subList(0, 2).size();
        Map<String, String> um = new MyMap();
        um.put("x", "y");
        n += um.size() + um.getOrDefault("x", "").length();
        System.out.println("interfaces n=" + n);
    }

    static void streams() {
        long a = IntStream.range(0, 2000).filter(i -> i % 3 == 0).mapToLong(i -> i).sum();
        List<String> b = Stream.of("d", "b", "c", "a").sorted().collect(Collectors.toList());
        Map<Boolean, List<Integer>> c = IntStream.range(0, 200).boxed()
                .collect(Collectors.partitioningBy(i -> i % 2 == 0));
        String d = Stream.of("p", "q", "r").collect(Collectors.joining(",", "[", "]"));
        Optional<Integer> e = Stream.of(3, 1, 2).max(Integer::compare);
        long f = Arrays.stream(new int[]{1, 2, 3}).map(i -> i * i).count();
        System.out.println("streams " + a + " " + b + " " + c.get(true).size() + " " + d
                + " " + e.orElse(-1) + " " + f);
    }

    static void properties() {
        Properties p = new Properties();
        p.setProperty("alpha", "1");
        p.put("beta", "2");
        String home = System.getProperty("java.home");
        Properties sys = System.getProperties();
        int n = p.size() + p.stringPropertyNames().size()
                + (p.getProperty("alpha", "?").length()) + (home == null ? 0 : 1);
        StringWriter sw = new StringWriter();
        try {
            p.store(sw, "census");
            Properties back = new Properties();
            back.load(new StringReader(sw.toString()));
            n += back.size();
        } catch (IOException io) {
            throw new RuntimeException(io);
        }
        // Count a fixed set of load-bearing keys rather than sys.size().
        // The raw size is not comparable across VMs -- CratonVM deliberately
        // publishes some tuning properties HotSpot does not -- so comparing it
        // makes this line differ forever and trains a reader to skip the diff.
        // What is worth asserting is that nothing HotSpot guarantees is MISSING.
        String[] required = {
            "java.home", "java.version", "java.vendor", "java.class.path",
            "java.io.tmpdir", "java.library.path", "os.name", "os.arch",
            "os.version", "file.separator", "path.separator", "line.separator",
            "user.dir", "user.home", "user.name", "file.encoding",
            "native.encoding", "java.specification.version",
            "java.class.version", "java.vm.name", "java.vm.version",
            "sun.cpu.endian", "sun.io.unicode.encoding", "sun.java.command",
            "sun.java.launcher", "sun.jnu.encoding",
        };
        int present = 0;
        StringBuilder missing = new StringBuilder();
        for (String key : required) {
            if (System.getProperty(key) != null) present++;
            else missing.append(' ').append(key);
        }
        System.out.println("properties n=" + n + " required=" + present + "/" + required.length
                + " missing=[" + missing.toString().trim() + "]");
    }

    static void io() {
        try {
            File f = File.createTempFile("census", ".txt");
            f.deleteOnExit();
            try (Writer w = new BufferedWriter(new FileWriter(f))) {
                for (int i = 0; i < 200; i++) w.write("line " + i + "\n");
            }
            int lines = 0, chars = 0;
            try (BufferedReader r = new BufferedReader(new FileReader(f))) {
                String s;
                while ((s = r.readLine()) != null) { lines++; chars += s.length(); }
            }
            byte[] raw;
            try (InputStream in = new FileInputStream(f);
                 ByteArrayOutputStream bo = new ByteArrayOutputStream()) {
                byte[] buf = new byte[512];
                int k;
                while ((k = in.read(buf)) > 0) bo.write(buf, 0, k);
                raw = bo.toByteArray();
            }
            try (DataOutputStream dos = new DataOutputStream(new ByteArrayOutputStream())) {
                dos.writeInt(7); dos.writeUTF("census"); dos.writeDouble(1.5);
            }
            Scanner sc = new Scanner("10 20 hello");
            int scanned = sc.nextInt() + sc.nextInt() + sc.next().length();
            System.out.println("io lines=" + lines + " chars=" + chars
                    + " raw=" + raw.length + " len=" + f.length() + " scanned=" + scanned);
        } catch (IOException e) {
            throw new RuntimeException(e);
        }
    }

    static void nio() {
        try {
            Path p = Files.createTempFile("census-nio", ".bin");
            p.toFile().deleteOnExit();
            Files.write(p, "hello nio census".getBytes("UTF-8"));
            byte[] back = Files.readAllBytes(p);
            long size;
            try (FileChannel ch = FileChannel.open(p, StandardOpenOption.READ)) {
                ByteBuffer direct = ByteBuffer.allocateDirect(64);
                ch.read(direct);
                direct.flip();
                size = ch.size() + direct.remaining();
            }
            ByteBuffer heap = ByteBuffer.allocate(32);
            heap.putInt(1).putLong(2L).putDouble(3.0).flip();
            System.out.println("nio back=" + back.length + " size=" + size
                    + " heap=" + heap.getInt() + " exists=" + Files.exists(p));
        } catch (IOException e) {
            throw new RuntimeException(e);
        }
    }

    static void net() {
        try (ServerSocket srv = new ServerSocket(0)) {
            // Bounded on BOTH ends deliberately. Under --jdk-only the accept
            // thread dies (Thread.run()V is unsatisfied), and an unbounded
            // accept()/connect() then hangs the whole run -- which kills the
            // census this workload exists to produce, and reads exactly like a
            // clean short run.
            srv.setSoTimeout(4000);
            int port = srv.getLocalPort();
            Thread t = new Thread(() -> {
                try (Socket s = srv.accept();
                     OutputStream o = s.getOutputStream()) {
                    o.write("pong".getBytes("UTF-8"));
                    o.flush();
                } catch (IOException ignored) { }
            });
            t.setDaemon(true);
            t.start();
            Socket c = new Socket();
            try (Socket cc = c;
                 InputStream in = openWithTimeout(cc, port)) {
                byte[] b = new byte[8];
                int n = in.read(b);
                // The port is EPHEMERAL and must not be printed: it differs
                // between every pair of runs, so a transcript carrying it can
                // never be diffed against a control. This line read `port=` +
                // the number until 2026-08-05, which is why this probe was
                // believed byte-identical to HotSpot while diffing on every
                // single run. Assert its shape instead.
                System.out.println("net portAssigned=" + (port > 0 && port <= 65535)
                        + " read=" + n
                        + " local=" + InetAddress.getLoopbackAddress().getHostAddress());
            }
            t.join(2000);
        } catch (IOException | InterruptedException e) {
            throw new RuntimeException(e);
        }
    }

    static InputStream openWithTimeout(Socket c, int port) throws IOException {
        c.connect(new InetSocketAddress("127.0.0.1", port), 4000);
        c.setSoTimeout(4000);
        return c.getInputStream();
    }

    static void concurrent() {
        ExecutorService fixed = Executors.newFixedThreadPool(3);
        ExecutorService cached = Executors.newCachedThreadPool();
        try {
            List<Future<Integer>> fs = new ArrayList<>();
            for (int i = 0; i < 24; i++) {
                final int k = i;
                fs.add((i % 2 == 0 ? fixed : cached).submit(() -> k * k));
            }
            int sum = 0;
            for (Future<Integer> f : fs) sum += f.get(4, TimeUnit.SECONDS);
            CountDownLatch latch = new CountDownLatch(4);
            for (int i = 0; i < 4; i++) fixed.execute(latch::countDown);
            latch.await(5, TimeUnit.SECONDS);
            ConcurrentLinkedQueue<Integer> q = new ConcurrentLinkedQueue<>();
            LinkedBlockingQueue<Integer> bq = new LinkedBlockingQueue<>();
            for (int i = 0; i < 100; i++) { q.add(i); bq.offer(i); }
            System.out.println("concurrent sum=" + sum + " q=" + q.size() + " bq=" + bq.size());
        } catch (InterruptedException | ExecutionException | TimeoutException e) {
            throw new RuntimeException(e);
        } finally {
            fixed.shutdownNow();
            cached.shutdownNow();
        }
    }

    static void text() {
        StringJoiner sj = new StringJoiner(", ", "{", "}");
        for (int i = 0; i < 50; i++) sj.add("e" + i);
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 200; i++) sb.append(i).append('-');
        String s = sb.toString();
        System.out.println("text sj=" + sj.toString().length()
                + " sb=" + s.length()
                + " split=" + s.split("-").length
                + " fmt=" + String.format("%s/%d/%.2f", "x", 3, 1.5).length()
                + " up=" + "abc".toUpperCase() + " idx=" + s.indexOf("42"));
    }

    static class MyCollection extends AbstractCollection<String> {
        private final List<String> backing = new ArrayList<>();
        @Override public Iterator<String> iterator() { return backing.iterator(); }
        @Override public int size() { return backing.size(); }
        @Override public boolean add(String s) { return backing.add(s); }
    }

    static class MyMap extends AbstractMap<String, String> {
        private final Set<Entry<String, String>> es = new LinkedHashSet<>();
        @Override public Set<Entry<String, String>> entrySet() { return es; }
        @Override public String put(String k, String v) {
            es.add(new SimpleEntry<>(k, v));
            return null;
        }
    }
}
