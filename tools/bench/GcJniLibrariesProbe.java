// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import com.sun.jna.Callback;
import com.sun.jna.Library;
import com.sun.jna.Memory;
import com.sun.jna.Native;
import com.sun.jna.Platform;
import com.sun.jna.Pointer;
import io.netty.buffer.ByteBufAllocator;
import io.netty.handler.ssl.OpenSsl;
import io.netty.handler.ssl.SslContext;
import io.netty.handler.ssl.SslContextBuilder;
import io.netty.handler.ssl.SslProvider;
import io.netty.handler.ssl.util.InsecureTrustManagerFactory;
import io.netty.util.ReferenceCountUtil;
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.FileInputStream;
import java.io.InputStream;
import java.io.OutputStream;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.security.KeyStore;
import java.util.Arrays;
import java.util.Random;
import java.util.concurrent.atomic.AtomicInteger;
import javax.net.ssl.KeyManagerFactory;
import javax.net.ssl.SSLEngine;
import javax.net.ssl.SSLEngineResult;
import javax.net.ssl.SSLException;
import net.jpountz.lz4.LZ4Compressor;
import net.jpountz.lz4.LZ4Factory;
import net.jpountz.lz4.LZ4FastDecompressor;
import net.jpountz.xxhash.XXHashFactory;

/**
 * gc JNI retirement (2026-10-10): the JNI-library run that
 * {@code common-w2c-jni-local-refs-are-raw-addresses} (step 2) and
 * {@code common-w9g-idle-foreign-threads-run-jni-functions-gc-blocked} (the JNA
 * callback run) waited on before {@code CRATONVM_JNI_INDIRECT_LOCALS} and
 * {@code CRATONVM_JNI_FOREIGN_TRANSITIONS} could default on.
 *
 * <p>Each case runs real third-party JNI code while a churn thread keeps the
 * young generation collecting (and so moving), and checks every result:
 * <ul>
 *   <li>{@code jna-upcall}: libc {@code qsort} with a JNA {@link Callback}
 *       comparator that allocates. The callback runs on the calling Java
 *       thread, inside JNA's native dispatch, so JNA's own locals are held
 *       across allocating up-calls;</li>
 *   <li>{@code jna-native-threads}: threads made by the OS
 *       ({@code CreateThread} / {@code pthread_create}) whose entry point is a
 *       JNA callback taking a {@code String}. JNA attaches each thread, builds
 *       the {@code String} with JNIEnv functions at the attach level (outside
 *       any Java call: the {@code common-w9g} path), calls Java and
 *       detaches;</li>
 *   <li>{@code lz4}: lz4-java's native compressor, decompressor and xxhash,
 *       on heap arrays and direct buffers (critical array access);</li>
 *   <li>{@code zstd}: zstd-jni's one-shot calls and its streams (native
 *       contexts reading and writing Java fields across calls);</li>
 *   <li>{@code tcnative}: netty-tcnative (BoringSSL) TLS handshakes between two
 *       {@code OpenSslEngine}s. The handshake calls back from C into Java for
 *       the key material and the certificate check.</li>
 * </ul>
 *
 * <p>HotSpot prints, in this order, and exits 0:
 * <pre>
 *   jna-upcall: PASS
 *   jna-native-threads: PASS
 *   lz4: PASS
 *   zstd: PASS
 *   tcnative: PASS
 *   PASS all 5
 * </pre>
 * A failing case prints {@code <case>: FAIL <detail>}, the summary
 * {@code FAIL <n> of 5}, and the exit code is 1.
 *
 * <p>Arguments: the PKCS12 key store for {@code tcnative} (password
 * {@code changeit}, alias {@code probe}), then optionally a round count, then
 * optionally a comma list of the cases to run. Make
 * the key store with HotSpot's {@code keytool -genkeypair -keyalg RSA -keysize
 * 2048 -alias probe -dname CN=localhost -storetype PKCS12 -storepass changeit
 * -keystore probe.p12}. Class path: jna 5.14.0, lz4-java 1.11.1, zstd-jni
 * 1.5.7-7, netty 4.2.16 (common, buffer, transport, resolver, codec-base,
 * handler) and netty-tcnative-classes + netty-tcnative-boringssl-static
 * 2.0.81 (the platform jar).
 */
public final class GcJniLibrariesProbe {

    static volatile boolean churning = true;
    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        String keyStore = args[0];
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 1;
        only = args.length > 2 ? java.util.Set.of(args[2].split(",")) : null;
        Thread churn = new Thread(GcJniLibrariesProbe::churn, "churn");
        churn.setDaemon(true);
        churn.start();
        int failed = 0;
        failed += report("jna-upcall", () -> jnaUpcall(rounds));
        failed += report("jna-native-threads", () -> jnaNativeThreads(rounds));
        failed += report("lz4", () -> lz4(rounds));
        failed += report("zstd", () -> zstd(rounds));
        failed += report("tcnative", () -> tcnative(keyStore, rounds));
        churning = false;
        churn.join();
        System.out.println(failed == 0 ? "PASS all 5" : "FAIL " + failed + " of 5");
        System.exit(failed == 0 ? 0 : 1);
    }

    /** Optional third argument: a comma list of the cases to run (the rest PASS unrun). */
    static java.util.Set<String> only;

    interface Case {
        String run() throws Exception;
    }

    static int report(String name, Case c) {
        // Progress on stderr only (stdout is the transcript compared with HotSpot).
        System.err.println("[gc-jni-libs] start " + name);
        String detail;
        try {
            detail = only == null || only.contains(name) ? c.run() : null;
        } catch (Throwable t) {
            detail = t.toString();
        }
        if (detail == null) {
            System.out.println(name + ": PASS");
            return 0;
        }
        System.out.println(name + ": FAIL " + detail);
        return 1;
    }

    /** Keeps a small ring of young garbage alive so every collection moves something. */
    static void churn() {
        Object[] ring = new Object[64];
        int i = 0;
        while (churning) {
            ring[i++ & 63] = new byte[(i & 7) * 1024 + 16];
            if ((i & 255) == 0) {
                sink = new int[4096];
            }
        }
    }

    // ---- JNA ---------------------------------------------------------------

    public interface CLib extends Library {
        void qsort(Pointer base, long count, long size, Comparator cmp);
    }

    public interface Comparator extends Callback {
        int invoke(Pointer a, Pointer b);
    }

    static String jnaUpcall(int rounds) {
        CLib c = Native.load(Platform.isWindows() ? "msvcrt" : "c", CLib.class);
        Random r = new Random(42);
        AtomicInteger calls = new AtomicInteger();
        Comparator cmp = (a, b) -> {
            calls.incrementAndGet();
            sink = new byte[256];
            return Integer.compare(a.getInt(0), b.getInt(0));
        };
        // Sized for Linux Generational, where a JNA upcall under the churn costs
        // about 3 ms (`docs/internal/gc/gc-jni-linux-generational-jna-upcalls-are-10x-slower-under-churn-CLOSED-20261012.md`).
        for (int round = 0; round < 10 * rounds; round++) {
            int n = 1000;
            int[] data = new int[n];
            for (int i = 0; i < n; i++) {
                data[i] = r.nextInt();
            }
            Memory m = new Memory(4L * n);
            m.write(0, data, 0, n);
            c.qsort(m, n, 4, cmp);
            int[] got = m.getIntArray(0, n);
            int[] want = data.clone();
            Arrays.sort(want);
            if (!Arrays.equals(got, want)) {
                return "round " + round + " not sorted";
            }
        }
        return calls.get() > 0 ? null : "the comparator never ran";
    }

    public interface Kernel32 extends Library {
        Pointer CreateThread(Pointer attrs, long stack, WinProc proc, Pointer arg, int flags,
                             Pointer tid);

        int WaitForSingleObject(Pointer handle, int millis);

        boolean CloseHandle(Pointer handle);
    }

    public interface WinProc extends Callback {
        int invoke(String arg);
    }

    public interface PThread extends Library {
        int pthread_create(Pointer thread, Pointer attr, PosixProc proc, Pointer arg);

        int pthread_join(long thread, Pointer ret);

        /** GNU: 0 once the thread has exited (and is joined), EBUSY before. Never blocks. */
        int pthread_tryjoin_np(long thread, Pointer ret);
    }

    public interface PosixProc extends Callback {
        Pointer invoke(String arg);
    }

    static String payload(int i) {
        StringBuilder sb = new StringBuilder("jna-thread-").append(i).append('-');
        for (int k = 0; k < 40 + (i % 50); k++) {
            sb.append((char) ('a' + (i + k) % 26));
        }
        return sb.toString();
    }

    static final AtomicInteger threadsDone = new AtomicInteger();
    static final AtomicInteger threadsBad = new AtomicInteger();
    /** What the first bad argument looked like (stdout only on failure). */
    static volatile String firstBad;

    /** The work every OS thread's callback does: check its argument, allocate, check again. */
    static void onNativeThread(String arg) {
        try {
            int dash = arg.indexOf('-', "jna-thread-".length());
            int i = Integer.parseInt(arg.substring("jna-thread-".length(), dash));
            boolean ok = arg.equals(payload(i));
            for (int k = 0; k < 200; k++) {
                sink = new byte[512 + k];
            }
            boolean after = arg.equals(payload(i));
            if (!ok || !after) {
                if (threadsBad.getAndIncrement() == 0) {
                    firstBad = "arg " + (ok ? "changed after allocating" : "wrong on entry")
                            + " (" + arg.length() + " chars, want " + payload(i).length() + ")";
                }
            }
        } catch (Throwable t) {
            if (threadsBad.getAndIncrement() == 0) {
                firstBad = t.toString();
            }
        } finally {
            threadsDone.incrementAndGet();
        }
    }

    static String jnaNativeThreads(int rounds) throws InterruptedException {
        int total = 64 * rounds;
        int batch = 4;
        threadsDone.set(0);
        threadsBad.set(0);
        firstBad = null;
        boolean win = Platform.isWindows();
        Kernel32 k32 = win ? Native.load("kernel32", Kernel32.class) : null;
        PThread pt = win ? null : Native.load("c", PThread.class);
        WinProc winProc = arg -> {
            onNativeThread(arg);
            return 0;
        };
        PosixProc posixProc = arg -> {
            onNativeThread(arg);
            return null;
        };
        for (int started = 0; started < total; started += batch) {
            Object[] handles = new Object[batch];
            Memory[] args = new Memory[batch];
            for (int j = 0; j < batch; j++) {
                byte[] s = payload(started + j).getBytes(StandardCharsets.US_ASCII);
                args[j] = new Memory(s.length + 1);
                args[j].write(0, s, 0, s.length);
                args[j].setByte(s.length, (byte) 0);
                if (win) {
                    handles[j] = k32.CreateThread(null, 0, winProc, args[j], 0, null);
                } else {
                    Memory tid = new Memory(8);
                    if (pt.pthread_create(tid, null, posixProc, args[j]) != 0) {
                        return "pthread_create failed";
                    }
                    handles[j] = tid.getLong(0);
                }
            }
            // Never block inside a native while an attached thread may still need
            // a pause: a Java thread inside a JNI native is a counted mutator in
            // CratonVM (docs/internal/proposals/gc/gcd-d2i-jni-native-methods-are-counted-mutators-CLOSED-USER-DECISION-20261012.md),
            // and the OS thread's DetachCurrentThread can wait for a pause in
            // progress. So wait in Java, and join with a non-blocking poll.
            long deadline = System.nanoTime() + 120_000_000_000L;
            while (threadsDone.get() < started + batch) {
                if (System.nanoTime() > deadline) {
                    return "callbacks done " + threadsDone.get() + " of " + (started + batch);
                }
                Thread.sleep(1);
            }
            for (int j = 0; j < batch; j++) {
                while (win ? k32.WaitForSingleObject((Pointer) handles[j], 0) != 0
                        : pt.pthread_tryjoin_np((Long) handles[j], null) != 0) {
                    if (System.nanoTime() > deadline) {
                        return "thread " + (started + j) + " did not exit";
                    }
                    Thread.sleep(1);
                }
                if (win) {
                    k32.CloseHandle((Pointer) handles[j]);
                }
            }
            // The argument strings live in `Memory` blocks a collector may free
            // (JNA's cleaner) as soon as nothing reads `args` again -- before the
            // OS threads have read them. Keep them until every thread is joined.
            java.lang.ref.Reference.reachabilityFence(args);
        }
        return threadsBad.get() == 0 ? null
                : threadsBad.get() + " bad arguments of " + total + ", first: " + firstBad;
    }

    // ---- lz4-java ----------------------------------------------------------

    static byte[] pattern(Random r, int n) {
        byte[] b = new byte[n];
        for (int i = 0; i < n; i++) {
            b[i] = (byte) ((i % 97) < 60 ? 'a' + (i % 13) : r.nextInt());
        }
        return b;
    }

    static String lz4(int rounds) {
        LZ4Factory f = LZ4Factory.nativeInstance();
        LZ4Compressor c = f.fastCompressor();
        LZ4Compressor hc = f.highCompressor();
        LZ4FastDecompressor d = f.fastDecompressor();
        Random r = new Random(7);
        for (int round = 0; round < 300 * rounds; round++) {
            byte[] src = pattern(r, 16 * 1024 + round);
            byte[] comp = ((round & 1) == 0 ? c : hc).compress(src);
            if (!Arrays.equals(d.decompress(comp, src.length), src)) {
                return "heap round trip " + round;
            }
            ByteBuffer in = ByteBuffer.allocateDirect(src.length);
            in.put(src).flip();
            ByteBuffer out = ByteBuffer.allocateDirect(c.maxCompressedLength(src.length));
            c.compress(in, out);
            out.flip();
            ByteBuffer back = ByteBuffer.allocateDirect(src.length);
            d.decompress(out, back);
            back.flip();
            byte[] got = new byte[src.length];
            back.get(got);
            if (!Arrays.equals(got, src)) {
                return "direct round trip " + round;
            }
            int h = XXHashFactory.nativeInstance().hash32().hash(src, 0, src.length, 0x9747b28c);
            int s = XXHashFactory.safeInstance().hash32().hash(src, 0, src.length, 0x9747b28c);
            if (h != s) {
                return "xxhash32 " + round;
            }
        }
        return null;
    }

    // ---- zstd-jni ----------------------------------------------------------

    static String zstd(int rounds) throws Exception {
        Random r = new Random(11);
        for (int round = 0; round < 200 * rounds; round++) {
            byte[] src = pattern(r, 24 * 1024 + round * 3);
            byte[] comp = com.github.luben.zstd.Zstd.compress(src, 1 + round % 9);
            byte[] back = com.github.luben.zstd.Zstd.decompress(comp, src.length);
            if (!Arrays.equals(back, src)) {
                return "one-shot round trip " + round;
            }
            ByteArrayOutputStream bos = new ByteArrayOutputStream();
            try (OutputStream zo = new com.github.luben.zstd.ZstdOutputStream(bos, 3)) {
                for (int off = 0; off < src.length; off += 1000) {
                    zo.write(src, off, Math.min(1000, src.length - off));
                    sink = new byte[2048];
                }
            }
            ByteArrayOutputStream got = new ByteArrayOutputStream();
            try (InputStream zi = new com.github.luben.zstd.ZstdInputStream(
                    new ByteArrayInputStream(bos.toByteArray()))) {
                byte[] buf = new byte[777];
                int n;
                while ((n = zi.read(buf)) > 0) {
                    got.write(buf, 0, n);
                    sink = new byte[1024];
                }
            }
            if (!Arrays.equals(got.toByteArray(), src)) {
                return "stream round trip " + round;
            }
        }
        return null;
    }

    // ---- netty-tcnative ----------------------------------------------------

    static String tcnative(String keyStore, int rounds) throws Exception {
        if (!OpenSsl.isAvailable()) {
            return "OpenSsl unavailable: " + OpenSsl.unavailabilityCause();
        }
        KeyStore ks = KeyStore.getInstance("PKCS12");
        try (FileInputStream in = new FileInputStream(keyStore)) {
            ks.load(in, "changeit".toCharArray());
        }
        KeyManagerFactory kmf = KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm());
        kmf.init(ks, "changeit".toCharArray());
        SslContext server = SslContextBuilder.forServer(kmf).sslProvider(SslProvider.OPENSSL).build();
        SslContext client = SslContextBuilder.forClient().sslProvider(SslProvider.OPENSSL)
                .trustManager(InsecureTrustManagerFactory.INSTANCE).build();
        for (int round = 0; round < 40 * rounds; round++) {
            SSLEngine se = server.newEngine(ByteBufAllocator.DEFAULT);
            SSLEngine ce = client.newEngine(ByteBufAllocator.DEFAULT, "localhost", 443);
            try {
                String bad = handshakeAndEcho(ce, se, round);
                if (bad != null) {
                    return "round " + round + ": " + bad;
                }
            } finally {
                ReferenceCountUtil.release(se);
                ReferenceCountUtil.release(ce);
            }
        }
        ReferenceCountUtil.release(server);
        ReferenceCountUtil.release(client);
        return null;
    }

    static String handshakeAndEcho(SSLEngine ce, SSLEngine se, int round) throws SSLException {
        int size = 32 * 1024;
        ByteBuffer c2s = ByteBuffer.allocate(size);
        ByteBuffer s2c = ByteBuffer.allocate(size);
        ByteBuffer cApp = ByteBuffer.allocate(size);
        ByteBuffer sApp = ByteBuffer.allocate(size);
        ByteBuffer empty = ByteBuffer.allocate(0);
        ce.beginHandshake();
        se.beginHandshake();
        for (int step = 0; step < 200; step++) {
            boolean cDone = ce.getHandshakeStatus() == SSLEngineResult.HandshakeStatus.NOT_HANDSHAKING;
            boolean sDone = se.getHandshakeStatus() == SSLEngineResult.HandshakeStatus.NOT_HANDSHAKING;
            if (cDone && sDone) {
                break;
            }
            ce.wrap(empty, c2s);
            runTasks(ce);
            se.wrap(empty, s2c);
            runTasks(se);
            c2s.flip();
            se.unwrap(c2s, sApp);
            runTasks(se);
            c2s.compact();
            s2c.flip();
            ce.unwrap(s2c, cApp);
            runTasks(ce);
            s2c.compact();
            sink = new byte[4096];
        }
        if (ce.getHandshakeStatus() != SSLEngineResult.HandshakeStatus.NOT_HANDSHAKING
                || se.getHandshakeStatus() != SSLEngineResult.HandshakeStatus.NOT_HANDSHAKING) {
            return "handshake did not finish";
        }
        byte[] msg = ("hello-" + round + "-" + "x".repeat(round % 300)).getBytes(StandardCharsets.UTF_8);
        c2s.clear();
        ce.wrap(ByteBuffer.wrap(msg), c2s);
        c2s.flip();
        sApp.clear();
        // The server may still owe a session ticket; drain until the record lands.
        for (int i = 0; i < 4 && sApp.position() < msg.length; i++) {
            se.unwrap(c2s, sApp);
        }
        sApp.flip();
        byte[] got = new byte[sApp.remaining()];
        sApp.get(got);
        return Arrays.equals(got, msg) ? null : "echo differs (" + got.length + " bytes)";
    }

    static void runTasks(SSLEngine e) {
        Runnable t;
        while ((t = e.getDelegatedTask()) != null) {
            t.run();
        }
    }
}
