// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3, wave 77, lane L5 -- the per-stream cost of a ZipFile
// entry read (`S`, ~33 us of a ~65 us `jar-read-sized` under --nojit in the
// wave-76 split:
// `docs/internal/proposals/interpreter/i71-orch-proposal-class-path-jar-loading-is-eight-times-hotspot-20261104.md`,
// "Progress (wave 77)"), split into its pieces with every loop body in a
// static method called once per iteration, so that with the JIT on the body
// runs method-compiled (wave 76's lesson: a bench loop in a lambda run once
// per round is interpreted for its first 1000 back edges in EVERY round --
// the OSR trigger counts per activation -- and W75's 800-iteration loops never
// leave the interpreter at all).
//
// Switch it A/Bs (default OFF; the same answers either way):
//   CRATONVM_REFERENCE_LAYOUT_MEMO=1  Reference.clear0() on Reference's own
//       slots resolved once per VM (no by-name walks, no class / static
//       lookups per call). Must lower phantom-clear, weak-clear-queued,
//       weak-clear-unqueued, cleaner-register-clean and the stream rows (one
//       clear0 per close) by the same few hundred ns; weak-new,
//       weak-set-churn and inflater-cache are controls.
//
// Families (one with `L5W77StreamSplitBench <family>`):
//   stream  stream-open-close-last   getInputStream(e) + close(), `e` the
//                                    entry the last getEntry built (the
//                                    lastEntryName shortcut: no lookup):
//                                    the stream objects alone (S).
//           stream-open-close-lookup the same for an entry fetched earlier
//                                    (one CEN lookup more).
//           stream-open-read-close   stream-open-close-last + readNBytes(size).
//   refs    cleaner-register-clean   Cleaner.register(o, action).clean().
//           phantom-clear            new PhantomReference(o, q).clear().
//           weak-clear-queued        new WeakReference(o, q).clear().
//           weak-clear-unqueued      new WeakReference(o).clear() (the
//                                    ThreadLocal.remove shape: NULL_QUEUE).
//           weak-new                 new WeakReference(o, q) (control).
//   colls   weak-set-churn           ZipFile's istreams add + two removes.
//           inflater-cache           ZipFile's cached-Inflater round trip.
//   osr     (for i76-L5-proposal-osr-loops-pay-the-monitor-helper-20261109-RETIRED-20261009.md)
//           churn-act-800    weak-set-churn inline in a loop method run 25
//                            times a round for 800 iterations each (never
//                            reaches the 1000-back-edge OSR floor, and 175
//                            calls stay under the 500-call compile threshold):
//                            the interpreted caller, all the way.
//           churn-act-20000  the same loop run once a round for 20000
//                            iterations (OSR after 1000).
//           churn-method     the 800-iteration activations calling a static
//                            churn method (compiled by invocation count).
//           iface-interp     an interpreted 800-iteration caller doing
//                            `Set.add` / `Set.remove` (invokeinterface).
//           virtual-interp   the same on the WeakHashMap itself
//                            (`put` / `remove`, invokevirtual).
//           static-interp    the same through one static helper per
//                            iteration (invokestatic).
//
// Read: if iface-interp / virtual-interp are several times static-interp
// with the JIT on (and the three are close under --nojit), the interpreter
// does not enter the JDK callees' compiled code from invokeinterface /
// invokevirtual sites (or pays a large toll there); churn-act-800 against
// churn-act-20000 / churn-method prices the per-activation OSR counter.
// Rerun the osr family with CRATONVM_TIER_OSR_BACKEDGE=100: churn-act-800
// should then fall to churn-act-20000's level.
//
// stdout: per row one deterministic "<row> <counts>" line (no "ns/") and one
// timing line "<row> <n> ns/op (median of rounds 1..ROUNDS-1, per iteration)".
// The jar is written by the bench into a temporary directory.
//
// A/B ($B the cratonvm binary, $J the JDK 25 bin directory; interleaved,
// three repetitions, medians):
//   $J/javac -d out L5W77StreamSplitBench.java
//   for JIT in "" --nojit; do for MODE in "" --compatible; do
//     $B --java-home $J/.. $JIT $MODE -cp out L5W77StreamSplitBench
//     env CRATONVM_REFERENCE_LAYOUT_MEMO=1 $B --java-home $J/.. $JIT $MODE -cp out L5W77StreamSplitBench
//   done; done
//   env CRATONVM_TIER_OSR_BACKEDGE=100 $B --java-home $J/.. -cp out L5W77StreamSplitBench osr
//   $J/java -cp out L5W77StreamSplitBench                                         # HotSpot
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// stream-open-close-last opened=10000
// stream-open-close-lookup opened=10000
// stream-open-read-close read=10000 bytes=40610000
// cleaner-register-clean cleaned=20000
// phantom-clear cleared=20000
// weak-clear-queued cleared=20000
// weak-clear-unqueued cleared=20000
// weak-new live=20000
// weak-set-churn added=20000
// weak-set-churn left=0
// inflater-cache polled=20000
// churn-act-800 added=20000 left=0
// churn-act-20000 added=20000 left=0
// churn-method added=20000 left=0
// iface-interp added=20000 left=0
// virtual-interp added=20000 left=0
// static-interp added=20000 left=0
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.File;
import java.io.IOException;
import java.io.InputStream;
import java.lang.ref.Cleaner;
import java.lang.ref.PhantomReference;
import java.lang.ref.ReferenceQueue;
import java.lang.ref.WeakReference;
import java.nio.file.Files;
import java.util.ArrayDeque;
import java.util.Arrays;
import java.util.Collections;
import java.util.Deque;
import java.util.Set;
import java.util.WeakHashMap;
import java.util.zip.Inflater;
import java.util.zip.ZipEntry;
import java.util.zip.ZipFile;
import java.util.zip.ZipOutputStream;

public class L5W77StreamSplitBench {
    static final int ROUNDS = 7;
    static final int STREAM_ITERS = 10_000;
    static final int ITERS = 20_000;
    static final int ACT = 800;
    static final int PAD = 4000;

    interface Body {
        long run(int i) throws Exception;
    }

    /** One row: ROUNDS rounds of `iters` calls of `body`, the first round a warm-up. */
    static void row(String label, String counts, int iters, Body body, Counts after) throws Exception {
        long[] perIter = new long[ROUNDS];
        for (int r = 0; r < ROUNDS; r++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < iters; i++) {
                body.run(i);
            }
            perIter[r] = (System.nanoTime() - t0) / iters;
            if (r == 0) {
                after.reset();
            }
        }
        long[] later = Arrays.copyOfRange(perIter, 1, ROUNDS);
        Arrays.sort(later);
        System.out.println(label + " " + after.render(counts, iters));
        System.out.println(label + " " + later[later.length / 2]
                + " ns/op (median of rounds 1.." + (ROUNDS - 1) + ", per iteration)");
    }

    /** Counters of the LAST round only (deterministic whatever the timing). */
    static final class Counts {
        long a;
        long b;
        void reset() {
            a = 0;
            b = 0;
        }
        String render(String names, int iters) {
            String[] n = names.split(",");
            // Rounds 1..ROUNDS-1 accumulate; report one round's worth.
            long rounds = ROUNDS - 1;
            String s = n[0] + "=" + (a / rounds);
            if (n.length > 1) {
                s += " " + n[1] + "=" + (b / rounds);
            }
            return s;
        }
    }

    /** `public class <name> extends java/lang/Object {}` plus a 4000-char Utf8 (W72's generator). */
    static byte[] classBytes(String name) throws IOException {
        StringBuilder text = new StringBuilder();
        for (int i = 0; i < PAD; i++) {
            text.append((char) ('a' + (i * 7 + name.length()) % 26));
        }
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(49);
        out.writeShort(6);
        out.writeByte(1); out.writeUTF(name);
        out.writeByte(7); out.writeShort(1);
        out.writeByte(1); out.writeUTF("java/lang/Object");
        out.writeByte(7); out.writeShort(3);
        out.writeByte(1); out.writeUTF(text.toString());
        out.writeShort(0x21);
        out.writeShort(2);
        out.writeShort(4);
        out.writeShort(0);
        out.writeShort(0);
        out.writeShort(0);
        out.writeShort(0);
        out.flush();
        return bytes.toByteArray();
    }

    // ---- stream family: bodies ----
    static ZipFile zip;
    static ZipEntry lastEntry;
    static ZipEntry otherEntry;
    static final Counts STREAM = new Counts();

    static long openCloseLast() throws IOException {
        try (InputStream in = zip.getInputStream(lastEntry)) {
            if (in != null) {
                STREAM.a++;
            }
        }
        return 1;
    }

    static long openCloseLookup() throws IOException {
        // `lastEntryName` names the other entry (the last getEntry before
        // the row), so every call looks this one up in the CEN again.
        try (InputStream in = zip.getInputStream(otherEntry)) {
            if (in != null) {
                STREAM.a++;
            }
        }
        return 1;
    }

    static long openReadClose() throws IOException {
        int len = (int) lastEntry.getSize();
        try (InputStream in = zip.getInputStream(lastEntry)) {
            STREAM.b += in.readNBytes(len).length;
            STREAM.a++;
        }
        return 1;
    }

    // ---- refs family: bodies ----
    static final Cleaner CLEANER = Cleaner.create();
    static final ReferenceQueue<Object> QUEUE = new ReferenceQueue<>();
    static final Counts REFS = new Counts();
    static final Runnable ACTION = () -> REFS.a++;

    static long cleanerRegisterClean() {
        Cleaner.Cleanable c = CLEANER.register(new Object(), ACTION);
        c.clean();
        return 1;
    }

    static long phantomClear() {
        Object o = new Object();
        PhantomReference<Object> r = new PhantomReference<>(o, QUEUE);
        r.clear();
        if (!r.refersTo(o)) {
            REFS.a++;
        }
        return 1;
    }

    static long weakClearQueued() {
        WeakReference<Object> r = new WeakReference<>(new Object(), QUEUE);
        r.clear();
        if (r.get() == null) {
            REFS.a++;
        }
        return 1;
    }

    static long weakClearUnqueued() {
        WeakReference<Object> r = new WeakReference<>(new Object());
        r.clear();
        if (r.get() == null) {
            REFS.a++;
        }
        return 1;
    }

    static Object keep;

    static long weakNew() {
        Object o = new Object();
        WeakReference<Object> r = new WeakReference<>(o, QUEUE);
        if (r.refersTo(o)) {
            REFS.a++;
        }
        keep = o;
        return 1;
    }

    // ---- colls / osr family: bodies ----
    static final Counts COLLS = new Counts();
    static Set<Object> streams;
    static WeakHashMap<Object, Boolean> streamMap;
    static Deque<Inflater> cache;

    static long churnOnce() {
        Object stream = new Object();
        synchronized (streams) {
            if (streams.add(stream)) {
                COLLS.a++;
            }
        }
        synchronized (streams) {
            streams.remove(stream);
        }
        synchronized (streams) {
            streams.remove(stream);
        }
        return 1;
    }

    static long inflaterCacheOnce() {
        Inflater inf;
        synchronized (cache) {
            inf = cache.poll();
        }
        if (inf == null) {
            inf = new Inflater(true);
        } else {
            COLLS.a++;
        }
        synchronized (cache) {
            inf.reset();
            cache.add(inf);
        }
        return 1;
    }

    /** weak-set-churn inline, `n` iterations in one activation. */
    static void churnLoop(int n) {
        Set<Object> s = streams;
        for (int i = 0; i < n; i++) {
            Object stream = new Object();
            synchronized (s) {
                if (s.add(stream)) {
                    COLLS.a++;
                }
            }
            synchronized (s) {
                s.remove(stream);
            }
            synchronized (s) {
                s.remove(stream);
            }
        }
    }

    static void churnMethodLoop(int n) {
        for (int i = 0; i < n; i++) {
            churnOnce();
        }
    }

    static void ifaceLoop(int n) {
        Set<Object> s = streams;
        for (int i = 0; i < n; i++) {
            Object stream = new Object();
            if (s.add(stream)) {
                COLLS.a++;
            }
            s.remove(stream);
            s.remove(stream);
        }
    }

    static void virtualLoop(int n) {
        WeakHashMap<Object, Boolean> m = streamMap;
        for (int i = 0; i < n; i++) {
            Object stream = new Object();
            if (m.put(stream, Boolean.TRUE) == null) {
                COLLS.a++;
            }
            m.remove(stream);
            m.remove(stream);
        }
    }

    static void staticOnce(Set<Object> s) {
        Object stream = new Object();
        if (s.add(stream)) {
            COLLS.a++;
        }
        s.remove(stream);
        s.remove(stream);
    }

    static void staticLoop(int n) {
        Set<Object> s = streams;
        for (int i = 0; i < n; i++) {
            staticOnce(s);
        }
    }

    interface Activation {
        void run(int n);
    }

    /** ITERS iterations a round as ITERS / act activations of `act` iterations. */
    static void osrRow(String label, int act, Activation a) {
        streams = Collections.newSetFromMap(new WeakHashMap<>());
        streamMap = new WeakHashMap<>();
        long[] perIter = new long[ROUNDS];
        for (int r = 0; r < ROUNDS; r++) {
            if (r == 1) {
                COLLS.reset();
            }
            long t0 = System.nanoTime();
            for (int k = 0; k < ITERS / act; k++) {
                a.run(act);
            }
            perIter[r] = (System.nanoTime() - t0) / ITERS;
        }
        long[] later = Arrays.copyOfRange(perIter, 1, ROUNDS);
        Arrays.sort(later);
        int left = streams.size() + streamMap.size();
        System.out.println(label + " added=" + (COLLS.a / (ROUNDS - 1)) + " left=" + left);
        System.out.println(label + " " + later[later.length / 2]
                + " ns/op (median of rounds 1.." + (ROUNDS - 1) + ", per iteration)");
    }

    static boolean wanted(String only, String family) {
        return only == null || only.equals(family);
    }

    public static void main(String[] args) throws Exception {
        String only = args.length > 0 ? args[0] : null;
        File dir = Files.createTempDirectory("l5w77s").toFile();
        File jar = new File(dir, "l5w77s.jar");
        try {
            try (ZipOutputStream out = new ZipOutputStream(Files.newOutputStream(jar.toPath()))) {
                for (int i = 0; i < 50; i++) {
                    String internal = "s77/C" + i;
                    out.putNextEntry(new ZipEntry(internal + ".class"));
                    out.write(classBytes(internal));
                    out.closeEntry();
                }
            }
            if (wanted(only, "stream")) {
                zip = new ZipFile(jar);
                try {
                    otherEntry = zip.getEntry("s77/C7.class");
                    lastEntry = zip.getEntry("s77/C3.class");
                    row("stream-open-close-last", "opened", STREAM_ITERS, i -> openCloseLast(), STREAM);
                    row("stream-open-close-lookup", "opened", STREAM_ITERS, i -> openCloseLookup(), STREAM);
                    // `lastEntryName` may have moved: rebuild the shortcut.
                    lastEntry = zip.getEntry("s77/C3.class");
                    row("stream-open-read-close", "read,bytes", STREAM_ITERS, i -> openReadClose(), STREAM);
                } finally {
                    zip.close();
                }
            }
            if (wanted(only, "refs")) {
                row("cleaner-register-clean", "cleaned", ITERS, i -> cleanerRegisterClean(), REFS);
                row("phantom-clear", "cleared", ITERS, i -> phantomClear(), REFS);
                row("weak-clear-queued", "cleared", ITERS, i -> weakClearQueued(), REFS);
                row("weak-clear-unqueued", "cleared", ITERS, i -> weakClearUnqueued(), REFS);
                row("weak-new", "live", ITERS, i -> weakNew(), REFS);
            }
            if (wanted(only, "colls")) {
                streams = Collections.newSetFromMap(new WeakHashMap<>());
                row("weak-set-churn", "added", ITERS, i -> churnOnce(), COLLS);
                System.out.println("weak-set-churn left=" + streams.size());
                cache = new ArrayDeque<>();
                cache.add(new Inflater(true));
                row("inflater-cache", "polled", ITERS, i -> inflaterCacheOnce(), COLLS);
                Inflater inf;
                while ((inf = cache.poll()) != null) {
                    inf.end();
                }
            }
            if (wanted(only, "osr")) {
                osrRow("churn-act-800", ACT, L5W77StreamSplitBench::churnLoop);
                osrRow("churn-act-20000", ITERS, L5W77StreamSplitBench::churnLoop);
                osrRow("churn-method", ACT, L5W77StreamSplitBench::churnMethodLoop);
                osrRow("iface-interp", ACT, L5W77StreamSplitBench::ifaceLoop);
                osrRow("virtual-interp", ACT, L5W77StreamSplitBench::virtualLoop);
                osrRow("static-interp", ACT, L5W77StreamSplitBench::staticLoop);
            }
        } finally {
            jar.delete();
            dir.delete();
        }
    }
}
