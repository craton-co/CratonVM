// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.IOException;
import java.io.OutputStream;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.TimeUnit;
import java.util.regex.Matcher;
import java.util.regex.Pattern;
import java.util.zip.GZIPInputStream;

/**
 * gci i4/hd: {@code GciHd3HeapWalkProbe} with a HotSpot reference that does
 * not hang, plus the live-only walk.
 *
 * <p>Why a new probe. HotSpot 25 on Windows streams a dcmd's output into the
 * attach pipe WHILE the command runs: {@code GC.class_histogram}'s
 * {@code VM_GC_HeapInspection} writes its rows from the VM thread, at the
 * safepoint. i3's probe read jcmd's stdout through a pipe from a Java thread
 * of the target itself; that thread is stopped by the very safepoint, so the
 * pipe to the probe filled, jcmd stopped reading the attach pipe, the VM
 * thread blocked writing into it, and nothing could move (vi3-hd A: 43 KB
 * received, then {@code [attach] pipe write error (232)} when the row's
 * timeout killed jcmd). Here jcmd writes to a FILE (nothing in the target has
 * to run for jcmd to make progress); the probe waits for jcmd in the native
 * {@code waitFor} and reads the file afterwards. A new class name also keeps
 * the battery's HotSpot cache (keyed on flags and class arguments) from
 * replaying the hung run.
 *
 * <p>Modes:
 * <ul>
 * <li>{@code walk <rounds>}: i3's {@code walk} (histogram rounds under two
 *   allocating, linking threads; then a dump, the same dump refused without
 *   {@code -overwrite}, then with it). The same 13 stdout lines.</li>
 * <li>{@code live}: 5000 {@code Held4} kept, 20000 more promoted by two
 *   {@code System.gc()} and then dropped, one allocating thread running (a
 *   collection requested by an attach thread runs on an allocating
 *   mutator). {@code GC.class_histogram} (HotSpot: live objects only, a full
 *   collection first) must count exactly 5000; {@code GC.class_histogram -all}
 *   at least 5000. Page
 *   {@code gci-i3hd-jcmd-heap-walks-count-dead-objects-20261012}.</li>
 * </ul>
 *
 * <p>stdout is deterministic (HotSpot 25 {@code -XX:+UseSerialGC}); each
 * jcmd's rc, round trip and first bytes go to stderr as {@code [gci-hd4] ...}.
 * jcmd: {@code -Dgci.jcmd=<path>}, else {@code java.home/bin}, else
 * {@code $JAVA_HOME/bin}.
 */
public final class GciHd4HeapWalkProbe {
    static final class Node {
        Node next;
        long v;
        byte[] payload;

        Node(Node next, long v) {
            this.next = next;
            this.v = v;
            this.payload = new byte[16 + (int) (v & 63)];
        }
    }

    static final class Held4 {
        final long v;

        Held4(long v) {
            this.v = v;
        }
    }

    static final List<Node> LIVE = new ArrayList<>();
    static volatile boolean stop;
    static volatile Object sink;

    static final String HEADER = " num     #instances         #bytes  class name (module)";
    static final String RULE = "-------------------------------------------------------";
    static final Pattern ROW = Pattern.compile("^ {0,3}\\d+: +(\\d+) +(\\d+)  (.+)$");
    static final Pattern TOTAL = Pattern.compile("^Total +\\d+ +\\d+$");
    static final long JCMD_TIMEOUT_S = 120;

    static Node chain(long seed, int len) {
        Node n = null;
        for (int i = 0; i < len; i++) {
            n = new Node(n, seed + i);
        }
        return n;
    }

    static void churn(long seed) {
        long i = seed;
        while (!stop) {
            Node c = chain(i, 32);
            synchronized (LIVE) {
                LIVE.set((int) (i & 63), c);
            }
            sink = chain(i * 31, 8);
            i++;
        }
    }

    static Thread startChurner(String name, long seed) {
        Thread t = new Thread(() -> churn(seed), name);
        t.setDaemon(true);
        t.start();
        return t;
    }

    static String jcmdPath() {
        String exe = System.getProperty("os.name").toLowerCase().contains("win") ? "jcmd.exe" : "jcmd";
        String jcmd = System.getProperty("gci.jcmd");
        if (jcmd != null) {
            return jcmd;
        }
        Path home = Path.of(System.getProperty("java.home"), "bin", exe);
        String envHome = System.getenv("JAVA_HOME");
        if (!Files.isRegularFile(home) && envHome != null) {
            home = Path.of(envHome, "bin", exe);
        }
        return home.toString();
    }

    /**
     * Runs jcmd with its output in a file; returns {rc, output}. rc -1: jcmd
     * did not finish within {@link #JCMD_TIMEOUT_S} and was killed.
     */
    static Object[] jcmd(String jcmd, long pid, String... command) throws Exception {
        List<String> cmd = new ArrayList<>();
        cmd.add(jcmd);
        cmd.add(Long.toString(pid));
        for (String c : command) {
            cmd.add(c);
        }
        Path out = Files.createTempFile("gci-hd4-jcmd-", ".txt");
        ProcessBuilder pb = new ProcessBuilder(cmd);
        pb.redirectErrorStream(true);
        pb.redirectOutput(ProcessBuilder.Redirect.to(out.toFile()));
        long t0 = System.nanoTime();
        Process p = pb.start();
        p.getOutputStream().close();
        int rc;
        if (p.waitFor(JCMD_TIMEOUT_S, TimeUnit.SECONDS)) {
            rc = p.exitValue();
        } else {
            p.destroyForcibly();
            p.waitFor();
            rc = -1;
        }
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        String text = new String(Files.readAllBytes(out), StandardCharsets.UTF_8);
        Files.deleteIfExists(out);
        String head = text.length() > 300 ? text.substring(0, 300) : text;
        System.err.println("[gci-hd4] " + String.join(" ", command) + " rc=" + rc + " ms=" + ms
                + " head=" + head.replace('\r', ' ').replace('\n', '|'));
        return new Object[] {rc, text};
    }

    /** The instance count of the histogram row named exactly {@code name}; -1 if none. */
    static long rowCount(String text, String name) {
        for (String line : text.split("\\R")) {
            Matcher m = ROW.matcher(line);
            if (m.matches() && m.group(3).equals(name)) {
                return Long.parseLong(m.group(1));
            }
        }
        return -1;
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "walk";
        String jcmd = jcmdPath();
        if (!Files.isRegularFile(Path.of(jcmd))) {
            System.err.println("[gci-hd4] no jcmd at " + jcmd + " (VOID: set -Dgci.jcmd or JAVA_HOME)");
        }
        long pid = ProcessHandle.current().pid();
        if (mode.equals("live")) {
            live(jcmd, pid);
            return;
        }
        if (mode.equals("quiet")) {
            quiet(jcmd, pid);
            return;
        }
        if (mode.equals("gz")) {
            gz(jcmd, pid);
            return;
        }
        walk(jcmd, pid, args.length > 1 ? Integer.parseInt(args[1]) : 6);
    }

    static Held4[] fill(int n, long sign) {
        Held4[] a = new Held4[n];
        for (int i = 0; i < n; i++) {
            a[i] = new Held4(sign * i);
        }
        return a;
    }

    /** Both arrays live in statics, filled by a method that has returned. */
    static Held4[] KEPT;
    static Held4[] DROPPED;

    static void live(String jcmd, long pid) throws Exception {
        KEPT = fill(5000, 1);
        DROPPED = fill(20000, -1);
        System.gc();
        System.gc();
        DROPPED = null;
        Held4[] kept = KEPT;
        for (int i = 0; i < 64; i++) {
            LIVE.add(chain(i * 1000L, 32));
        }
        startChurner("gci-hd4-churn", 1);
        String name = "GciHd4HeapWalkProbe$Held4";
        Object[] live = jcmd(jcmd, pid, "GC.class_histogram");
        long liveCount = rowCount((String) live[1], name);
        Object[] all = jcmd(jcmd, pid, "GC.class_histogram", "-all");
        long allCount = rowCount((String) all[1], name);
        stop = true;
        System.out.println("live histogram rc " + live[0]);
        System.out.println("live histogram held exact " + (liveCount == kept.length));
        System.out.println("all histogram rc " + all[0]);
        System.out.println("all histogram held at least live " + (allCount >= kept.length));
        System.out.println("kept " + kept.length + " first " + kept[0].v + " last " + kept[kept.length - 1].v);
        System.err.println("[gci-hd4] held4 live=" + liveCount + " all=" + allCount);
    }

    /**
     * gci i5/hd: allocates ~16 KB per 10 ms tick and keeps nothing: never
     * close to filling eden (or promoting anything) in the few seconds the
     * walks take, but an allocating mutator that takes a pending
     * {@code jcmd GC.run}-style request at its next allocation (interpreted)
     * or TLAB refill (compiled), which the live walk's collection needs.
     */
    static void trickle() {
        while (!stop) {
            for (int i = 0; i < 16; i++) {
                sink = new byte[1024];
            }
            try {
                Thread.sleep(10);
            } catch (InterruptedException e) {
                return;
            }
        }
    }

    /** Collections so far, over every GC bean (diagnostic, stderr only). */
    static long gcCount() {
        long n = 0;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            long c = gc.getCollectionCount();
            if (c > 0) {
                n += c;
            }
        }
        return n;
    }

    /**
     * gci i5/hd: {@code live} without its confound. vi4-hd L0 was CONFOUNDED:
     * {@code live}'s churner promotes its chains, old fills, and a major
     * collection freed the dropped objects before the default arm's walk in 2
     * of 3 runs. Here nothing runs between the drop and the walks but the
     * {@link #trickle} thread, which promotes nothing and cannot fill eden in
     * time: the default arm (no collection first) must count the 20000
     * dropped objects, the switch arm (and HotSpot) must not. stderr names
     * the collections between the drop and the end of the live walk
     * ({@code gcs_in_window}): 0 on a valid default row, >= 1 on a switch row.
     */
    static void quiet(String jcmd, long pid) throws Exception {
        gcCount(); // loads the management classes before the window
        KEPT = fill(5000, 1);
        DROPPED = fill(20000, -1);
        System.gc();
        System.gc();
        DROPPED = null;
        Held4[] kept = KEPT;
        Thread t = new Thread(() -> trickle(), "gci-hd5-trickle");
        t.setDaemon(true);
        t.start();
        long g0 = gcCount();
        String name = "GciHd4HeapWalkProbe$Held4";
        Object[] live = jcmd(jcmd, pid, "GC.class_histogram");
        long g1 = gcCount();
        long liveCount = rowCount((String) live[1], name);
        Object[] all = jcmd(jcmd, pid, "GC.class_histogram", "-all");
        long allCount = rowCount((String) all[1], name);
        stop = true;
        System.out.println("quiet live histogram rc " + live[0]);
        System.out.println("quiet live histogram held exact " + (liveCount == kept.length));
        System.out.println("quiet all histogram rc " + all[0]);
        System.out.println("quiet all histogram held at least live " + (allCount >= kept.length));
        System.out.println("kept " + kept.length + " first " + kept[0].v + " last " + kept[kept.length - 1].v);
        System.err.println("[gci-hd5] quiet held4 live=" + liveCount + " all=" + allCount
                + " gcs_in_window=" + (g1 - g0));
    }

    /**
     * gci i5/hd ({@code gci-i3hd-heap-walks-and-diagnostics-review-findings}
     * item 10): {@code GC.heap_dump -gz=1} writes a gzip file that
     * {@code GZIPInputStream} reads to its end (CRC and size checked) and
     * that holds an HPROF dump; {@code -gz=10} is refused with HotSpot's line
     * and creates no file.
     */
    static void gz(String jcmd, long pid) throws Exception {
        Path dump = Path.of(System.getProperty("java.io.tmpdir"), "gci-hd5-" + pid + ".hprof.gz")
                .toAbsolutePath();
        Files.deleteIfExists(dump);
        Object[] d = jcmd(jcmd, pid, "GC.heap_dump", "-gz=1", dump.toString());
        boolean created = false;
        for (String line : ((String) d[1]).split("\\R")) {
            if (line.startsWith("Heap dump file created [")) {
                created = true;
            }
        }
        boolean gzipMagic = false;
        boolean hprof = false;
        boolean readsToEnd = false;
        long size = Files.isRegularFile(dump) ? Files.size(dump) : -1;
        if (size > 2) {
            try (var raw = Files.newInputStream(dump)) {
                byte[] m = raw.readNBytes(2);
                gzipMagic = m.length == 2 && (m[0] & 0xff) == 0x1f && (m[1] & 0xff) == 0x8b;
            }
            try (var in = new GZIPInputStream(Files.newInputStream(dump))) {
                byte[] head = in.readNBytes(18);
                hprof = new String(head, StandardCharsets.US_ASCII).equals("JAVA PROFILE 1.0.2");
                in.transferTo(OutputStream.nullOutputStream());
                readsToEnd = true;
            } catch (IOException e) {
                System.err.println("[gci-hd5] gz read failed: " + e);
            }
        }
        Files.deleteIfExists(dump);
        Object[] bad = jcmd(jcmd, pid, "GC.heap_dump", "-gz=10", dump.toString());
        boolean refused = ((String) bad[1]).contains("Compression level out of range (1-9): 10");
        boolean noFile = !Files.exists(dump);
        Files.deleteIfExists(dump);
        System.out.println("gz dump rc " + d[0]);
        System.out.println("gz dump says created " + created);
        System.out.println("gz dump is gzip " + gzipMagic);
        System.out.println("gz dump holds hprof " + hprof);
        System.out.println("gz dump reads to end " + readsToEnd);
        System.out.println("gz level 10 refused " + refused + " no file " + noFile);
        System.err.println("[gci-hd5] gz size " + size + " path " + dump);
    }

    static void walk(String jcmd, long pid, int rounds) throws Exception {
        for (int i = 0; i < 64; i++) {
            LIVE.add(chain(i * 1000L, 32));
        }
        startChurner("gci-hd4-churn-a", 1);
        startChurner("gci-hd4-churn-b", 1L << 40);

        int rc0 = 0;
        int headerOk = 0;
        int totalOk = 0;
        int nodeRow = 0;
        int byteRow = 0;
        int sorted = 0;
        for (int r = 0; r < rounds; r++) {
            Object[] res = jcmd(jcmd, pid, "GC.class_histogram");
            if ((Integer) res[0] == 0) {
                rc0++;
            }
            String[] lines = ((String) res[1]).split("\\R");
            boolean header = false;
            boolean total = false;
            boolean node = false;
            boolean bytes = false;
            boolean order = true;
            long prev = Long.MAX_VALUE;
            for (int i = 0; i < lines.length; i++) {
                String line = lines[i];
                if (line.equals(HEADER) && i + 1 < lines.length && lines[i + 1].equals(RULE)) {
                    header = true;
                }
                if (TOTAL.matcher(line).matches() && line.length() == 6 + 13 + 2 + 13) {
                    total = true;
                }
                Matcher m = ROW.matcher(line);
                if (m.matches()) {
                    long b = Long.parseLong(m.group(2));
                    if (b > prev) {
                        order = false;
                    }
                    prev = b;
                    String name = m.group(3);
                    if (name.equals("GciHd4HeapWalkProbe$Node")) {
                        node = true;
                    }
                    if (name.startsWith("[B (java.base")) {
                        bytes = true;
                    }
                }
            }
            headerOk += header ? 1 : 0;
            totalOk += total ? 1 : 0;
            nodeRow += node ? 1 : 0;
            byteRow += bytes ? 1 : 0;
            sorted += order ? 1 : 0;
        }
        System.out.println("histogram rounds " + rounds);
        System.out.println("histogram rc0 " + rc0);
        System.out.println("histogram header " + headerOk);
        System.out.println("histogram total line " + totalOk);
        System.out.println("histogram node row " + nodeRow);
        System.out.println("histogram byte array row " + byteRow);
        System.out.println("histogram sorted " + sorted);

        Path dump = Path.of(System.getProperty("java.io.tmpdir"), "gci-hd4-" + pid + ".hprof")
                .toAbsolutePath();
        Files.deleteIfExists(dump);
        Object[] d1 = jcmd(jcmd, pid, "GC.heap_dump", dump.toString());
        String t1 = (String) d1[1];
        boolean created = false;
        boolean dumping = false;
        for (String line : t1.split("\\R")) {
            if (line.startsWith("Heap dump file created [") && line.endsWith(" secs]")) {
                created = true;
            }
            if (line.equals("Dumping heap to " + dump + " ...")) {
                dumping = true;
            }
        }
        boolean magic = false;
        long size1 = Files.isRegularFile(dump) ? Files.size(dump) : -1;
        if (size1 > 18) {
            byte[] head = new byte[18];
            try (var in = Files.newInputStream(dump)) {
                magic = in.readNBytes(head, 0, 18) == 18
                        && new String(head, StandardCharsets.US_ASCII).equals("JAVA PROFILE 1.0.2");
            }
        }
        Object[] d2 = jcmd(jcmd, pid, "GC.heap_dump", dump.toString());
        boolean refused = ((String) d2[1]).contains("File exists");
        long size2 = Files.isRegularFile(dump) ? Files.size(dump) : -2;
        Object[] d3 = jcmd(jcmd, pid, "GC.heap_dump", "-overwrite", dump.toString());
        boolean overwritten = false;
        for (String line : ((String) d3[1]).split("\\R")) {
            if (line.startsWith("Heap dump file created [")) {
                overwritten = true;
            }
        }
        stop = true;
        System.out.println("dump rc " + d1[0]);
        System.out.println("dump says dumping " + dumping);
        System.out.println("dump says created " + created);
        System.out.println("dump file is hprof " + magic);
        System.out.println("dump again refused " + refused + " untouched " + (size2 == size1));
        System.out.println("dump overwrite created " + overwritten);
        System.err.println("[gci-hd4] dump size " + size1 + " -> " + size2 + " path " + dump);
        Files.deleteIfExists(dump);
        synchronized (LIVE) {
            System.err.println("[gci-hd4] live chains " + LIVE.size());
        }
    }
}
