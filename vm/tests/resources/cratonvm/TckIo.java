package cratonvm;

import java.io.*;
import java.nio.ByteBuffer;
import java.nio.CharBuffer;
import java.nio.IntBuffer;
import java.nio.LongBuffer;

/**
 * Session 48: TCK — java.io / java.nio Tests.
 *
 * 50 test methods exercising File, FileInputStream, FileOutputStream,
 * ByteArrayStreams, ByteBuffer, CharBuffer, IntBuffer, LongBuffer,
 * and end-to-end I/O scenarios through the JVM.
 */
public class TckIo {

    // ===================================================================
    // java.io.File tests
    // ===================================================================

    public static int file_createDeleteExists() {
        try {
            File f = new File("tck_io_test_cde.tmp");
            if (f.exists()) f.delete();
            f.createNewFile();
            if (!f.exists()) return 0;
            f.delete();
            if (f.exists()) return 0;
            return 1;
        } catch (Exception e) { return 0; }
    }

    public static int file_isFileIsDirectory() {
        try {
            File f = new File("tck_io_test_ff.tmp");
            f.createNewFile();
            if (!f.isFile()) { f.delete(); return 0; }
            if (f.isDirectory()) { f.delete(); return 0; }
            f.delete();
            return 1;
        } catch (Exception e) { return 0; }
    }

    public static int file_mkdir() {
        File d = new File("tck_io_test_dir");
        if (d.exists()) d.delete();
        boolean ok = d.mkdir();
        if (!ok) return 0;
        if (!d.isDirectory()) { d.delete(); return 0; }
        d.delete();
        return 1;
    }

    public static int file_length() {
        try {
            File f = new File("tck_io_test_len.tmp");
            FileOutputStream fos = new FileOutputStream(f);
            fos.write(new byte[]{1, 2, 3, 4, 5});
            fos.close();
            long len = f.length();
            f.delete();
            return len == 5 ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    public static int file_absolutePath() {
        File f = new File("tck_io_test_abs.tmp");
        String abs = f.getAbsolutePath();
        // Absolute path should be longer than relative name
        return abs.length() > "tck_io_test_abs.tmp".length() ? 1 : 0;
    }

    public static int file_canReadWrite() {
        try {
            File f = new File("tck_io_test_rw.tmp");
            f.createNewFile();
            boolean r = f.canRead();
            boolean w = f.canWrite();
            f.delete();
            return (r && w) ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    // ===================================================================
    // java.io.FileOutputStream / FileInputStream tests
    // ===================================================================

    public static int fos_writeSingleByte() {
        try {
            File f = new File("tck_io_test_wsb.tmp");
            FileOutputStream fos = new FileOutputStream(f);
            fos.write(42);
            fos.close();
            FileInputStream fis = new FileInputStream(f);
            int b = fis.read();
            fis.close();
            f.delete();
            return b == 42 ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    public static int fos_writeBulk() {
        try {
            File f = new File("tck_io_test_wb.tmp");
            byte[] data = {10, 20, 30, 40, 50};
            FileOutputStream fos = new FileOutputStream(f);
            fos.write(data);
            fos.close();
            FileInputStream fis = new FileInputStream(f);
            byte[] buf = new byte[5];
            int n = fis.read(buf);
            fis.close();
            f.delete();
            if (n != 5) return 0;
            for (int i = 0; i < 5; i++) {
                if (buf[i] != data[i]) return 0;
            }
            return 1;
        } catch (Exception e) { return 0; }
    }

    public static int fos_appendMode() {
        try {
            File f = new File("tck_io_test_app.tmp");
            FileOutputStream fos1 = new FileOutputStream(f);
            fos1.write(new byte[]{1, 2, 3});
            fos1.close();
            FileOutputStream fos2 = new FileOutputStream(f, true);
            fos2.write(new byte[]{4, 5});
            fos2.close();
            long len = f.length();
            f.delete();
            return len == 5 ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    public static int fis_readEof() {
        try {
            File f = new File("tck_io_test_eof.tmp");
            FileOutputStream fos = new FileOutputStream(f);
            fos.write(99);
            fos.close();
            FileInputStream fis = new FileInputStream(f);
            fis.read(); // consume the byte
            int eof = fis.read();
            fis.close();
            f.delete();
            return eof == -1 ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    public static int fis_available() {
        try {
            File f = new File("tck_io_test_avail.tmp");
            FileOutputStream fos = new FileOutputStream(f);
            fos.write(new byte[]{1, 2, 3, 4});
            fos.close();
            FileInputStream fis = new FileInputStream(f);
            int avail = fis.available();
            fis.close();
            f.delete();
            return avail >= 4 ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    public static int fis_skip() {
        try {
            File f = new File("tck_io_test_skip.tmp");
            FileOutputStream fos = new FileOutputStream(f);
            fos.write(new byte[]{10, 20, 30, 40, 50});
            fos.close();
            FileInputStream fis = new FileInputStream(f);
            fis.skip(3);
            int b = fis.read();
            fis.close();
            f.delete();
            return b == 40 ? 1 : 0;
        } catch (Exception e) { return 0; }
    }

    public static int fis_closeIdempotent() {
        try {
            File f = new File("tck_io_test_ci.tmp");
            FileOutputStream fos = new FileOutputStream(f);
            fos.write(1);
            fos.close();
            FileInputStream fis = new FileInputStream(f);
            fis.close();
            fis.close(); // second close should not throw
            f.delete();
            return 1;
        } catch (Exception e) { return 0; }
    }

    // ===================================================================
    // java.io.ByteArrayInputStream / ByteArrayOutputStream tests
    // ===================================================================

    public static int baos_basic() {
        ByteArrayOutputStream baos = new ByteArrayOutputStream();
        baos.write(65);
        baos.write(66);
        baos.write(67);
        byte[] arr = baos.toByteArray();
        if (arr.length != 3) return 0;
        if (arr[0] != 65 || arr[1] != 66 || arr[2] != 67) return 0;
        return 1;
    }

    public static int baos_size() {
        ByteArrayOutputStream baos = new ByteArrayOutputStream();
        if (baos.size() != 0) return 0;
        baos.write(1);
        baos.write(2);
        return baos.size() == 2 ? 1 : 0;
    }

    public static int baos_reset() {
        ByteArrayOutputStream baos = new ByteArrayOutputStream();
        baos.write(1);
        baos.write(2);
        baos.reset();
        return baos.size() == 0 ? 1 : 0;
    }

    public static int bais_readAll() {
        byte[] data = {10, 20, 30};
        ByteArrayInputStream bais = new ByteArrayInputStream(data);
        if (bais.read() != 10) return 0;
        if (bais.read() != 20) return 0;
        if (bais.read() != 30) return 0;
        if (bais.read() != -1) return 0;
        return 1;
    }

    public static int bais_available() {
        byte[] data = {1, 2, 3, 4, 5};
        ByteArrayInputStream bais = new ByteArrayInputStream(data);
        if (bais.available() != 5) return 0;
        bais.read();
        return bais.available() == 4 ? 1 : 0;
    }

    public static int bais_skip() {
        byte[] data = {10, 20, 30, 40, 50};
        ByteArrayInputStream bais = new ByteArrayInputStream(data);
        bais.skip(2);
        return bais.read() == 30 ? 1 : 0;
    }

    public static int baos_toString() {
        ByteArrayOutputStream baos = new ByteArrayOutputStream();
        baos.write(72);  // H
        baos.write(105); // i
        String s = baos.toString();
        return "Hi".equals(s) ? 1 : 0;
    }

    // ===================================================================
    // java.io.StringReader / StringWriter tests
    // ===================================================================

    public static int sw_basic() {
        StringWriter sw = new StringWriter();
        sw.write("Hello");
        sw.write(' ');
        sw.write("World");
        return "Hello World".equals(sw.toString()) ? 1 : 0;
    }

    public static int sr_readChar() {
        try {
            StringReader sr = new StringReader("ABC");
            if (sr.read() != 'A') return 0;
            if (sr.read() != 'B') return 0;
            if (sr.read() != 'C') return 0;
            if (sr.read() != -1) return 0;
            return 1;
        } catch (Exception e) { return 0; }
    }

    public static int sr_readCharArrayMultiline() {
        try {
            String s = "line1\nline2\nline3";
            StringReader sr = new StringReader(s);
            char[] buf = new char[64];
            int n = sr.read(buf, 0, buf.length);
            if (n != s.length()) return 0;
            if (!s.equals(new String(buf, 0, n))) return 0;
            if (sr.read(buf, 0, buf.length) != -1) return 0;
            return 1;
        } catch (Exception e) { return 0; }
    }

    // ===================================================================
    // java.nio.ByteBuffer tests
    // ===================================================================

    public static int bb_allocateCapacity() {
        ByteBuffer bb = ByteBuffer.allocate(64);
        if (bb.capacity() != 64) return 0;
        if (bb.position() != 0) return 0;
        if (bb.limit() != 64) return 0;
        if (bb.remaining() != 64) return 0;
        return 1;
    }

    public static int bb_putGetFlip() {
        ByteBuffer bb = ByteBuffer.allocate(8);
        bb.put((byte) 10);
        bb.put((byte) 20);
        bb.put((byte) 30);
        bb.flip();
        if (bb.remaining() != 3) return 0;
        if (bb.get() != 10) return 0;
        if (bb.get() != 20) return 0;
        if (bb.get() != 30) return 0;
        return 1;
    }

    public static int bb_putGetAbsolute() {
        ByteBuffer bb = ByteBuffer.allocate(8);
        bb.put(3, (byte) 99);
        return bb.get(3) == 99 ? 1 : 0;
    }

    public static int bb_wrap() {
        byte[] arr = {5, 10, 15, 20};
        ByteBuffer bb = ByteBuffer.wrap(arr);
        if (bb.capacity() != 4) return 0;
        if (bb.limit() != 4) return 0;
        if (bb.get() != 5) return 0;
        if (bb.get() != 10) return 0;
        return 1;
    }

    public static int bb_clearRewind() {
        ByteBuffer bb = ByteBuffer.allocate(8);
        bb.put((byte) 1);
        bb.put((byte) 2);
        bb.clear();
        if (bb.position() != 0) return 0;
        if (bb.limit() != 8) return 0;
        bb.put((byte) 3);
        bb.rewind();
        return bb.get() == 3 ? 1 : 0;
    }

    public static int bb_markReset() {
        ByteBuffer bb = ByteBuffer.allocate(8);
        bb.put((byte) 1);
        bb.put((byte) 2);
        bb.flip();
        bb.get(); // read 1
        bb.mark();
        bb.get(); // read 2
        bb.reset();
        return bb.get() == 2 ? 1 : 0;
    }

    public static int bb_putGetInt() {
        ByteBuffer bb = ByteBuffer.allocate(16);
        bb.putInt(0x12345678);
        bb.flip();
        return bb.getInt() == 0x12345678 ? 1 : 0;
    }

    public static int bb_putGetLong() {
        ByteBuffer bb = ByteBuffer.allocate(16);
        bb.putLong(0x123456789ABCDEF0L);
        bb.flip();
        return bb.getLong() == 0x123456789ABCDEF0L ? 1 : 0;
    }

    public static int bb_putGetShort() {
        ByteBuffer bb = ByteBuffer.allocate(8);
        bb.putShort((short) 12345);
        bb.flip();
        return bb.getShort() == 12345 ? 1 : 0;
    }

    public static int bb_putGetFloat() {
        ByteBuffer bb = ByteBuffer.allocate(8);
        bb.putFloat(3.14f);
        bb.flip();
        float v = bb.getFloat();
        return Math.abs(v - 3.14f) < 0.001f ? 1 : 0;
    }

    public static int bb_putGetDouble() {
        ByteBuffer bb = ByteBuffer.allocate(16);
        bb.putDouble(2.718281828);
        bb.flip();
        double v = bb.getDouble();
        return Math.abs(v - 2.718281828) < 0.0001 ? 1 : 0;
    }

    public static int bb_putGetChar() {
        ByteBuffer bb = ByteBuffer.allocate(8);
        bb.putChar('Z');
        bb.flip();
        return bb.getChar() == 'Z' ? 1 : 0;
    }

    public static int bb_hasArray() {
        ByteBuffer bb = ByteBuffer.allocate(4);
        return bb.hasArray() ? 1 : 0;
    }

    public static int bb_array() {
        ByteBuffer bb = ByteBuffer.allocate(4);
        bb.put((byte) 10);
        bb.put((byte) 20);
        byte[] arr = bb.array();
        if (arr[0] != 10) return 0;
        if (arr[1] != 20) return 0;
        return 1;
    }

    public static int bb_remaining() {
        ByteBuffer bb = ByteBuffer.allocate(10);
        bb.put((byte) 1);
        bb.put((byte) 2);
        bb.put((byte) 3);
        return bb.remaining() == 7 ? 1 : 0;
    }

    public static int bb_compact() {
        ByteBuffer bb = ByteBuffer.allocate(8);
        bb.put((byte) 1);
        bb.put((byte) 2);
        bb.put((byte) 3);
        bb.flip();
        bb.get(); // consume 1
        bb.compact();
        // After compact: position=2, limit=8, [2, 3, ...]
        if (bb.position() != 2) return 0;
        if (bb.limit() != 8) return 0;
        return 1;
    }

    public static int bb_slice() {
        ByteBuffer bb = ByteBuffer.allocate(8);
        bb.put((byte) 10);
        bb.put((byte) 20);
        bb.put((byte) 30);
        bb.put((byte) 40);
        bb.position(1);
        bb.limit(3);
        ByteBuffer slice = bb.slice();
        if (slice.capacity() != 2) return 0;
        if (slice.get() != 20) return 0;
        if (slice.get() != 30) return 0;
        return 1;
    }

    public static int bb_duplicate() {
        ByteBuffer bb = ByteBuffer.allocate(4);
        bb.put((byte) 7);
        bb.put((byte) 8);
        bb.flip();
        ByteBuffer dup = bb.duplicate();
        if (dup.get() != 7) return 0;
        if (dup.get() != 8) return 0;
        return 1;
    }

    // ===================================================================
    // java.nio.CharBuffer tests
    // ===================================================================

    public static int cb_allocatePutGet() {
        CharBuffer cb = CharBuffer.allocate(16);
        cb.put('H');
        cb.put('i');
        cb.flip();
        if (cb.get() != 'H') return 0;
        if (cb.get() != 'i') return 0;
        return 1;
    }

    public static int cb_wrapCharSequence() {
        CharBuffer cb = CharBuffer.wrap("Hello");
        if (cb.length() != 5) return 0;
        if (cb.charAt(0) != 'H') return 0;
        if (cb.charAt(4) != 'o') return 0;
        return 1;
    }

    // ===================================================================
    // java.nio.IntBuffer tests
    // ===================================================================

    public static int ib_allocatePutGet() {
        IntBuffer ib = IntBuffer.allocate(8);
        ib.put(100);
        ib.put(200);
        ib.put(300);
        ib.flip();
        if (ib.get() != 100) return 0;
        if (ib.get() != 200) return 0;
        if (ib.get() != 300) return 0;
        return 1;
    }

    public static int ib_wrapArray() {
        int[] arr = {10, 20, 30, 40};
        IntBuffer ib = IntBuffer.wrap(arr);
        if (ib.capacity() != 4) return 0;
        if (ib.get() != 10) return 0;
        return 1;
    }

    // ===================================================================
    // java.nio.LongBuffer tests
    // ===================================================================

    public static int lb_allocatePutGet() {
        LongBuffer lb = LongBuffer.allocate(4);
        lb.put(1000000000L);
        lb.put(2000000000L);
        lb.flip();
        if (lb.get() != 1000000000L) return 0;
        if (lb.get() != 2000000000L) return 0;
        return 1;
    }

    // ===================================================================
    // ByteArrayInputStream extended + BufferedOutputStream tests
    // ===================================================================

    public static int bytearray_inputstream_basic() {
        byte[] data = {10, 20, 30, 40, 50};
        ByteArrayInputStream bais = new ByteArrayInputStream(data);
        // read single byte
        if (bais.read() != 10) return 0;
        // available after one read
        if (bais.available() != 4) return 0;
        // skip 2 bytes (20, 30)
        long skipped = bais.skip(2);
        if (skipped != 2) return 0;
        // next read should be 40
        if (bais.read() != 40) return 0;
        // available should be 1
        if (bais.available() != 1) return 0;
        // read last byte
        if (bais.read() != 50) return 0;
        // EOF
        if (bais.read() != -1) return 0;
        return 1;
    }

    public static int buffered_output_basic() {
        try {
            ByteArrayOutputStream baos = new ByteArrayOutputStream();
            BufferedOutputStream bos = new BufferedOutputStream(baos);
            bos.write(65); // A
            bos.write(66); // B
            bos.write(new byte[]{67, 68, 69}); // C, D, E
            bos.flush();
            byte[] result = baos.toByteArray();
            if (result.length != 5) return 0;
            if (result[0] != 65) return 0;
            if (result[1] != 66) return 0;
            if (result[2] != 67) return 0;
            if (result[3] != 68) return 0;
            if (result[4] != 69) return 0;
            return 1;
        } catch (Exception e) { return 0; }
    }

    // ===================================================================
    // End-to-end / integration tests
    // ===================================================================

    public static int e2e_writeReadRoundtrip() {
        try {
            File f = new File("tck_io_test_e2e.tmp");
            String msg = "CratonVM I/O roundtrip";
            FileOutputStream fos = new FileOutputStream(f);
            byte[] data = msg.getBytes();
            fos.write(data);
            fos.close();
            FileInputStream fis = new FileInputStream(f);
            byte[] buf = new byte[data.length];
            int n = fis.read(buf);
            fis.close();
            f.delete();
            if (n != data.length) return 0;
            for (int i = 0; i < n; i++) {
                if (buf[i] != data[i]) return 0;
            }
            return 1;
        } catch (Exception e) { return 0; }
    }

    public static int e2e_byteBufferToArray() {
        ByteBuffer bb = ByteBuffer.allocate(8);
        bb.putInt(0xCAFEBABE);
        bb.flip();
        byte[] arr = new byte[4];
        bb.get(arr, 0, 4);
        // Big-endian: 0xCA, 0xFE, 0xBA, 0xBE
        if ((arr[0] & 0xFF) != 0xCA) return 0;
        if ((arr[1] & 0xFF) != 0xFE) return 0;
        if ((arr[2] & 0xFF) != 0xBA) return 0;
        if ((arr[3] & 0xFF) != 0xBE) return 0;
        return 1;
    }

    public static int e2e_baosToInputStream() {
        ByteArrayOutputStream baos = new ByteArrayOutputStream();
        baos.write(1);
        baos.write(2);
        baos.write(3);
        byte[] bytes = baos.toByteArray();
        ByteArrayInputStream bais = new ByteArrayInputStream(bytes);
        if (bais.read() != 1) return 0;
        if (bais.read() != 2) return 0;
        if (bais.read() != 3) return 0;
        if (bais.read() != -1) return 0;
        return 1;
    }
}
