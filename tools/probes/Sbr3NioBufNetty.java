import io.netty.buffer.*;
import io.netty.util.internal.PlatformDependent;
import java.lang.management.*;
import java.nio.*;
import java.nio.charset.StandardCharsets;
import java.util.*;

/**
 * SBR3 NIOBUF: Netty over the buffer session pair and the Bits accounting (the workloads that refused the pair in
 * SBR wave 6: PcapWriteHandler / AdaptiveByteBuf direct buffers). Needs netty-buffer + netty-common on the class path.
 */
public class Sbr3NioBufNetty {
    static void row(String n, Object v) { System.out.println(n + " |" + String.valueOf(v).replaceAll("0x[0-9a-fA-F]+", "0xA").replaceAll("@[0-9a-f]+", "@X") + "|"); }
    interface T { Object run() throws Throwable; }
    static void t(String n, T r) { try { row(n, r.run()); } catch (Throwable e) { row(n, "THREW " + e.getClass().getName() + ": " + e.getMessage()); } }
    static BufferPoolMXBean pool() {
        for (var p : ManagementFactory.getPlatformMXBeans(BufferPoolMXBean.class)) if (p.getName().equals("direct")) return p;
        return null;
    }
    public static void main(String[] a) throws Throwable {
        t("pd.hasUnsafe", () -> PlatformDependent.hasUnsafe());
        t("pd.hasDirectBufferNoCleanerConstructor", () -> PlatformDependent.hasDirectBufferNoCleanerConstructor());
        t("pd.directMemoryLimit", () -> PlatformDependent.maxDirectMemory() > 0);
        t("unpooled.direct.rw", () -> { ByteBuf b = Unpooled.directBuffer(64); b.writeInt(7).writeLong(9L).writeBytes("abc".getBytes()); String s = b.readInt() + " " + b.readLong() + " " + b.readCharSequence(3, StandardCharsets.US_ASCII); b.release(); return s; });
        t("unpooled.heap.rw", () -> { ByteBuf b = Unpooled.buffer(64); b.writeInt(7).writeLong(9L); String s = b.readInt() + " " + b.readLong(); b.release(); return s; });
        t("pooled.direct.rw", () -> { ByteBuf b = PooledByteBufAllocator.DEFAULT.directBuffer(256); b.writeLong(5L).writeShort(2); String s = b.readLong() + " " + b.readShort() + " " + b.isDirect(); b.release(); return s; });
        t("pooled.heap.rw", () -> { ByteBuf b = PooledByteBufAllocator.DEFAULT.heapBuffer(256); b.writeLong(5L); String s = b.readLong() + " " + b.hasArray(); b.release(); return s; });
        t("adaptive.direct.rw", () -> { ByteBuf b = new AdaptiveByteBufAllocator(true).directBuffer(512); b.writeBytes(new byte[300]); String s = b.writerIndex() + " " + b.isDirect(); b.release(); return s; });
        t("adaptive.heap.rw", () -> { ByteBuf b = new AdaptiveByteBufAllocator(false).heapBuffer(512); b.writeBytes(new byte[300]); String s = b.writerIndex() + " " + b.hasArray(); b.release(); return s; });
        t("nioBuffer.direct", () -> { ByteBuf b = Unpooled.directBuffer(32); b.writeInt(0x01020304); ByteBuffer nb = b.nioBuffer(); String s = nb.isDirect() + " " + nb.remaining() + " " + nb.getInt(0); b.release(); return s; });
        t("nioBuffer.pooled.direct", () -> { ByteBuf b = PooledByteBufAllocator.DEFAULT.directBuffer(32); b.writeInt(0x0a0b0c0d); ByteBuffer nb = b.nioBuffer(); String s = nb.isDirect() + " " + nb.remaining() + " " + nb.getInt(0); b.release(); return s; });
        t("wrapped.direct", () -> { ByteBuffer d = ByteBuffer.allocateDirect(16); d.putInt(0, 99); ByteBuf b = Unpooled.wrappedBuffer(d); String s = b.getInt(0) + " " + b.isDirect(); b.release(); return s; });
        t("wrapped.heap", () -> { ByteBuf b = Unpooled.wrappedBuffer(ByteBuffer.wrap(new byte[]{0, 0, 0, 5})); return b.getInt(0) + " " + b.hasArray(); });
        t("slice.dup", () -> { ByteBuf b = Unpooled.directBuffer(32); b.writeBytes(new byte[]{1, 2, 3, 4, 5}); ByteBuf s = b.slice(1, 3); ByteBuf d = b.duplicate(); String r = s.getByte(0) + " " + d.readableBytes() + " " + s.readableBytes(); b.release(); return r; });
        t("composite", () -> { CompositeByteBuf c = Unpooled.compositeBuffer(); c.addComponents(true, Unpooled.directBuffer(4).writeInt(1), Unpooled.buffer(4).writeInt(2)); String s = c.readInt() + " " + c.readInt() + " " + c.nioBufferCount(); c.release(); return s; });
        t("bytebufutil", () -> { ByteBuf b = Unpooled.directBuffer(16).writeBytes(new byte[]{(byte) 0xca, (byte) 0xfe}); String s = ByteBufUtil.hexDump(b) + " " + ByteBufUtil.getBytes(b).length; b.release(); return s; });
        t("copy.direct.heap", () -> { ByteBuf d = Unpooled.directBuffer(8).writeBytes(new byte[]{9, 8, 7}); ByteBuf h = Unpooled.buffer(8); h.writeBytes(d); String s = h.readByte() + " " + h.readByte(); d.release(); return s; });
        t("readonly", () -> { ByteBuf b = Unpooled.unmodifiableBuffer(Unpooled.directBuffer(4).writeInt(3)); try { b.writeInt(1); return "no"; } catch (Throwable e) { return e.getClass().getSimpleName(); } });
        t("direct.counted", () -> { long u0 = PlatformDependent.usedDirectMemory(); ByteBuf b = Unpooled.directBuffer(10000); long u1 = PlatformDependent.usedDirectMemory(); b.release(); long u2 = PlatformDependent.usedDirectMemory(); return (u1 >= u0) + " " + (u2 <= u1); });
        t("pool.counters", () -> { long c0 = pool().getCount(); ByteBuffer d = ByteBuffer.allocateDirect(1000); long c1 = pool().getCount(); return (c1 - c0) + " " + d.capacity(); });
        t("pool.after.netty", () -> { var l = new ArrayList<ByteBuf>(); for (int i = 0; i < 200; i++) l.add(Unpooled.directBuffer(1024)); long u = pool().getMemoryUsed(); l.forEach(ByteBuf::release); return u > 0; });
        t("tomcat.like", () -> { ByteBuffer b = ByteBuffer.allocateDirect(8192); b.put("GET / HTTP/1.1\r\n".getBytes()); b.flip(); byte[] o = new byte[b.remaining()]; b.get(o); b.clear(); return new String(o).trim(); });
    }
}
