// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.regex.Pattern;

/**
 * gci i3/hd: the attach heap walks ({@code jcmd <pid> GC.class_histogram},
 * {@code jcmd <pid> GC.heap_dump}) run inside a pause, print HotSpot 25's
 * text, and survive mutators that allocate and link objects while they run.
 * Page: {@code docs/known-issues/gc/gci-i2oo-jcmd-heap-walks-run-without-a-safepoint-20261012.md}.
 *
 * <p>Modes:
 * <ul>
 * <li>{@code walk <rounds>} (default {@code walk 6}): two daemon threads keep
 *   building and dropping linked {@code Node} chains (a live set of 64 chains
 *   is kept in a static list) while the probe runs {@code jcmd <self>
 *   GC.class_histogram} {@code rounds} times, then {@code GC.heap_dump} to a
 *   fresh temp file, then the same dump again without {@code -overwrite}
 *   (refused: the file exists), then with {@code -overwrite}.</li>
 * <li>{@code spin <secs>}: allocate and link for {@code secs} seconds, then
 *   print {@code spin done}. A run long enough for a
 *   {@code CRATONVM_DBG_HANGWALK} watchdog to fire inside it.</li>
 * </ul>
 *
 * <p>stdout is deterministic (HotSpot 25 {@code -XX:+UseSerialGC}), one
 * {@code <key> <value>} line per fact. The jcmd path ({@code java.home/bin},
 * else {@code $JAVA_HOME/bin}, or {@code -Dgci.jcmd=<path>}), each jcmd's rc,
 * round trip and first lines go to stderr as {@code [gci-hd3] ...}.
 */
public final class GciHd3HeapWalkProbe {
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

    static final List<Node> LIVE = new ArrayList<>();
    static volatile boolean stop;
    static volatile Object sink;

    static final String HEADER = " num     #instances         #bytes  class name (module)";
    static final String RULE = "-------------------------------------------------------";
    static final Pattern ROW = Pattern.compile("^ {0,3}\\d+: +(\\d+) +(\\d+)  (.+)$");
    static final Pattern TOTAL = Pattern.compile("^Total +\\d+ +\\d+$");

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

    /** Runs jcmd; returns {rc, output}. */
    static Object[] jcmd(String jcmd, long pid, String... command) throws Exception {
        List<String> cmd = new ArrayList<>();
        cmd.add(jcmd);
        cmd.add(Long.toString(pid));
        for (String c : command) {
            cmd.add(c);
        }
        ProcessBuilder pb = new ProcessBuilder(cmd);
        pb.redirectErrorStream(true);
        long t0 = System.nanoTime();
        Process p = pb.start();
        byte[] out = p.getInputStream().readAllBytes();
        int rc = p.waitFor();
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        String text = new String(out, StandardCharsets.UTF_8);
        String head = text.length() > 300 ? text.substring(0, 300) : text;
        System.err.println("[gci-hd3] " + String.join(" ", command) + " rc=" + rc + " ms=" + ms
                + " head=" + head.replace('\r', ' ').replace('\n', '|'));
        return new Object[] {rc, text};
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "walk";
        for (int i = 0; i < 64; i++) {
            LIVE.add(chain(i * 1000L, 32));
        }
        if (mode.equals("spin")) {
            int secs = args.length > 1 ? Integer.parseInt(args[1]) : 15;
            startChurner("gci-hd3-churn-a", 1);
            long end = System.nanoTime() + secs * 1_000_000_000L;
            long i = 7;
            while (System.nanoTime() < end) {
                sink = chain(i++, 16);
            }
            stop = true;
            System.out.println("spin done");
            return;
        }
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 6;
        startChurner("gci-hd3-churn-a", 1);
        startChurner("gci-hd3-churn-b", 1L << 40);
        String jcmd = jcmdPath();
        if (!Files.isRegularFile(Path.of(jcmd))) {
            System.err.println("[gci-hd3] no jcmd at " + jcmd + " (VOID: set -Dgci.jcmd or JAVA_HOME)");
        }
        long pid = ProcessHandle.current().pid();

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
                java.util.regex.Matcher m = ROW.matcher(line);
                if (m.matches()) {
                    long b = Long.parseLong(m.group(2));
                    if (b > prev) {
                        order = false;
                    }
                    prev = b;
                    String name = m.group(3);
                    if (name.equals("GciHd3HeapWalkProbe$Node")) {
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

        Path dump = Path.of(System.getProperty("java.io.tmpdir"), "gci-hd3-" + pid + ".hprof")
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
        System.err.println("[gci-hd3] dump size " + size1 + " -> " + size2 + " path " + dump);
        Files.deleteIfExists(dump);
        synchronized (LIVE) {
            System.err.println("[gci-hd3] live chains " + LIVE.size());
        }
    }
}
