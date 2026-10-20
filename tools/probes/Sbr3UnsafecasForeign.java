import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.nio.ByteBuffer;
import java.nio.channels.FileChannel;
import java.nio.channels.Pipe;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;

/**
 * SBR3 UNSAFECAS item 4: foreign defects found in round 3, diffed against HotSpot in both modes.
 * Run plainly, with -XX:MaxDirectMemorySize=1m (and again with --add-exports java.base/jdk.internal.misc=ALL-UNNAMED).
 * Each section prints one line, never throws out of main.
 */
public class Sbr3UnsafecasForeign {
    static void row(String n, Object v) { System.out.println(n + " = " + v); }
    static String ex(Throwable t) { return t.getClass().getName() + ": " + t.getMessage(); }

    public static void main(String[] a) throws Throwable {
        gather();
        pipe();
        maxDirect();
        closedArena();
    }

    static void gather() throws Throwable {
        Path p = Files.createTempFile("sbr3uc", ".bin");
        try {
            try (FileChannel ch = FileChannel.open(p, StandardOpenOption.WRITE)) {
                ByteBuffer[] bs = { ByteBuffer.wrap("abc".getBytes(StandardCharsets.US_ASCII)),
                    ByteBuffer.wrap("defg".getBytes(StandardCharsets.US_ASCII)), ByteBuffer.allocate(0),
                    ByteBuffer.allocateDirect(3).put("hij".getBytes(StandardCharsets.US_ASCII)).flip() };
                long n = ch.write(bs);
                row("gather heap+direct", n + " " + new String(Files.readAllBytes(p), StandardCharsets.US_ASCII) + " pos=" + ch.position()
                        + " rem=" + bs[0].remaining() + bs[1].remaining() + bs[3].remaining());
            } catch (Throwable t) { row("gather heap+direct", ex(t)); }
            try (FileChannel ch = FileChannel.open(p, StandardOpenOption.WRITE, StandardOpenOption.TRUNCATE_EXISTING)) {
                ByteBuffer[] bs = { ByteBuffer.wrap("1234".getBytes()), ByteBuffer.wrap("5678".getBytes()), ByteBuffer.wrap("90".getBytes()) };
                long n = ch.write(bs, 1, 2);
                row("gather slice", n + " " + new String(Files.readAllBytes(p)));
            } catch (Throwable t) { row("gather slice", ex(t)); }
            try (FileChannel ch = FileChannel.open(p, StandardOpenOption.READ)) {
                ByteBuffer[] bs = { ByteBuffer.allocate(2), ByteBuffer.allocateDirect(3), ByteBuffer.allocate(10) };
                long n = ch.read(bs);
                row("scatter read", n + " " + bs[0].position() + bs[1].position() + bs[2].position());
            } catch (Throwable t) { row("scatter read", ex(t)); }
            try (FileChannel ch = FileChannel.open(p, StandardOpenOption.WRITE, StandardOpenOption.TRUNCATE_EXISTING)) {
                ByteBuffer d = ByteBuffer.allocateDirect(5);
                d.put("hello".getBytes()).flip();
                row("single direct write", ch.write(d) + " " + new String(Files.readAllBytes(p)));
            } catch (Throwable t) { row("single direct write", ex(t)); }
        } finally { Files.deleteIfExists(p); }
    }

    static void pipe() throws Throwable {
        try {
            Pipe pp = Pipe.open();
            ByteBuffer d = ByteBuffer.allocateDirect(6);
            d.put("direct".getBytes()).flip();
            int n = pp.sink().write(d);
            ByteBuffer in = ByteBuffer.allocate(16);
            int m = pp.source().read(in);
            row("pipe direct write", n + " " + m + " " + new String(in.array(), 0, in.position()));
            ByteBuffer h = ByteBuffer.wrap("heap".getBytes());
            n = pp.sink().write(h);
            in.clear();
            m = pp.source().read(in);
            row("pipe heap write", n + " " + m + " " + new String(in.array(), 0, in.position()));
            ByteBuffer[] g = { ByteBuffer.allocateDirect(2).put((byte) 'x').put((byte) 'y').flip(), ByteBuffer.wrap("zz".getBytes()) };
            long gn = pp.sink().write(g);
            ByteBuffer ind = ByteBuffer.allocateDirect(16);
            m = pp.source().read(ind);
            row("pipe gather", gn + " " + m);
            pp.sink().close();
            row("pipe eof", pp.source().read(in));
            pp.source().close();
        } catch (Throwable t) { row("pipe", ex(t)); }
    }

    static void maxDirect() {
        try {
            row("maxDirect prop", System.getProperty("jdk.nio.maxCachedBufferSize"));
            ByteBuffer b = ByteBuffer.allocateDirect(512 * 1024);
            row("direct 512k", b.capacity());
            ByteBuffer c = ByteBuffer.allocateDirect(1024 * 1024);
            row("direct +1m", c.capacity());
        } catch (Throwable t) { row("maxDirect", ex(t)); }
        try {
            Class<?> vm = Class.forName("jdk.internal.misc.VM");
            row("VM.maxDirectMemory", vm.getMethod("maxDirectMemory").invoke(null));
        } catch (Throwable t) { row("VM.maxDirectMemory", ex(t).replace("\n", " ")); }
    }

    static void closedArena() {
        try {
            Arena ar = Arena.ofConfined();
            MemorySegment seg = ar.allocate(16);
            seg.set(java.lang.foreign.ValueLayout.JAVA_BYTE, 3, (byte) 9);
            ByteBuffer bb = seg.asByteBuffer();
            row("arena open read", bb.get(3));
            ar.close();
            try { row("arena closed read", bb.get(3)); } catch (Throwable t) { row("arena closed read", ex(t)); }
            try { row("arena closed absolute getInt", bb.getInt(0)); } catch (Throwable t) { row("arena closed absolute getInt", ex(t)); }
            try { bb.put(0, (byte) 1); row("arena closed put", "no throw"); } catch (Throwable t) { row("arena closed put", ex(t)); }
            try { row("arena closed seg read", seg.get(java.lang.foreign.ValueLayout.JAVA_BYTE, 3)); } catch (Throwable t) { row("arena closed seg read", ex(t)); }
            try { ByteBuffer c2 = bb.slice(); row("arena closed slice", c2.capacity()); c2.get(0); } catch (Throwable t) { row("arena closed slice get", ex(t)); }
            try { ByteBuffer big = ByteBuffer.allocate(16); big.put(bb.duplicate()); row("arena closed bulk", "no throw"); } catch (Throwable t) { row("arena closed bulk", ex(t)); }
        } catch (Throwable t) { row("closedArena", ex(t)); }
    }
}
