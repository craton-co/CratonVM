import java.io.File;
import java.io.RandomAccessFile;
import java.lang.reflect.Field;
import java.nio.MappedByteBuffer;
import java.nio.channels.FileChannel;

/** Does a `MappedByteBuffer` this VM produces carry a real address? (L1 R5.)
 *
 *  H2's full-text tests reach the unclassified null-base fallback 513+ times at
 *  `offset=0x0`, attributed to Lucene 9.7's
 *  `MappedByteBufferIndexInputProvider` -- whose `unmapHackImpl()` builds a
 *  MethodHandle on `sun.misc.Unsafe.invokeCleaner` to unmap.
 *
 *  The SAME Lucene code on HotSpot does not fault, and on HotSpot a null base
 *  with offset 0 is a read of absolute address 0. So HotSpot is not making that
 *  access -- which means something upstream in this VM is answering 0 where
 *  HotSpot answers a real address. This asks that question directly.
 *
 *  Prints no address: an address differs per run by construction. Prints only
 *  whether it is non-zero, which is the whole question.
 */
public class MappedAddrProbe {
    static void p(String tag, Object v) {
        System.out.println(tag + " |" + v + "|");
    }

    static long addressOf(Object buf) {
        for (Class<?> c = buf.getClass(); c != null; c = c.getSuperclass()) {
            try {
                Field f = c.getDeclaredField("address");
                f.setAccessible(true);
                return f.getLong(buf);
            } catch (ReflectiveOperationException ignored) {
            }
        }
        return -1L;
    }

    public static void main(String[] args) throws Exception {
        File tmp = File.createTempFile("l1r5", ".dat");
        tmp.deleteOnExit();
        try (RandomAccessFile raf = new RandomAccessFile(tmp, "rw")) {
            raf.setLength(8192);
            try (FileChannel ch = raf.getChannel()) {
                MappedByteBuffer mb = ch.map(FileChannel.MapMode.READ_WRITE, 0, 8192);
                p("mapped isDirect", mb.isDirect());
                p("mapped capacity", mb.capacity());
                p("mapped address is non-zero", addressOf(mb) != 0);
                // the round-trip still has to work whatever the address says
                mb.putInt(0, 0x0BADF00D);
                p("mapped int round-trip", Integer.toHexString(mb.getInt(0)));
                MappedByteBuffer dup = (MappedByteBuffer) mb.duplicate();
                p("duplicate address is non-zero", addressOf(dup) != 0);
                p("slice address is non-zero", addressOf(mb.slice()) != 0);
            }
        }
        // The comparison case: a plain direct buffer, which a prior record
        // already says carries a non-zero address on JDK 21+.
        java.nio.ByteBuffer direct = java.nio.ByteBuffer.allocateDirect(64);
        p("allocateDirect address is non-zero", addressOf(direct) != 0);
        System.out.println("DONE MappedAddrProbe");
    }
}
