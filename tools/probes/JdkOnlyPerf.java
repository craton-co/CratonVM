// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Ratio probe: run under the default mode and under --jdk-only and compare each row.
// See docs/internal/jdk-only/heap-bytebuffer-and-chm-run-interpreted-under-jdk-only-20260918.md (retired)
import java.nio.*;
import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.*;
public class JdkOnlyPerf {
    static long t0;
    static void start() { t0 = System.nanoTime(); }
    static void end(String n, Object sink) { System.out.println(String.format("%-28s %6d ms  (%s)", n, (System.nanoTime() - t0) / 1000000, sink == null ? "" : sink.hashCode() == 0 ? "" : "")); }
    public static void main(String[] a) throws Exception {
        int N = 1_000_000;
        for (int round = 0; round < 2; round++) {
            System.out.println("-- round " + round);
            start(); HashMap<Integer,Integer> hm = new HashMap<>(); for (int i = 0; i < N; i++) hm.put(i, i); long s = 0; for (int i = 0; i < N; i++) s += hm.get(i); end("HashMap put+get 1M", s);
            start(); HashSet<Integer> hs = new HashSet<>(); for (int i = 0; i < N; i++) hs.add(i); s = 0; for (int x : hs) s += x; end("HashSet add+iterate 1M", s);
            start(); ArrayList<Integer> al = new ArrayList<>(); for (int i = 0; i < N; i++) al.add(i); s = 0; for (int i = 0; i < N; i++) s += al.get(i); end("ArrayList add+get 1M", s);
            start(); ConcurrentHashMap<Integer,Integer> chm = new ConcurrentHashMap<>(); for (int i = 0; i < N; i++) chm.put(i, i); s = 0; for (int i = 0; i < N; i++) s += chm.get(i); end("CHM put+get 1M", s);
            start(); StringBuilder sb = new StringBuilder(); for (int i = 0; i < N; i++) { sb.append(i); if (sb.length() > 1000) sb.setLength(0); } end("StringBuilder append 1M", sb.length());
            start(); s = 0; for (int i = 0; i < N; i++) s += ("k" + i).hashCode(); end("String concat+hash 1M", s);
            start(); AtomicLong al2 = new AtomicLong(); for (int i = 0; i < N; i++) al2.incrementAndGet(); end("AtomicLong inc 1M", al2.get());
            start(); ByteBuffer bb = ByteBuffer.allocateDirect(1 << 16); for (int i = 0; i < N; i++) { bb.putInt((i & 1023) * 4, i); s += bb.getInt((i & 1023) * 4); } end("DirectByteBuffer putInt/getInt 1M", s);
            start(); ByteBuffer hb = ByteBuffer.allocate(1 << 16); for (int i = 0; i < N; i++) { hb.putInt((i & 1023) * 4, i); s += hb.getInt((i & 1023) * 4); } end("HeapByteBuffer putInt/getInt 1M", s);
            start(); byte[] src = new byte[4096], dst = new byte[4096]; for (int i = 0; i < 100000; i++) System.arraycopy(src, 0, dst, 0, 4096); end("arraycopy 4K x100k", dst.length);
            start(); ThreadLocal<int[]> tl = ThreadLocal.withInitial(() -> new int[1]); for (int i = 0; i < N; i++) tl.get()[0]++; end("ThreadLocal get 1M", 0);
            start(); Object lock = new Object(); int c = 0; for (int i = 0; i < N; i++) synchronized (lock) { c++; } end("synchronized 1M", c);
            start(); s = 0; for (int i = 0; i < N; i++) s += System.identityHashCode(new Object()); end("identityHashCode 1M", s);
            start(); long[] arr = new long[1 << 16]; for (int r = 0; r < 20; r++) for (int i = 0; i < arr.length; i++) arr[i] += i * r; end("long[] loop 1.3M", arr[5]);
            start(); ArrayDeque<Integer> dq = new ArrayDeque<>(); for (int i = 0; i < N; i++) { dq.addLast(i); if (dq.size() > 100) dq.pollFirst(); } end("ArrayDeque 1M", dq.size());
            start(); TreeMap<Integer,Integer> tm = new TreeMap<>(); for (int i = 0; i < 200000; i++) tm.put(i * 7 % 200003, i); s = 0; for (int k : tm.keySet()) s += k; end("TreeMap put+iter 200k", s);
            start(); LinkedList<Integer> ll = new LinkedList<>(); for (int i = 0; i < 300000; i++) ll.add(i); s = 0; for (int x : ll) s += x; end("LinkedList 300k", s);
            start(); java.util.zip.CRC32 crc = new java.util.zip.CRC32(); byte[] d = new byte[1 << 16]; for (int i = 0; i < 200; i++) crc.update(d, 0, d.length); end("CRC32 12MB", crc.getValue());
            start(); java.util.zip.Deflater df = new java.util.zip.Deflater(); byte[] in = new byte[1 << 20]; Random rnd = new Random(1); for (int i = 0; i < in.length; i += 4) in[i] = (byte) rnd.nextInt(16); df.setInput(in); df.finish(); byte[] out = new byte[1 << 21]; int n = df.deflate(out); end("Deflater 1MB", n);
            start(); java.security.MessageDigest md = java.security.MessageDigest.getInstance("SHA-256"); for (int i = 0; i < 2000; i++) md.update(new byte[1024]); end("SHA-256 2MB", md.digest().length);
        }
    }
}
