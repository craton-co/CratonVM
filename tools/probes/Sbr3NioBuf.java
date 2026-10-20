import java.io.*;
import java.lang.foreign.*;
import java.lang.invoke.*;
import java.lang.management.*;
import java.lang.module.*;
import java.nio.*;
import java.nio.channels.*;
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.*;

/**
 * SBR3 NIOBUF family probe (HotSpot-diffed): Bits counters via BufferPoolMXBean, buffer session rows
 * (heap/direct/segment-backed/arena-backed), channel I/O over temp direct buffers, ModuleReferenceImpl.open.
 * Rows print "name |result|" with addresses/hash digits normalised. Env NB_ONLY=<prefix> filters rows.
 */
public class Sbr3NioBuf {
    static String norm(Object v) {
        return String.valueOf(v).replaceAll("0x[0-9a-fA-F]+", "0xA").replaceAll("@[0-9a-f]+", "@X");
    }
    static void row(String n, Object v) { System.out.println(n + " |" + norm(v) + "|"); }
    interface T { Object run() throws Throwable; }
    static void t(String n, T r) {
        String only = System.getenv("NB_ONLY");
        if (only != null && !n.startsWith(only)) return;
        try { row(n, r.run()); } catch (Throwable e) { row(n, "THREW " + e.getClass().getName() + ": " + norm(e.getMessage()).replaceAll("[0-9]{4,}", "N")); }
    }
    static BufferPoolMXBean pool(String name) {
        for (var p : ManagementFactory.getPlatformMXBeans(BufferPoolMXBean.class)) if (p.getName().equals(name)) return p;
        return null;
    }
    static long[] snap() { var p = pool("direct"); return new long[]{p.getCount(), p.getMemoryUsed(), p.getTotalCapacity()}; }
    static String delta(long[] a, long[] b) { return "dcount=" + (b[0] - a[0]) + " dused=" + (b[1] - a[1]) + " dcap=" + (b[2] - a[2]); }

    public static void main(String[] args) throws Throwable {
        // ---- Bits counters
        t("bits.pools", () -> { var l = new ArrayList<String>(); for (var p : ManagementFactory.getPlatformMXBeans(BufferPoolMXBean.class)) l.add(p.getName()); Collections.sort(l); return l; });
        var keep = new ArrayList<ByteBuffer>();
        for (int sz : new int[]{0, 1, 64, 1000, 4096, 4097, 100000}) {
            t("bits.alloc." + sz, () -> { long[] a = snap(); keep.add(ByteBuffer.allocateDirect(sz)); return delta(a, snap()); });
        }
        t("bits.allocOrder", () -> { long[] a = snap(); keep.add(ByteBuffer.allocateDirect(10).order(ByteOrder.nativeOrder())); keep.add(ByteBuffer.allocateDirect(10)); return delta(a, snap()); });
        t("bits.slice.noCount", () -> { long[] a = snap(); var b = keep.get(3); var s = b.slice(); var d = b.duplicate(); var r = b.asReadOnlyBuffer(); var i = b.asIntBuffer(); return delta(a, snap()); });
        t("bits.heap.noCount", () -> { long[] a = snap(); var h = ByteBuffer.allocate(5000); var w = ByteBuffer.wrap(new byte[100]); return delta(a, snap()); });
        t("bits.arena.noCount", () -> { long[] a = snap(); try (Arena ar = Arena.ofConfined()) { ar.allocate(4096); return delta(a, snap()); } });
        t("bits.mapped.pool", () -> { var p = pool("mapped"); return p == null ? "none" : (p.getCount() >= 0) + " " + (p.getMemoryUsed() >= 0) + " " + (p.getTotalCapacity() >= 0); });
        t("bits.oom", () -> ByteBuffer.allocateDirect(Integer.MAX_VALUE).capacity());
        t("bits.neg", () -> ByteBuffer.allocateDirect(-1));
        t("bits.afterOomIntact", () -> { long[] a = snap(); keep.add(ByteBuffer.allocateDirect(200)); return delta(a, snap()); });
        t("bits.freeViaGc", () -> {
            long before = snap()[0];
            for (int i = 0; i < 50; i++) ByteBuffer.allocateDirect(1024 * 1024);
            for (int i = 0; i < 10; i++) { System.gc(); Thread.sleep(50); }
            long after = snap()[0];
            return "grew<=50: " + (after - before <= 50) + " reclaimed: " + (after - before < 50);
        });
        t("bits.pageAlign", () -> Boolean.getBoolean("sun.nio.PageAlignDirectMemory"));
        t("bits.mem.mxbean", () -> pool("direct").getMemoryUsed() >= 0 && pool("direct").getTotalCapacity() >= 0);
        t("bits.mem.mxbean.name", () -> pool("direct").getName() + " " + pool("direct").getObjectName());
        t("bits.mem.mxbean.mapped.name", () -> pool("mapped").getObjectName());

        // ---- Heap buffer session / checkSession
        t("heap.get.put", () -> { var b = ByteBuffer.allocate(16); b.put(3, (byte) 4); b.putInt(4, 0x01020304); return b.get(3) + " " + b.getInt(4) + " " + b.getLong(8); });
        t("heap.relative", () -> { var b = ByteBuffer.allocate(16); b.putShort((short) 7).putChar('x').putFloat(1.5f); b.flip(); return b.getShort() + " " + b.getChar() + " " + b.getFloat(); });
        t("heap.bulk", () -> { var b = ByteBuffer.allocate(8); b.put(new byte[]{1, 2, 3}); b.flip(); byte[] o = new byte[3]; b.get(o); return Arrays.toString(o); });
        t("heap.views", () -> { var b = ByteBuffer.allocate(16); var i = b.asIntBuffer(); i.put(0, 42); return b.getInt(0) + " " + i.get(0) + " " + b.asLongBuffer().capacity(); });
        t("heap.slice.dup.ro", () -> { var b = ByteBuffer.allocate(8); b.put(2, (byte) 9); var s = b.slice(2, 4); var d = b.duplicate(); var r = b.asReadOnlyBuffer(); return s.get(0) + " " + d.get(2) + " " + r.get(2) + " " + r.isReadOnly(); });
        t("heap.ro.put", () -> ByteBuffer.allocate(8).asReadOnlyBuffer().put(0, (byte) 1));
        t("heap.oob", () -> ByteBuffer.allocate(8).getLong(4));
        t("heap.compare.equals", () -> { var a = ByteBuffer.wrap(new byte[]{1, 2}); var b = ByteBuffer.wrap(new byte[]{1, 3}); return a.compareTo(b) + " " + a.equals(b) + " " + a.mismatch(b); });
        t("heap.char.string", () -> { var c = CharBuffer.wrap("hello"); return c.get(1) + " " + c.subSequence(1, 3) + " " + c; });
        t("heap.int.long.double", () -> { var i = IntBuffer.allocate(4); i.put(1, 5); var l = LongBuffer.allocate(2); l.put(0, 9L); var d = DoubleBuffer.allocate(2); d.put(1, 2.5); return i.get(1) + " " + l.get(0) + " " + d.get(1); });
        t("heap.hasArray", () -> { var b = ByteBuffer.allocate(8); return b.hasArray() + " " + b.arrayOffset() + " " + b.isDirect(); });

        // ---- Direct buffer session
        t("dir.get.put", () -> { var b = ByteBuffer.allocateDirect(16); b.put(3, (byte) 4); b.putInt(4, 0x01020304); return b.get(3) + " " + b.getInt(4) + " " + b.getLong(8); });
        t("dir.order", () -> { var b = ByteBuffer.allocateDirect(8).order(ByteOrder.LITTLE_ENDIAN); b.putInt(0, 1); return b.get(0) + " " + b.order(); });
        t("dir.views", () -> { var b = ByteBuffer.allocateDirect(16); var i = b.asIntBuffer(); i.put(1, 42); return b.getInt(4) + " " + i.isDirect(); });
        t("dir.slice.dup.ro", () -> { var b = ByteBuffer.allocateDirect(8); b.put(2, (byte) 9); var s = b.slice(2, 4); var d = b.duplicate(); var r = b.asReadOnlyBuffer(); return s.get(0) + " " + d.get(2) + " " + r.get(2) + " " + r.isReadOnly() + " " + r.isDirect(); });
        t("dir.ro.put", () -> ByteBuffer.allocateDirect(8).asReadOnlyBuffer().put(0, (byte) 1));
        t("dir.bulk", () -> { var b = ByteBuffer.allocateDirect(8); b.put(new byte[]{1, 2, 3}); b.flip(); byte[] o = new byte[3]; b.get(o); return Arrays.toString(o); });
        t("dir.copy.heap", () -> { var d = ByteBuffer.allocateDirect(8); d.put(new byte[]{5, 6, 7}); d.flip(); var h = ByteBuffer.allocate(8); h.put(d); return Arrays.toString(h.array()); });
        t("dir.mismatch", () -> { var a = ByteBuffer.allocateDirect(4); var b = ByteBuffer.allocate(4); b.put(2, (byte) 1); return a.mismatch(b) + " " + a.compareTo(b); });
        t("dir.oob", () -> ByteBuffer.allocateDirect(8).getLong(4));
        t("dir.clear.compact", () -> { var b = ByteBuffer.allocateDirect(8); b.put((byte) 1).put((byte) 2).flip(); b.get(); b.compact(); return b.position() + " " + b.get(0); });
        t("dir.char.dbl", () -> { var b = ByteBuffer.allocateDirect(16); b.putDouble(0, 1.25); b.putChar(8, 'q'); return b.getDouble(0) + " " + b.getChar(8); });

        // ---- Segment-backed (the arena carrier answers MemorySessionImpl)
        t("seg.ofBuffer.direct", () -> { var b = ByteBuffer.allocateDirect(16); b.put(3, (byte) 7); var s = MemorySegment.ofBuffer(b); return s.byteSize() + " " + s.get(ValueLayout.JAVA_BYTE, 3) + " " + s.isNative(); });
        t("seg.ofBuffer.heap", () -> { var b = ByteBuffer.allocate(16); b.put(3, (byte) 7); var s = MemorySegment.ofBuffer(b); return s.byteSize() + " " + s.get(ValueLayout.JAVA_BYTE, 3) + " " + s.isNative(); });
        t("seg.ofBuffer.ro", () -> { var s = MemorySegment.ofBuffer(ByteBuffer.allocate(8).asReadOnlyBuffer()); return s.isReadOnly(); });
        t("seg.ofBuffer.pos", () -> { var b = ByteBuffer.allocateDirect(16); b.position(4); b.limit(12); var s = MemorySegment.ofBuffer(b); return s.byteSize(); });
        t("seg.ofBuffer.int", () -> { var b = IntBuffer.allocate(8); b.position(2); return MemorySegment.ofBuffer(b).byteSize(); });
        t("seg.ofBuffer.alive", () -> { var b = ByteBuffer.allocateDirect(8); return MemorySegment.ofBuffer(b).scope().isAlive(); });
        t("seg.arenaBuf.get", () -> { try (Arena ar = Arena.ofConfined()) { var s = ar.allocate(8); s.set(ValueLayout.JAVA_BYTE, 0, (byte) 3); var b = s.asByteBuffer(); return b.get(0) + " " + b.isDirect() + " " + b.capacity(); } });
        t("seg.arenaBuf.closed.get", () -> { var ar = Arena.ofConfined(); var b = ar.allocate(8).asByteBuffer(); ar.close(); return b.get(0); });
        t("seg.arenaBuf.closed.put", () -> { var ar = Arena.ofConfined(); var b = ar.allocate(8).asByteBuffer(); ar.close(); return b.put(0, (byte) 1); });
        t("seg.arenaBuf.closed.bulk", () -> { var ar = Arena.ofConfined(); var b = ar.allocate(8).asByteBuffer(); ar.close(); return b.get(new byte[2]); });
        t("seg.arenaBuf.closed.view", () -> { var ar = Arena.ofConfined(); var b = ar.allocate(8).asByteBuffer(); var v = b.asIntBuffer(); ar.close(); return v.get(0); });
        t("seg.arenaBuf.closed.slice", () -> { var ar = Arena.ofConfined(); var b = ar.allocate(8).asByteBuffer(); ar.close(); return b.slice().capacity(); });
        t("seg.arenaBuf.closed.dup", () -> { var ar = Arena.ofConfined(); var b = ar.allocate(8).asByteBuffer(); ar.close(); return b.duplicate().capacity(); });
        t("seg.arenaBuf.closed.isDirect", () -> { var ar = Arena.ofConfined(); var b = ar.allocate(8).asByteBuffer(); ar.close(); return b.isDirect(); });
        t("seg.arenaBuf.otherThread", () -> {
            var ar = Arena.ofConfined(); var b = ar.allocate(8).asByteBuffer();
            var res = new String[1];
            var th = new Thread(() -> { try { res[0] = "got " + b.get(0); } catch (Throwable e) { res[0] = e.getClass().getName() + ": " + e.getMessage(); } });
            th.start(); th.join(); ar.close(); return res[0];
        });
        t("seg.arenaBuf.shared.otherThread", () -> {
            var ar = Arena.ofShared(); var b = ar.allocate(8).asByteBuffer();
            var res = new String[1];
            var th = new Thread(() -> { try { res[0] = "got " + b.get(0); } catch (Throwable e) { res[0] = e.getClass().getName() + ": " + e.getMessage(); } });
            th.start(); th.join(); ar.close(); return res[0];
        });
        t("seg.arenaBuf.shared.closed", () -> { var ar = Arena.ofShared(); var b = ar.allocate(8).asByteBuffer(); ar.close(); return b.getInt(0); });
        t("seg.arenaBuf.auto", () -> { var b = Arena.ofAuto().allocate(8).asByteBuffer(); b.putInt(0, 5); return b.getInt(0); });
        t("seg.arenaBuf.global", () -> { var b = Arena.global().allocate(8).asByteBuffer(); b.putInt(0, 6); return b.getInt(0); });
        t("seg.ofBuffer.arena.closed", () -> { var ar = Arena.ofConfined(); var b = ar.allocate(8).asByteBuffer(); var s = MemorySegment.ofBuffer(b); ar.close(); return s.get(ValueLayout.JAVA_BYTE, 0); });
        t("seg.ofBuffer.arena.alive", () -> { var ar = Arena.ofConfined(); var b = ar.allocate(8).asByteBuffer(); var s = MemorySegment.ofBuffer(b); boolean a = s.scope().isAlive(); ar.close(); return a + " " + s.scope().isAlive(); });
        t("seg.ofBuffer.heap.closeless", () -> { var b = ByteBuffer.allocate(8); var s = MemorySegment.ofBuffer(b); return s.scope().isAlive() + " " + s.scope().equals(Arena.global().scope()); });
        t("seg.asByteBuffer.ro", () -> { try (Arena ar = Arena.ofConfined()) { var b = ar.allocate(8).asReadOnly().asByteBuffer(); return b.isReadOnly(); } });
        t("seg.asByteBuffer.slice.arena", () -> { try (Arena ar = Arena.ofConfined()) { var b = ar.allocate(16).asByteBuffer(); b.position(4); var s = b.slice(); s.put(0, (byte) 8); return b.get(4) + " " + s.capacity(); } });
        t("seg.channel.write.arenaBuf", () -> {
            Path p = Files.createTempFile("nb3", ".bin");
            try (Arena ar = Arena.ofConfined(); var ch = FileChannel.open(p, StandardOpenOption.WRITE)) {
                var s = ar.allocate(8); for (int i = 0; i < 8; i++) s.set(ValueLayout.JAVA_BYTE, i, (byte) (i + 1));
                int n = ch.write(s.asByteBuffer()); return n + " " + Files.size(p);
            } finally { Files.deleteIfExists(p); }
        });
        t("seg.channel.read.arenaBuf.closed", () -> {
            Path p = Files.createTempFile("nb3", ".bin"); Files.write(p, new byte[]{1, 2, 3, 4});
            var ar = Arena.ofConfined(); var b = ar.allocate(8).asByteBuffer(); ar.close();
            try (var ch = FileChannel.open(p, StandardOpenOption.READ)) { return ch.read(b); } finally { Files.deleteIfExists(p); }
        });
        t("seg.channel.read.arenaBuf.otherThread", () -> {
            Path p = Files.createTempFile("nb3", ".bin"); Files.write(p, new byte[]{1, 2, 3, 4});
            var ar = Arena.ofConfined(); var b = ar.allocate(8).asByteBuffer();
            var res = new String[1];
            var th = new Thread(() -> { try (var ch = FileChannel.open(p, StandardOpenOption.READ)) { res[0] = "read " + ch.read(b); } catch (Throwable e) { res[0] = e.getClass().getName() + ": " + e.getMessage(); } });
            th.start(); th.join(); ar.close(); Files.deleteIfExists(p); return res[0];
        });

        // ---- Channel I/O over temp / direct / heap buffers (IOUtil.acquireScope paths)
        t("chan.file.heapBuf", () -> {
            Path p = Files.createTempFile("nb3", ".bin");
            try (var ch = FileChannel.open(p, StandardOpenOption.WRITE, StandardOpenOption.READ)) {
                int w = ch.write(ByteBuffer.wrap(new byte[]{1, 2, 3, 4, 5})); var b = ByteBuffer.allocate(8); ch.position(0); int r = ch.read(b); return w + " " + r + " " + b.get(4);
            } finally { Files.deleteIfExists(p); }
        });
        t("chan.file.directBuf", () -> {
            Path p = Files.createTempFile("nb3", ".bin");
            try (var ch = FileChannel.open(p, StandardOpenOption.WRITE, StandardOpenOption.READ)) {
                var d = ByteBuffer.allocateDirect(8); d.put(new byte[]{9, 8, 7}).flip(); int w = ch.write(d); var b = ByteBuffer.allocateDirect(8); ch.position(0); int r = ch.read(b); return w + " " + r + " " + b.get(2);
            } finally { Files.deleteIfExists(p); }
        });
        t("chan.file.gather", () -> {
            Path p = Files.createTempFile("nb3", ".bin");
            try (var ch = FileChannel.open(p, StandardOpenOption.WRITE, StandardOpenOption.READ)) {
                long w = ch.write(new ByteBuffer[]{ByteBuffer.wrap(new byte[]{1, 2}), ByteBuffer.allocateDirect(2)}); return w + " " + ch.size();
            } finally { Files.deleteIfExists(p); }
        });
        t("chan.file.positional", () -> {
            Path p = Files.createTempFile("nb3", ".bin"); Files.write(p, new byte[]{1, 2, 3, 4});
            try (var ch = FileChannel.open(p, StandardOpenOption.READ)) { var b = ByteBuffer.allocateDirect(2); int r = ch.read(b, 2); return r + " " + b.get(0); } finally { Files.deleteIfExists(p); }
        });
        t("chan.files.write.read", () -> { Path p = Files.createTempFile("nb3", ".bin"); try { Files.write(p, new byte[1000]); return Files.readAllBytes(p).length + " " + Files.size(p); } finally { Files.deleteIfExists(p); } });
        t("chan.files.newByteChannel", () -> { Path p = Files.createTempFile("nb3", ".bin"); try (var ch = Files.newByteChannel(p, StandardOpenOption.WRITE)) { return ch.write(ByteBuffer.wrap(new byte[]{1, 2, 3})); } finally { Files.deleteIfExists(p); } });
        t("chan.map", () -> {
            Path p = Files.createTempFile("nb3", ".bin"); Files.write(p, new byte[]{1, 2, 3, 4});
            try (var ch = FileChannel.open(p, StandardOpenOption.READ, StandardOpenOption.WRITE)) { var m = ch.map(FileChannel.MapMode.READ_WRITE, 0, 4); m.put(1, (byte) 9); m.force(); return m.get(0) + " " + m.get(1) + " " + m.isDirect() + " " + m.capacity(); } finally { Files.deleteIfExists(p); }
        });
        t("chan.map.segment", () -> {
            Path p = Files.createTempFile("nb3", ".bin"); Files.write(p, new byte[]{1, 2, 3, 4});
            try (var ch = FileChannel.open(p, StandardOpenOption.READ, StandardOpenOption.WRITE); Arena ar = Arena.ofConfined()) { var s = ch.map(FileChannel.MapMode.READ_WRITE, 0, 4, ar); var b = s.asByteBuffer(); b.put(2, (byte) 7); return s.get(ValueLayout.JAVA_BYTE, 2) + " " + b.get(2); } finally { Files.deleteIfExists(p); }
        });
        t("chan.pipe", () -> { var pp = Pipe.open(); var d = ByteBuffer.allocateDirect(4); d.putInt(77).flip(); pp.sink().write(d); var r = ByteBuffer.allocate(4); pp.source().read(r); pp.sink().close(); pp.source().close(); return r.getInt(0); });
        t("chan.socket", () -> {
            try (var ss = java.nio.channels.ServerSocketChannel.open()) {
                ss.bind(new java.net.InetSocketAddress(java.net.InetAddress.getLoopbackAddress(), 0));
                var c = SocketChannel.open(ss.getLocalAddress()); var a = ss.accept();
                var d = ByteBuffer.allocateDirect(8); d.putLong(0x1122334455667788L).flip(); c.write(d);
                var r = ByteBuffer.allocateDirect(8); while (r.hasRemaining()) a.read(r);
                var hb = ByteBuffer.allocate(4); hb.putInt(5).flip(); a.write(hb); var hr = ByteBuffer.allocate(4); while (hr.hasRemaining()) c.read(hr);
                c.close(); a.close(); return Long.toHexString(r.getLong(0)) + " " + hr.getInt(0);
            }
        });
        t("chan.datagram", () -> {
            try (var d1 = DatagramChannel.open().bind(new java.net.InetSocketAddress(java.net.InetAddress.getLoopbackAddress(), 0)); var d2 = DatagramChannel.open()) {
                var b = ByteBuffer.allocateDirect(4); b.putInt(3).flip(); d2.send(b, d1.getLocalAddress());
                var r = ByteBuffer.allocateDirect(8); d1.receive(r); return r.position() + " " + r.getInt(0);
            }
        });
        t("chan.charset.enc", () -> { var e = java.nio.charset.StandardCharsets.UTF_8.newEncoder(); var d = ByteBuffer.allocateDirect(16); e.encode(CharBuffer.wrap("héllo"), d, true); d.flip(); return d.remaining() + " " + d.get(1); });
        t("chan.charset.dec", () -> { var dec = java.nio.charset.StandardCharsets.UTF_8.newDecoder(); var b = ByteBuffer.allocateDirect(4); b.put(new byte[]{104, (byte) 0xc3, (byte) 0xa9}).flip(); return dec.decode(b); });
        t("chan.zip.deflate.direct", () -> {
            var z = new java.util.zip.CRC32(); var d = ByteBuffer.allocateDirect(8); d.put(new byte[]{1, 2, 3}).flip(); z.update(d); return z.getValue();
        });
        t("chan.adler.direct", () -> { var z = new java.util.zip.Adler32(); var d = ByteBuffer.allocateDirect(8); d.put(new byte[]{1, 2, 3}).flip(); z.update(d); return z.getValue(); });

        // ---- ModuleReferenceImpl.open
        ModuleFinder sf = ModuleFinder.ofSystem();
        t("mri.open.base", () -> { try (var rd = sf.find("java.base").get().open()) { return rd.getClass().getName().replaceAll("\\$.*", "") + " " + rd.find("java/lang/Object.class").isPresent(); } });
        t("mri.open.twice", () -> { var ref = sf.find("java.base").get(); try (var a = ref.open(); var b = ref.open()) { return (a != b) + " " + a.find("java/lang/String.class").isPresent(); } });
        t("mri.open.find.missing", () -> { try (var rd = sf.find("java.base").get().open()) { return rd.find("java/lang/Nope.class").isPresent(); } });
        t("mri.open.find.uri", () -> { try (var rd = sf.find("java.base").get().open()) { return rd.find("java/lang/Object.class").get().toString(); } });
        t("mri.open.read", () -> { try (var rd = sf.find("java.base").get().open(); var in = rd.open("java/lang/Object.class").get()) { var h = in.readNBytes(4); return String.format("%02x%02x%02x%02x", h[0], h[1], h[2], h[3]); } });
        t("mri.open.read.bb", () -> { try (var rd = sf.find("java.base").get().open()) { var bb = rd.read("java/lang/Object.class").get(); try { return bb.remaining() > 100 && bb.getInt(0) == 0xCAFEBABE; } finally { rd.release(bb); } } });
        t("mri.open.list", () -> { try (var rd = sf.find("java.sql").get().open()) { return rd.list().anyMatch(n -> n.equals("java/sql/Connection.class")); } });
        t("mri.open.list.count", () -> { try (var rd = sf.find("java.logging").get().open()) { return rd.list().filter(n -> n.endsWith(".class")).count() > 20; } });
        t("mri.open.closed", () -> { var rd = sf.find("java.base").get().open(); rd.close(); return rd.find("java/lang/Object.class").isPresent(); });
        t("mri.open.closed.list", () -> { var rd = sf.find("java.base").get().open(); rd.close(); return rd.list().count(); });
        t("mri.open.other", () -> { var l = new ArrayList<String>(); for (String m : new String[]{"java.sql", "java.xml", "jdk.unsupported", "java.net.http"}) { try (var rd = sf.find(m).get().open()) { l.add(m + "=" + rd.find("module-info.class").isPresent()); } } return l; });
        t("mri.open.moduleinfo", () -> { try (var rd = sf.find("java.sql").get().open(); var in = rd.open("module-info.class").get()) { return in.readNBytes(4).length; } });
        t("mri.open.loc", () -> { var ref = sf.find("java.base").get(); return ref.location().isPresent() ? ref.location().get().getScheme() : "none"; });
        t("mri.open.concurrent", () -> {
            var ex = Executors.newFixedThreadPool(4); var fs = new ArrayList<Future<Boolean>>();
            for (int i = 0; i < 8; i++) fs.add(ex.submit(() -> { try (var rd = sf.find("java.base").get().open()) { return rd.find("java/lang/Object.class").isPresent(); } }));
            boolean all = true; for (var f : fs) all &= f.get(); ex.shutdown(); return all;
        });
        t("mri.layer.open", () -> { var m = Object.class.getModule(); var rd = ModuleLayer.boot().configuration().findModule("java.base").get().reference().open(); try { return rd.find("java/lang/Object.class").isPresent(); } finally { rd.close(); } });
        t("mri.loader.resource", () -> Object.class.getResource("Object.class") != null);
        t("mri.module.resource", () -> { try (var in = Object.class.getModule().getResourceAsStream("java/lang/Object.class")) { return in != null; } });
    }
}
