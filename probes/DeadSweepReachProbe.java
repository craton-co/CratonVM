import java.io.*;
import java.lang.invoke.*;
import java.lang.management.*;
import java.lang.reflect.*;
import java.nio.*;
import java.nio.channels.*;
import java.nio.charset.*;
import java.nio.file.*;
import java.security.*;
import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.*;
import java.util.function.*;
import java.util.logging.LogManager;

/**
 * Is `scripts/baselines/jdk-only-dead-everywhere.tsv` a list of DEAD
 * registrations, or a list of registrations nobody exercised?
 *
 * `scripts/jdk-only-dead-sweep.py` subtracts every triple its input censuses
 * dispatched, which makes the answer only as good as the workloads run. Two
 * broad workloads were run, and they caught four rows: `HashMap$KeyItr` and
 * `TreeSet$Itr`, iterating. `ArrayDeque$Itr`, `LinkedList$Itr` and
 * `TreeMap$KeyItr` are minted by the SAME `try_alloc_synthetic` calls in
 * `native-collections/src/lib.rs`, and are on the list — not because anything
 * measured them dead, but because no probe had iterated those three
 * collections.
 *
 * So this probe is not a breadth workload. Every section here exists to reach
 * one named family the list calls dead, and the only output that matters is
 * the `invocations` column of a census taken over it. A section that prints a
 * plausible value while its native never ran proves nothing, so the sections
 * print what they got AND the run is scored from the census, not from stdout.
 *
 *   cratonvm --real-jdk --java-home <JDK> --explain-jdk-only \
 *       --dump-native-registry reach.json -cp probes DeadSweepReachProbe
 *   python3 scripts/jdk-only-dead-sweep.py --images ... --inherited ... \
 *       --dispatched reach.json <others>
 *
 * Deliberately NOT merged into JdkOnlyBreadthProbe: that probe's sections are
 * chosen to cover JDK surface, this one's to cover a committed baseline, and
 * the two want to drift apart. When the baseline changes, this file changes
 * with it.
 */
public class DeadSweepReachProbe {
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

    static void p(String k, Object v) {
        System.out.println("  " + k + " = " + v);
    }

    // ---- the minted-iterator family (native-collections try_alloc_synthetic)

    static void iterators() {
        System.out.println("[iterators]");
        Deque<String> ad = new ArrayDeque<>(List.of("a", "b", "c"));
        int n = 0;
        for (Iterator<String> it = ad.iterator(); it.hasNext(); ) { it.next(); n++; }
        p("ArrayDeque$Itr", n);

        List<String> ll = new LinkedList<>(List.of("x", "y", "z"));
        n = 0;
        for (Iterator<String> it = ll.iterator(); it.hasNext(); ) { it.next(); n++; }
        p("LinkedList$Itr", n);
        // remove() is a separate registration on the same minted class
        Iterator<String> llr = ll.iterator();
        llr.next();
        llr.remove();
        p("LinkedList$Itr.remove -> size", ll.size());

        TreeMap<String, Integer> tm = new TreeMap<>();
        tm.put("k1", 1); tm.put("k2", 2);
        n = 0;
        for (Iterator<String> it = tm.keySet().iterator(); it.hasNext(); ) { it.next(); n++; }
        p("TreeMap$KeyItr", n);

        TreeSet<String> ts = new TreeSet<>(List.of("p", "q"));
        n = 0;
        for (Iterator<String> it = ts.iterator(); it.hasNext(); ) { it.next(); n++; }
        p("TreeSet$Itr", n);
        Iterator<String> tsr = ts.iterator();
        tsr.next();
        tsr.remove();
        p("TreeSet$Itr.remove -> size", ts.size());

        HashMap<String, Integer> hm = new HashMap<>();
        hm.put("a", 1); hm.put("b", 2);
        n = 0;
        for (Iterator<String> it = hm.keySet().iterator(); it.hasNext(); ) { it.next(); n++; }
        p("HashMap$KeyItr", n);
        // the minted 2-field HashMap$Entry, via an entrySet walk
        int sum = 0;
        for (Map.Entry<String, Integer> e : hm.entrySet()) sum += e.getValue();
        p("HashMap$Entry via entrySet", sum);

        PriorityQueue<Integer> pq = new PriorityQueue<>(List.of(3, 1, 2));
        n = 0;
        for (Iterator<Integer> it = pq.iterator(); it.hasNext(); ) { it.next(); n++; }
        p("PriorityQueue$Itr", n);
    }

    // ---- Enumeration stand-ins

    static void enumerations() {
        System.out.println("[enumerations]");
        Hashtable<String, String> ht = new Hashtable<>();
        ht.put("one", "1");
        Enumeration<String> keys = ht.keys();
        int n = 0;
        while (keys.hasMoreElements()) { keys.nextElement(); n++; }
        p("Hashtable.keys", n);

        Enumeration<String> ce = Collections.enumeration(List.of("a", "b"));
        n = 0;
        while (ce.hasMoreElements()) { ce.nextElement(); n++; }
        p("Collections.enumeration", n);

        // LogManager$StringEnumeration
        Enumeration<String> ln = LogManager.getLogManager().getLoggerNames();
        n = 0;
        while (ln.hasMoreElements()) { ln.nextElement(); n++; }
        p("LogManager.getLoggerNames", n);

        // java/util/IteratorEnumeration — KeyStore.aliases()
        try {
            KeyStore ks = KeyStore.getInstance(KeyStore.getDefaultType());
            ks.load(null, null);
            ks.setKeyEntry("nope", new byte[] {1, 2, 3}, new java.security.cert.Certificate[0]);
            Enumeration<String> al = ks.aliases();
            n = 0;
            while (al.hasMoreElements()) { al.nextElement(); n++; }
            p("KeyStore.aliases", n);
        } catch (Throwable t) {
            p("KeyStore.aliases", "threw " + t.getClass().getSimpleName());
        }
    }

    // ---- functional-interface combinators (Function$Compose, Consumer$AndThen,
    //      Predicate$$Lambda$And/Or/Negate)

    static void combinators() {
        System.out.println("[combinators]");
        Function<Integer, Integer> plus1 = x -> x + 1;
        Function<Integer, Integer> times2 = x -> x * 2;
        p("Function.compose", plus1.compose(times2).apply(5));
        p("Function.andThen", plus1.andThen(times2).apply(5));
        p("Function.identity", Function.identity().apply("id"));

        StringBuilder sb = new StringBuilder();
        Consumer<String> c1 = sb::append;
        Consumer<String> c2 = s -> sb.append(s.toUpperCase());
        c1.andThen(c2).accept("ab");
        p("Consumer.andThen", sb.toString());

        Predicate<Integer> even = x -> x % 2 == 0;
        Predicate<Integer> pos = x -> x > 0;
        p("Predicate.and", even.and(pos).test(4));
        p("Predicate.or", even.or(pos).test(3));
        p("Predicate.negate", even.negate().test(3));

        BiFunction<Integer, Integer, Integer> add = Integer::sum;
        p("BiFunction.andThen", add.andThen(plus1).apply(1, 2));
    }

    // ---- atomic field updaters (the $RustJvmImpl classes)

    static class Holder {
        volatile int i = 1;
        volatile long l = 2L;
        volatile String s = "s";
    }

    static void fieldUpdaters() {
        System.out.println("[fieldUpdaters]");
        Holder h = new Holder();
        AtomicIntegerFieldUpdater<Holder> iu =
                AtomicIntegerFieldUpdater.newUpdater(Holder.class, "i");
        iu.set(h, 7);
        iu.compareAndSet(h, 7, 8);
        iu.incrementAndGet(h);
        p("AtomicIntegerFieldUpdater", iu.get(h));

        AtomicLongFieldUpdater<Holder> lu =
                AtomicLongFieldUpdater.newUpdater(Holder.class, "l");
        lu.set(h, 70L);
        lu.compareAndSet(h, 70L, 80L);
        lu.addAndGet(h, 1L);
        p("AtomicLongFieldUpdater", lu.get(h));

        AtomicReferenceFieldUpdater<Holder, String> ru =
                AtomicReferenceFieldUpdater.newUpdater(Holder.class, String.class, "s");
        ru.set(h, "t");
        ru.compareAndSet(h, "t", "u");
        p("AtomicReferenceFieldUpdater", ru.get(h));
    }

    // ---- dynamic proxies (Proxy$Instance, Proxy$Dispatch)

    public interface Greeter {
        String hello(String who);
        int count();
    }

    static void proxies() {
        System.out.println("[proxies]");
        Greeter g = (Greeter) Proxy.newProxyInstance(
                DeadSweepReachProbe.class.getClassLoader(),
                new Class<?>[] {Greeter.class},
                (proxy, method, args) -> switch (method.getName()) {
                    case "hello" -> "hi " + args[0];
                    case "count" -> 42;
                    case "toString" -> "proxy";
                    case "hashCode" -> 1;
                    case "equals" -> proxy == args[0];
                    default -> null;
                });
        p("proxy.hello", g.hello("world"));
        p("proxy.count", g.count());
        p("proxy.toString", g.toString());
        p("Proxy.isProxyClass", Proxy.isProxyClass(g.getClass()));
        p("getInvocationHandler", Proxy.getInvocationHandler(g) != null);
        p("proxy interfaces", Arrays.toString(g.getClass().getInterfaces()));
    }

    // ---- direct buffers and their deallocators (DirectBufferDeallocator,
    //      BucketDirectBufferDeallocator, sun/misc/Cleaner)

    static void directBuffers() {
        System.out.println("[directBuffers]");
        ByteBuffer bb = ByteBuffer.allocateDirect(4096);
        bb.putInt(0, 0x0BADF00D);
        p("direct getInt", Integer.toHexString(bb.getInt(0)));
        p("isDirect", bb.isDirect());
        ByteBuffer slice = bb.slice();
        p("slice capacity", slice.capacity());
        // drop the reference and ask for a collection, so the deallocator path
        // is at least reachable; not asserted, because GC timing is not ours
        bb = null;
        slice = null;
        System.gc();
        p("post-gc", "requested");
        ByteBuffer small = ByteBuffer.allocateDirect(16);
        p("second direct", small.capacity());
    }

    // ---- java.lang.management (sun/management/Flag, OperatingSystemImpl,
    //      HotSpotDiagnostic, com.sun.jmx.mbeanserver converters)

    static void management() {
        System.out.println("[management]");
        OperatingSystemMXBean os = ManagementFactory.getOperatingSystemMXBean();
        p("os.arch", os.getArch());
        p("os.processors", os.getAvailableProcessors());
        p("os.loadAverage", os.getSystemLoadAverage() >= -1.0);
        RuntimeMXBean rt = ManagementFactory.getRuntimeMXBean();
        p("rt.name nonempty", rt.getName() != null && !rt.getName().isEmpty());
        p("rt.inputArgs", rt.getInputArguments().size() >= 0);
        MemoryMXBean mem = ManagementFactory.getMemoryMXBean();
        p("heap max>0", mem.getHeapMemoryUsage().getMax() != 0);
        ThreadMXBean th = ManagementFactory.getThreadMXBean();
        p("threadCount>0", th.getThreadCount() > 0);
        p("mbean count", ManagementFactory.getPlatformMBeanServer().getMBeanCount() > 0);
        // the MXBean type mappers are reached by asking for a CompositeData-
        // valued attribute through the MBeanServer
        try {
            javax.management.MBeanServer mbs = ManagementFactory.getPlatformMBeanServer();
            Object v = mbs.getAttribute(
                    new javax.management.ObjectName("java.lang:type=Memory"),
                    "HeapMemoryUsage");
            p("HeapMemoryUsage composite", v != null ? v.getClass().getSimpleName() : "null");
        } catch (Throwable t) {
            p("HeapMemoryUsage composite", "threw " + t.getClass().getSimpleName());
        }
    }

    // ---- ServiceLoader$Itr

    static void serviceLoader() {
        System.out.println("[serviceLoader]");
        int n = 0;
        Iterator<java.nio.file.spi.FileSystemProvider> it =
                ServiceLoader.load(java.nio.file.spi.FileSystemProvider.class).iterator();
        while (it.hasNext()) { it.next(); n++; }
        p("FileSystemProvider providers", n);
        n = 0;
        ServiceLoader<java.nio.charset.spi.CharsetProvider> sl =
                ServiceLoader.load(java.nio.charset.spi.CharsetProvider.class);
        for (java.nio.charset.spi.CharsetProvider cp : sl) { n++; }
        p("CharsetProvider providers", n);
        p("stream()", ServiceLoader.load(java.nio.file.spi.FileSystemProvider.class)
                .stream().count());
    }

    // ---- CompletedFuture (async channel completion)

    static void completedFuture() {
        System.out.println("[completedFuture]");
        Path tmp = null;
        try {
            tmp = Files.createTempFile("deadsweep", ".bin");
            Files.write(tmp, "hello-async".getBytes(StandardCharsets.UTF_8));
            try (AsynchronousFileChannel ch =
                         AsynchronousFileChannel.open(tmp, StandardOpenOption.READ)) {
                ByteBuffer buf = ByteBuffer.allocate(64);
                Future<Integer> f = ch.read(buf, 0);
                p("future.isDone (pre-get)", f.isDone());
                int got = f.get(10, TimeUnit.SECONDS);
                p("async read bytes", got);
                p("future.isDone", f.isDone());
                p("future.isCancelled", f.isCancelled());
                p("future.cancel", f.cancel(false));
            }
        } catch (Throwable t) {
            p("async", "threw " + t);
        } finally {
            if (tmp != null) { try { Files.deleteIfExists(tmp); } catch (IOException ignored) { } }
        }
    }

    // ---- SharedSecrets / AccessController$1 factories

    static void sharedSecrets() {
        System.out.println("[sharedSecrets]");
        // AccessController is the receiver whose $1 the VM mints for the
        // JavaSecurityAccess shared secret; reaching it at all is the point.
        p("doPrivileged", AccessController.doPrivileged(
                (PrivilegedAction<String>) () -> "privileged"));
        p("context", AccessController.getContext() != null);
        // JavaLangAccess is consulted by String/Integer paths under the hood;
        // touch a few of the surfaces that route through it
        p("String.join", String.join("-", "a", "b", "c"));
        p("Integer.parse", Integer.parseInt("123"));
        p("intern", "interned".intern());
        p("StringBuilder chain", new StringBuilder().append(1).append('c')
                .append("s").append(2L).append(true).toString());
    }

    // ---- the WatchService family (watch.rs, 27 rows)

    static void watchService() {
        System.out.println("[watchService]");
        Path dir = null;
        try {
            dir = Files.createTempDirectory("deadsweep-watch");
            try (WatchService ws = FileSystems.getDefault().newWatchService()) {
                WatchKey key = dir.register(ws, StandardWatchEventKinds.ENTRY_CREATE);
                p("registered", key.isValid());
                Files.writeString(dir.resolve("touched.txt"), "x");
                WatchKey polled = ws.poll(3, TimeUnit.SECONDS);
                p("polled", polled != null);
                if (polled != null) {
                    p("events", polled.pollEvents().size());
                    p("reset", polled.reset());
                }
                key.cancel();
                p("cancelled -> valid", key.isValid());
            }
        } catch (Throwable t) {
            p("watch", "threw " + t);
        } finally {
            if (dir != null) {
                try {
                    Files.walk(dir).sorted(Comparator.reverseOrder())
                            .forEach(x -> { try { Files.deleteIfExists(x); } catch (IOException ignored) { } });
                } catch (IOException ignored) { }
            }
        }
    }

    // ---- sockets: the plain_socket.rs family (51 rows)

    static void sockets() {
        System.out.println("[sockets]");
        try (java.net.ServerSocket ss = new java.net.ServerSocket(0)) {
            ss.setSoTimeout(3000);
            int port = ss.getLocalPort();
            p("bound port>0", port > 0);
            Thread t = new Thread(() -> {
                try (java.net.Socket c = new java.net.Socket("127.0.0.1", port)) {
                    c.getOutputStream().write("ping".getBytes(StandardCharsets.UTF_8));
                    c.getOutputStream().flush();
                    p("client wrote", 4);
                } catch (IOException e) {
                    p("client", "threw " + e);
                }
            });
            t.setDaemon(true);
            t.start();
            try (java.net.Socket s = ss.accept()) {
                byte[] b = new byte[4];
                int n = s.getInputStream().read(b);
                p("server read", n);
                p("payload", new String(b, 0, Math.max(n, 0), StandardCharsets.UTF_8));
                p("soLinger", s.getSoLinger());
                p("tcpNoDelay", s.getTcpNoDelay());
                p("available", s.getInputStream().available());
            }
            t.join(3000);
        } catch (Throwable t) {
            p("sockets", "threw " + t);
        }
    }

    // ---- Unsafe / VarHandle CAS family (unsafe_natives.rs, 21 rows)

    static void casFamily() {
        System.out.println("[casFamily]");
        try {
            VarHandle vh = MethodHandles.lookup()
                    .findVarHandle(Holder.class, "i", int.class);
            Holder h = new Holder();
            p("compareAndSet", vh.compareAndSet(h, 1, 5));
            p("getAndAdd", vh.getAndAdd(h, 2));
            p("compareAndExchange", vh.compareAndExchange(h, 7, 9));
            p("getAcquire", vh.getAcquire(h));
            p("weakCompareAndSet", vh.weakCompareAndSet(h, 9, 11));
            p("final", h.i);
        } catch (Throwable t) {
            p("varhandle", "threw " + t);
        }
        AtomicInteger ai = new AtomicInteger(1);
        p("AtomicInteger.cae", ai.compareAndExchange(1, 2));
        p("AtomicInteger.weakCas", ai.weakCompareAndSetPlain(2, 3));
        AtomicLong al = new AtomicLong(1);
        p("AtomicLong.cae", al.compareAndExchange(1, 2));
        AtomicReference<String> ar = new AtomicReference<>("a");
        p("AtomicReference.cae", ar.compareAndExchange("a", "b"));
    }

    // ---- nio_file.rs filesystem attribute family (28 rows)

    static void fileAttributes() {
        System.out.println("[fileAttributes]");
        Path f = null;
        try {
            f = Files.createTempFile("deadsweep-attr", ".txt");
            Files.writeString(f, "content");
            p("size", Files.size(f));
            p("isRegular", Files.isRegularFile(f));
            p("lastModified", Files.getLastModifiedTime(f) != null);
            p("readable", Files.isReadable(f));
            p("posix perms", Files.getPosixFilePermissions(f).size() >= 0);
            p("store", Files.getFileStore(f).name() != null);
            p("realPath", f.toRealPath() != null);
            Path link = f.resolveSibling(f.getFileName() + ".link");
            try {
                Files.createSymbolicLink(link, f);
                p("symlink target", Files.readSymbolicLink(link).getFileName());
                Files.deleteIfExists(link);
            } catch (Throwable t) {
                p("symlink", "unavailable: " + t.getClass().getSimpleName());
            }
            p("freeSpace>0", Files.getFileStore(f).getUsableSpace() > 0);
            File io = f.toFile();
            p("File.canWrite", io.canWrite());
            p("File.length", io.length());
            p("File.list of parent", io.getParentFile().list() != null);
        } catch (Throwable t) {
            p("attrs", "threw " + t);
        } finally {
            if (f != null) { try { Files.deleteIfExists(f); } catch (IOException ignored) { } }
        }
    }

    // ---- streams.rs / phases_late/streams.rs

    static void streams() {
        System.out.println("[streams]");
        p("map/filter/collect", new ArrayList<>(List.of(1, 2, 3, 4)).stream()
                .filter(x -> x % 2 == 0).map(x -> x * 3)
                .collect(java.util.stream.Collectors.toList()));
        p("reduce", java.util.stream.IntStream.rangeClosed(1, 5).sum());
        p("flatMap", java.util.stream.Stream.of(List.of(1, 2), List.of(3))
                .flatMap(List::stream).count());
        p("sorted", java.util.stream.Stream.of(3, 1, 2).sorted().toList());
        p("groupingBy", java.util.stream.Stream.of("aa", "b", "cc")
                .collect(java.util.stream.Collectors.groupingBy(String::length)).size());
        p("iterate/limit", java.util.stream.Stream.iterate(1, x -> x + 1).limit(4).toList());
        p("Optional.map", Optional.of("v").map(String::toUpperCase).orElse("-"));
    }

    // ---- deprecated_internal.rs (rmi activation names, 24 rows)

    static void deprecatedNames() {
        System.out.println("[deprecatedNames]");
        for (String n : new String[] {
                "java.rmi.activation.Activatable", "java.rmi.activation.ActivationGroup",
                "java.lang.Compiler", "java.net.PlainSocketImpl",
                "sun.misc.Cleaner", "sun.misc.URLClassPath",
                "sun.reflect.Reflection", "java.lang.UNIXProcess",
                "jdk.internal.misc.SharedSecrets", "sun.nio.fs.PollingWatchService",
                "sun.nio.ch.KQueuePort", "sun.nio.ch.WindowsFileDispatcherImpl",
                "java.net.InetAddressImplFactory", "jdk.internal.logger.AbstractLoggerFinder",
        }) {
            String verdict;
            try {
                Class<?> c = Class.forName(n, false, DeadSweepReachProbe.class.getClassLoader());
                verdict = "LOADED " + c.getName();
            } catch (Throwable t) {
                verdict = t.getClass().getSimpleName();
            }
            p(n, verdict);
        }
    }

    public static void main(String[] args) {
        section("iterators", DeadSweepReachProbe::iterators);
        section("enumerations", DeadSweepReachProbe::enumerations);
        section("combinators", DeadSweepReachProbe::combinators);
        section("fieldUpdaters", DeadSweepReachProbe::fieldUpdaters);
        section("proxies", DeadSweepReachProbe::proxies);
        section("directBuffers", DeadSweepReachProbe::directBuffers);
        section("management", DeadSweepReachProbe::management);
        section("serviceLoader", DeadSweepReachProbe::serviceLoader);
        section("completedFuture", DeadSweepReachProbe::completedFuture);
        section("sharedSecrets", DeadSweepReachProbe::sharedSecrets);
        section("watchService", DeadSweepReachProbe::watchService);
        section("sockets", DeadSweepReachProbe::sockets);
        section("casFamily", DeadSweepReachProbe::casFamily);
        section("fileAttributes", DeadSweepReachProbe::fileAttributes);
        section("streams", DeadSweepReachProbe::streams);
        section("deprecatedNames", DeadSweepReachProbe::deprecatedNames);
        System.out.println("DeadSweepReachProbe sections=" + sections + " failed=" + failed);
    }
}
