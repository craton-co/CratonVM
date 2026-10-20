// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.BufferedInputStream;
import java.io.DataInputStream;
import java.io.EOFException;
import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.HashSet;
import java.util.List;
import java.util.Set;
import java.util.concurrent.TimeUnit;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

/**
 * gci i14/hd: heap diagnostics on G1 against HotSpot 25 G1
 * ({@code docs/known-issues/gc/gci-i14hd-g1-heap-diagnostics-review-findings-20261013.md}).
 *
 * <p>Modes:
 * <ul>
 * <li>{@code heapinfo}: {@code jcmd <self> GC.heap_info}, each line printed in
 *   SHAPE form (hex runs -> {@code H}, digit runs -> {@code N}, space runs ->
 *   one space; jcmd's {@code <pid>:} line, blank lines and the
 *   {@code class space} line dropped: this VM has no compressed class
 *   space). Item 6.</li>
 * <li>{@code humdump}: three 9 MiB {@code byte[]} (humongous at 1 MiB regions,
 *   each larger than the dumper's 8 MiB segment) kept in a static, then
 *   {@code jcmd <self> GC.heap_dump -all <file>}; the file is parsed record by
 *   record: does it parse to its end, how many of the three arrays it holds
 *   with every byte right, and how many instance / object-array records name
 *   a class object no CLASS_DUMP or LOAD_CLASS defines. Items 2 and 5.</li>
 * <li>{@code histo}: the same arrays, {@code jcmd <self> GC.class_histogram -all}:
 *   no {@code class#<id>} row, and the {@code [B} row's bytes cover the
 *   arrays. Items 1, 2 and 4.</li>
 * <li>{@code sparse}: 60000 small objects promoted (20 {@code System.gc()}),
 *   every tenth dropped, then {@code jcmd <self> GC.class_histogram} (live:
 *   a full collection first). HotSpot counts exactly the 54000 kept. Item 7
 *   (a dead object in an Old region the G1 full collection did not compact
 *   is counted here).</li>
 * <li>{@code quiet}: prints {@code PROBE-OK} (the {@code -Xlog:gc+exit} rows).</li>
 * <li>{@code loaders} (second pass, item 10): {@code GC.heap_dump -all}; does
 *   some CLASS_DUMP name a class loader object (the probe's own class is the
 *   application loader's), and does every non-zero loader id name an
 *   INSTANCE record of the dump.</li>
 * <li>{@code young} (second pass, item 9): about 400 MB of short-lived
 *   {@code byte[1024]}, so young pauses happen on their own; prints
 *   {@code PROBE-OK} (the {@code -Xlog:gc+heap=debug} rows).</li>
 * </ul>
 *
 * <p>stdout is deterministic; each jcmd's rc, round trip and first bytes go to
 * stderr as {@code [gci-hd14] ...}. jcmd writes to a file (see
 * {@code GciHd4HeapWalkProbe} for why). jcmd: {@code -Dgci.jcmd=<path>}, else
 * {@code java.home/bin}, else {@code $JAVA_HOME/bin}.
 */
public final class GciHd14G1DiagProbe {
    static final class Sparse14 {
        long a;
        long b;

        Sparse14(long v) {
            this.a = v;
            this.b = ~v;
        }
    }

    static final int HUM_LEN = 9 * 1024 * 1024 + 13;
    static final int HUM_COUNT = 3;
    static final long JCMD_TIMEOUT_S = 120;
    static final Pattern ROW = Pattern.compile("^ {0,3}\\d+: +(\\d+) +(\\d+)  (.+)$");

    static byte[][] HUM;
    static Sparse14[] SPARSE;

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

    /** Runs jcmd with its output in a file; returns {rc, output}. rc -1: timed out. */
    static Object[] jcmd(String jcmd, long pid, String... command) throws Exception {
        List<String> cmd = new ArrayList<>();
        cmd.add(jcmd);
        cmd.add(Long.toString(pid));
        for (String c : command) {
            cmd.add(c);
        }
        Path out = Files.createTempFile("gci-hd14-jcmd-", ".txt");
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
        System.err.println("[gci-hd14] " + String.join(" ", command) + " rc=" + rc + " ms=" + ms
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

    static String shape(String line) {
        return line.replaceAll("0x[0-9a-fA-F]+", "H").replaceAll("[0-9]+", "N").replaceAll(" +", " ");
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "quiet";
        String jcmd = jcmdPath();
        if (!mode.equals("quiet") && !Files.isRegularFile(Path.of(jcmd))) {
            System.err.println("[gci-hd14] no jcmd at " + jcmd + " (VOID: set -Dgci.jcmd or JAVA_HOME)");
        }
        long pid = ProcessHandle.current().pid();
        switch (mode) {
            case "heapinfo":
                heapinfo(jcmd, pid);
                break;
            case "humdump":
                humdump(jcmd, pid);
                break;
            case "histo":
                histo(jcmd, pid);
                break;
            case "sparse":
                sparse(jcmd, pid);
                break;
            case "loaders":
                loaders(jcmd, pid);
                break;
            case "young":
                young();
                break;
            default:
                System.out.println("PROBE-OK");
                break;
        }
    }

    static void heapinfo(String jcmd, long pid) throws Exception {
        Object[] r = jcmd(jcmd, pid, "GC.heap_info");
        System.out.println("heapinfo rc " + r[0]);
        for (String line : ((String) r[1]).split("\\R")) {
            if (line.isBlank() || line.matches("^\\d+:$") || line.contains("class space")) {
                continue;
            }
            System.out.println("heapinfo | " + shape(line));
        }
    }

    static void fillHumongous() {
        HUM = new byte[HUM_COUNT][];
        for (int k = 0; k < HUM_COUNT; k++) {
            byte[] b = new byte[HUM_LEN];
            for (int i = 0; i < b.length; i++) {
                b[i] = (byte) (i * 31 + 7 + k);
            }
            HUM[k] = b;
        }
    }

    static void histo(String jcmd, long pid) throws Exception {
        fillHumongous();
        Object[] r = jcmd(jcmd, pid, "GC.class_histogram", "-all");
        boolean noSynthetic = true;
        long byteBytes = -1;
        for (String line : ((String) r[1]).split("\\R")) {
            Matcher m = ROW.matcher(line);
            if (!m.matches()) {
                continue;
            }
            String name = m.group(3);
            if (name.startsWith("class#") || name.startsWith("[Lclass#")) {
                noSynthetic = false;
                System.err.println("[gci-hd14] synthetic row: " + line);
            }
            if (name.equals("[B") || name.startsWith("[B (")) {
                byteBytes = Long.parseLong(m.group(2));
            }
        }
        System.out.println("histo rc " + r[0]);
        System.out.println("histo no synthetic rows " + noSynthetic);
        System.out.println("histo byte arrays hold the humongous " + (byteBytes >= (long) HUM_COUNT * HUM_LEN));
        System.out.println("kept " + HUM.length + " " + HUM[2][1]);
        System.err.println("[gci-hd14] histo byteBytes=" + byteBytes);
    }

    static void sparse(String jcmd, long pid) throws Exception {
        int n = 60000;
        Sparse14[] a = new Sparse14[n];
        for (int i = 0; i < n; i++) {
            a[i] = new Sparse14(i);
        }
        SPARSE = a;
        for (int i = 0; i < 20; i++) {
            System.gc();
        }
        int dropped = 0;
        for (int i = 0; i < n; i += 10) {
            a[i] = null;
            dropped++;
        }
        int kept = n - dropped;
        String name = "GciHd14G1DiagProbe$Sparse14";
        Object[] r = jcmd(jcmd, pid, "GC.class_histogram");
        long count = rowCount((String) r[1], name);
        System.out.println("sparse histogram rc " + r[0]);
        System.out.println("sparse live histogram exact " + (count == kept));
        System.out.println("sparse kept " + kept + " second " + a[1].a);
        System.err.println("[gci-hd14] sparse count=" + count + " kept=" + kept);
    }

    static volatile Object youngSink;

    static void young() {
        for (int i = 0; i < 400 * 1024; i++) {
            youngSink = new byte[1024];
        }
        System.out.println("PROBE-OK");
    }

    static void loaders(String jcmd, long pid) throws Exception {
        Path dump = Path.of(System.getProperty("java.io.tmpdir"), "gci-hd14l-" + pid + ".hprof").toAbsolutePath();
        Files.deleteIfExists(dump);
        Object[] r = jcmd(jcmd, pid, "GC.heap_dump", "-all", dump.toString());
        Hprof h = new Hprof();
        long size = Files.isRegularFile(dump) ? Files.size(dump) : -1;
        if (size > 0) {
            try (InputStream in = Files.newInputStream(dump)) {
                h.read(in);
            } catch (IOException | RuntimeException e) {
                h.ok = false;
                System.err.println("[gci-hd14] parse failed: " + e);
            }
        }
        Files.deleteIfExists(dump);
        long dangling = 0;
        for (Long id : h.loaderIds) {
            if (!h.instances.contains(id)) {
                dangling++;
            }
        }
        System.out.println("loaders rc " + r[0]);
        System.out.println("loaders parse ok " + h.ok);
        System.out.println("loaders named " + !h.loaderIds.isEmpty());
        System.out.println("loaders dangling " + dangling);
        System.err.println("[gci-hd14] loaders distinct=" + h.loaderIds.size() + " instances=" + h.instances.size());
    }

    static void humdump(String jcmd, long pid) throws Exception {
        fillHumongous();
        Path dump = Path.of(System.getProperty("java.io.tmpdir"), "gci-hd14-" + pid + ".hprof").toAbsolutePath();
        Files.deleteIfExists(dump);
        Object[] r = jcmd(jcmd, pid, "GC.heap_dump", "-all", dump.toString());
        Hprof h = new Hprof();
        long size = Files.isRegularFile(dump) ? Files.size(dump) : -1;
        if (size > 0) {
            try (InputStream in = Files.newInputStream(dump)) {
                h.read(in);
            } catch (IOException | RuntimeException e) {
                h.ok = false;
                System.err.println("[gci-hd14] parse failed: " + e);
            }
        }
        Files.deleteIfExists(dump);
        System.out.println("humdump rc " + r[0]);
        System.out.println("humdump parse ok " + h.ok);
        System.out.println("humdump humongous byte arrays " + h.humArrays);
        System.out.println("humdump content ok " + (h.humArrays > 0 && h.humContentOk == h.humArrays));
        System.out.println("humdump dangling class refs " + h.dangling());
        System.out.println("kept " + HUM.length + " " + HUM[0][5]);
        System.err.println("[gci-hd14] humdump size=" + size + " records=" + h.records
                + " subrecords=" + h.subRecords + " classes=" + h.classes.size());
    }

    /** A minimal HPROF 1.0.2 reader: every top-level record, every heap sub-record. */
    static final class Hprof {
        boolean ok = true;
        int idSize = 8;
        long pos;
        long records;
        long subRecords;
        int humArrays;
        int humContentOk;
        final Set<Long> classes = new HashSet<>();
        final Set<Long> refs = new HashSet<>();
        final Set<Long> loaderIds = new HashSet<>();
        final Set<Long> instances = new HashSet<>();
        DataInputStream in;

        long dangling() {
            long n = 0;
            for (Long id : refs) {
                if (!classes.contains(id)) {
                    n++;
                }
            }
            return n;
        }

        int u1() throws IOException {
            pos += 1;
            return in.readUnsignedByte();
        }

        int u2() throws IOException {
            pos += 2;
            return in.readUnsignedShort();
        }

        long u4() throws IOException {
            pos += 4;
            return in.readInt() & 0xffffffffL;
        }

        long id() throws IOException {
            if (idSize == 8) {
                pos += 8;
                return in.readLong();
            }
            return u4();
        }

        void skip(long n) throws IOException {
            if (n < 0) {
                throw new IOException("negative skip " + n);
            }
            in.skipNBytes(n);
            pos += n;
        }

        int typeSize(int t) throws IOException {
            switch (t) {
                case 2:
                    return idSize;
                case 4:
                case 8:
                    return 1;
                case 5:
                case 9:
                    return 2;
                case 6:
                case 10:
                    return 4;
                case 7:
                case 11:
                    return 8;
                default:
                    throw new IOException("bad basic type " + t + " at " + pos);
            }
        }

        void read(InputStream raw) throws IOException {
            in = new DataInputStream(new BufferedInputStream(raw, 1 << 16));
            StringBuilder magic = new StringBuilder();
            int c;
            while ((c = u1()) != 0) {
                magic.append((char) c);
            }
            if (!magic.toString().equals("JAVA PROFILE 1.0.2")) {
                ok = false;
                return;
            }
            idSize = (int) u4();
            u4();
            u4();
            boolean sawEnd = false;
            while (true) {
                int tag;
                try {
                    tag = in.readUnsignedByte();
                } catch (EOFException e) {
                    break;
                }
                pos += 1;
                u4();
                long len = u4();
                records++;
                long start = pos;
                if (tag == 0x1C || tag == 0x0C) {
                    long end = start + len;
                    while (pos < end) {
                        if (!subRecord()) {
                            ok = false;
                            return;
                        }
                        subRecords++;
                    }
                    if (pos != end) {
                        System.err.println("[gci-hd14] segment overran: pos=" + pos + " end=" + end);
                        ok = false;
                        return;
                    }
                } else if (tag == 0x02) {
                    u4();
                    classes.add(id());
                    skip(len - (pos - start));
                } else {
                    if (tag == 0x2C) {
                        sawEnd = true;
                    }
                    skip(len);
                }
            }
            if (!sawEnd) {
                System.err.println("[gci-hd14] no HEAP_DUMP_END record");
                ok = false;
            }
        }

        boolean subRecord() throws IOException {
            int t = u1();
            switch (t) {
                case 0xFF:
                case 0x05:
                case 0x07:
                    id();
                    return true;
                case 0x01:
                    id();
                    id();
                    return true;
                case 0x02:
                case 0x03:
                case 0x08:
                    id();
                    u4();
                    u4();
                    return true;
                case 0x04:
                case 0x06:
                    id();
                    u4();
                    return true;
                case 0x20:
                    classDump();
                    return true;
                case 0x21: {
                    instances.add(id());
                    u4();
                    refs.add(id());
                    skip(u4());
                    return true;
                }
                case 0x22: {
                    id();
                    u4();
                    long n = u4();
                    refs.add(id());
                    skip(n * idSize);
                    return true;
                }
                case 0x23: {
                    id();
                    u4();
                    long n = u4();
                    int ty = u1();
                    primArray(n, ty);
                    return true;
                }
                default:
                    System.err.println("[gci-hd14] unknown sub-record tag " + t + " at " + pos);
                    return false;
            }
        }

        void classDump() throws IOException {
            long cid = id();
            u4();
            id();
            long loader = id();
            if (loader != 0) {
                loaderIds.add(loader);
            }
            for (int i = 0; i < 4; i++) {
                id();
            }
            u4();
            int cp = u2();
            for (int i = 0; i < cp; i++) {
                u2();
                skip(typeSize(u1()));
            }
            int statics = u2();
            for (int i = 0; i < statics; i++) {
                id();
                skip(typeSize(u1()));
            }
            int fields = u2();
            for (int i = 0; i < fields; i++) {
                id();
                u1();
            }
            classes.add(cid);
        }

        void primArray(long n, int ty) throws IOException {
            long bytes = n * typeSize(ty);
            if (ty != 8 || n != HUM_LEN) {
                skip(bytes);
                return;
            }
            byte[] b = new byte[HUM_LEN];
            in.readFully(b);
            pos += HUM_LEN;
            humArrays++;
            int k = b[0] - 7;
            boolean good = k >= 0 && k < HUM_COUNT;
            for (int i = 0; good && i < b.length; i++) {
                if (b[i] != (byte) (i * 31 + 7 + k)) {
                    good = false;
                }
            }
            if (good) {
                humContentOk++;
            }
        }
    }
}
