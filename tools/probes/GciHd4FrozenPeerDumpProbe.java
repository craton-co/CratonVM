// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.BufferedInputStream;
import java.io.DataInputStream;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * gci i4/hd: a heap dump roots what only a spinning thread's frames hold.
 * Page {@code gci-i2jn-heap-dump-omits-frozen-compiled-peers-roots-20261012}
 * (the MANUAL M-FP block of {@code vi3-hd.list}, as a battery row).
 *
 * <p>Two daemon threads spin in a loop that, once compiled, polls nowhere
 * under {@code CRATONVM_JIT_SAFEPOINT_POLLS=0}, so a pause's take-over
 * freezes them:
 * <ul>
 * <li>{@code gci-hd4-interp-holder}: an INTERPRETED caller (called once)
 *   holds a {@code HeldI} in a local across its call of the loop;</li>
 * <li>{@code gci-hd4-compiled-holder}: the loop's own method holds a
 *   {@code HeldC} in a local used after the loop, so after OSR only the
 *   compiled frame (a register or a spill slot) holds it.</li>
 * </ul>
 * Neither object is reachable from anything else (each escaped once through a
 * static that was nulled at once, so no compiler scalar-replaces it). After a
 * settle, {@code main} takes a heap dump -- {@code jcmd <self> GC.heap_dump}
 * (mode {@code jcmd}, jcmd's output in a file: see {@code GciHd4HeapWalkProbe})
 * or {@code -XX:+HeapDumpOnOutOfMemoryError} (mode {@code oome <dump path>}) --
 * and reads the HPROF itself: each {@code Held*} instance, and whether a GC
 * root record names it.
 *
 * <p>stdout (deterministic against HotSpot 25, which roots both through
 * Java-frame roots): {@code dump ...}, {@code dump parsed true},
 * {@code heldI instances 1}, {@code heldI rooted true},
 * {@code heldC instances 1}, {@code heldC rooted true}. Diagnostics on stderr
 * as {@code [gci-hd4fp] ...}.
 */
public final class GciHd4FrozenPeerDumpProbe {
    static final class HeldI {
        final long v;

        HeldI(long v) {
            this.v = v;
        }
    }

    static final class HeldC {
        final long v;

        HeldC(long v) {
            this.v = v;
        }
    }

    static volatile boolean stop;
    static volatile Object escape;
    static volatile long sinkI;
    static volatile long sinkC;
    static final AtomicInteger STARTED = new AtomicInteger();
    static List<long[]> hog;

    /** Called once, so it stays interpreted: its local {@code h} is a Java-frame root. */
    static long interpCaller(long seed) {
        HeldI h = new HeldI(seed);
        escape = h;
        escape = null;
        long r = spinLoop(seed);
        return r + h.v;
    }

    static long spinLoop(long seed) {
        STARTED.incrementAndGet();
        long x = seed;
        while (!stop) {
            x = x * 6364136223846793005L + 1442695040888963407L;
        }
        return x;
    }

    /** {@code h} is live across the loop: after OSR only the compiled frame holds it. */
    static long compiledHolder(long seed) {
        HeldC h = new HeldC(seed);
        escape = h;
        escape = null;
        STARTED.incrementAndGet();
        long x = seed;
        while (!stop) {
            x = x * 6364136223846793005L + 1442695040888963407L;
        }
        return x + h.v;
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "jcmd";
        Thread ti = new Thread(() -> sinkI = interpCaller(3), "gci-hd4-interp-holder");
        Thread tc = new Thread(() -> sinkC = compiledHolder(5), "gci-hd4-compiled-holder");
        ti.setDaemon(true);
        tc.setDaemon(true);
        ti.start();
        tc.start();
        while (STARTED.get() < 2) {
            Thread.sleep(10);
        }
        // Long enough for both loops to be OSR-compiled.
        Thread.sleep(3000);
        Path dump;
        long pid = ProcessHandle.current().pid();
        if (mode.equals("oome")) {
            // -XX:HeapDumpPath=<dir>: both VMs write <dir>/java_pid<pid>.hprof.
            Path d = Path.of(args[1]).toAbsolutePath();
            dump = Files.isDirectory(d) ? d.resolve("java_pid" + pid + ".hprof") : d;
            exhaust();
            System.out.println("dump oome caught");
        } else {
            dump = Path.of(System.getProperty("java.io.tmpdir"), "gci-hd4fp-" + pid + ".hprof")
                    .toAbsolutePath();
            Files.deleteIfExists(dump);
            int rc = jcmd(jcmdPath(), pid, "GC.heap_dump", "-all", dump.toString());
            System.out.println("dump rc " + rc);
        }
        stop = true;
        ti.join();
        tc.join();
        Parsed p = new Parsed();
        try {
            parse(dump, p);
            System.out.println("dump parsed true");
        } catch (IOException e) {
            System.out.println("dump parsed false");
            System.err.println("[gci-hd4fp] parse failed: " + e);
        }
        System.out.println("heldI instances " + p.heldICount);
        System.out.println("heldI rooted " + (p.heldICount == 1 && p.roots.contains(p.heldIId)));
        System.out.println("heldC instances " + p.heldCCount);
        System.out.println("heldC rooted " + (p.heldCCount == 1 && p.roots.contains(p.heldCId)));
        System.err.println("[gci-hd4fp] roots " + p.roots.size() + " heldI id 0x" + Long.toHexString(p.heldIId)
                + " heldC id 0x" + Long.toHexString(p.heldCId) + " path " + dump
                + " sinks " + (sinkI != 0) + " " + (sinkC != 0));
        Files.deleteIfExists(dump);
    }

    /** Fills the heap until OutOfMemoryError (which writes the dump), then lets it go. */
    static void exhaust() {
        hog = new ArrayList<>();
        try {
            while (true) {
                hog.add(new long[1 << 16]);
            }
        } catch (OutOfMemoryError e) {
            hog = null;
        }
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

    /** jcmd with its output in a file (never a pipe the target must drain); rc -1 on timeout. */
    static int jcmd(String jcmd, long pid, String... command) throws Exception {
        List<String> cmd = new ArrayList<>();
        cmd.add(jcmd);
        cmd.add(Long.toString(pid));
        for (String c : command) {
            cmd.add(c);
        }
        Path out = Files.createTempFile("gci-hd4fp-jcmd-", ".txt");
        ProcessBuilder pb = new ProcessBuilder(cmd);
        pb.redirectErrorStream(true);
        pb.redirectOutput(ProcessBuilder.Redirect.to(out.toFile()));
        long t0 = System.nanoTime();
        Process proc = pb.start();
        proc.getOutputStream().close();
        int rc;
        if (proc.waitFor(120, TimeUnit.SECONDS)) {
            rc = proc.exitValue();
        } else {
            proc.destroyForcibly();
            proc.waitFor();
            rc = -1;
        }
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        String text = new String(Files.readAllBytes(out), StandardCharsets.UTF_8);
        Files.deleteIfExists(out);
        System.err.println("[gci-hd4fp] " + String.join(" ", command) + " rc=" + rc + " ms=" + ms
                + " out=" + text.replace('\r', ' ').replace('\n', '|'));
        return rc;
    }

    // ---- a minimal HPROF 1.0.2 reader ------------------------------------

    static final class Parsed {
        final Set<Long> roots = new HashSet<>();
        long heldIClass = Long.MIN_VALUE;
        long heldCClass = Long.MIN_VALUE;
        long heldIId = Long.MIN_VALUE;
        long heldCId = Long.MIN_VALUE;
        int heldICount;
        int heldCCount;
    }

    static final class In {
        final DataInputStream in;
        long pos;
        int idSize = 8;

        In(Path p) throws IOException {
            in = new DataInputStream(new BufferedInputStream(Files.newInputStream(p), 1 << 16));
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

        long u8() throws IOException {
            pos += 8;
            return in.readLong();
        }

        long id() throws IOException {
            return idSize == 4 ? u4() : u8();
        }

        void skip(long n) throws IOException {
            in.skipNBytes(n);
            pos += n;
        }

        byte[] bytes(int n) throws IOException {
            byte[] b = new byte[n];
            in.readFully(b);
            pos += n;
            return b;
        }

        int valueSize(int type) throws IOException {
            switch (type) {
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
                    throw new IOException("bad basic type " + type + " at " + pos);
            }
        }
    }

    static void parse(Path path, Parsed p) throws IOException {
        Map<Long, String> names = new HashMap<>();
        In h = new In(path);
        try {
            StringBuilder magic = new StringBuilder();
            int c;
            while ((c = h.u1()) != 0) {
                magic.append((char) c);
            }
            if (!magic.toString().startsWith("JAVA PROFILE")) {
                throw new IOException("not an HPROF file: " + magic);
            }
            h.idSize = (int) h.u4();
            h.u8();
            while (true) {
                int tag = h.in.read();
                if (tag < 0) {
                    break;
                }
                h.pos += 1;
                h.u4();
                long len = h.u4();
                if (tag == 0x01) {
                    long sid = h.id();
                    String s = new String(h.bytes((int) (len - h.idSize)), StandardCharsets.UTF_8);
                    if (s.endsWith("$HeldI") || s.endsWith("$HeldC")) {
                        names.put(sid, s);
                    }
                } else if (tag == 0x02) {
                    h.u4();
                    long cid = h.id();
                    h.u4();
                    String n = names.get(h.id());
                    if (n != null && n.endsWith("$HeldI")) {
                        p.heldIClass = cid;
                    } else if (n != null && n.endsWith("$HeldC")) {
                        p.heldCClass = cid;
                    }
                } else if (tag == 0x0C || tag == 0x1C) {
                    heap(h, len, p);
                } else {
                    h.skip(len);
                }
            }
        } finally {
            h.in.close();
        }
    }

    static void heap(In h, long len, Parsed p) throws IOException {
        long end = h.pos + len;
        while (h.pos < end) {
            int t = h.u1();
            switch (t) {
                case 0xFF:
                case 0x05:
                case 0x07:
                    p.roots.add(h.id());
                    break;
                case 0x01:
                    p.roots.add(h.id());
                    h.id();
                    break;
                case 0x02:
                case 0x03:
                case 0x08:
                    p.roots.add(h.id());
                    h.u4();
                    h.u4();
                    break;
                case 0x04:
                case 0x06:
                    p.roots.add(h.id());
                    h.u4();
                    break;
                case 0x20:
                    classDump(h);
                    break;
                case 0x21: {
                    long oid = h.id();
                    h.u4();
                    long cid = h.id();
                    long n = h.u4();
                    h.skip(n);
                    if (cid == p.heldIClass) {
                        p.heldICount++;
                        p.heldIId = oid;
                    } else if (cid == p.heldCClass) {
                        p.heldCCount++;
                        p.heldCId = oid;
                    }
                    break;
                }
                case 0x22: {
                    h.id();
                    h.u4();
                    long n = h.u4();
                    h.id();
                    h.skip(n * h.idSize);
                    break;
                }
                case 0x23: {
                    h.id();
                    h.u4();
                    long n = h.u4();
                    int type = h.u1();
                    h.skip(n * h.valueSize(type));
                    break;
                }
                default:
                    throw new IOException("unknown heap sub-record 0x" + Integer.toHexString(t) + " at " + h.pos);
            }
        }
    }

    static void classDump(In h) throws IOException {
        h.id();
        h.u4();
        for (int i = 0; i < 6; i++) {
            h.id();
        }
        h.u4();
        int cp = h.u2();
        for (int i = 0; i < cp; i++) {
            h.u2();
            h.skip(h.valueSize(h.u1()));
        }
        int statics = h.u2();
        for (int i = 0; i < statics; i++) {
            h.id();
            h.skip(h.valueSize(h.u1()));
        }
        int fields = h.u2();
        for (int i = 0; i < fields; i++) {
            h.id();
            h.u1();
        }
    }
}
