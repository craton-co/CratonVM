package cratonvm;

import java.io.*;
import java.nio.ByteBuffer;
import java.nio.CharBuffer;

/**
 * I/O bootstrap tests (Session 12).
 *
 * Tests cover: FileInputStream, FileOutputStream, BufferedReader,
 * ByteArrayInputStream, ByteArrayOutputStream, ByteBuffer, CharBuffer,
 * and end-to-end file read/write scenarios.
 */
public class IoBootstrapTest {

    // ---------------------------------------------------------------
    // Test 1: Write a file and read it back
    // ---------------------------------------------------------------
    public static int testFileWriteRead() {
        String path = "io_test_output.txt";
        String content = "Hello CratonVM IO";
        try {
            // Write
            FileOutputStream fos = new FileOutputStream(path);
            byte[] data = content.getBytes();
            fos.write(data);
            fos.flush();
            fos.close();

            // Read back
            FileInputStream fis = new FileInputStream(path);
            byte[] buf = new byte[data.length];
            int n = fis.read(buf);
            fis.close();

            if (n != data.length) return 0;
            for (int i = 0; i < n; i++) {
                if (buf[i] != data[i]) return 0;
            }
            return 1; // success
        } catch (Exception e) {
            return 0;
        }
    }

    // ---------------------------------------------------------------
    // Test 2: FileOutputStream append mode
    // ---------------------------------------------------------------
    public static int testFileAppend() {
        String path = "io_test_append.txt";
        try {
            FileOutputStream fos1 = new FileOutputStream(path);
            fos1.write("ABC".getBytes());
            fos1.close();

            FileOutputStream fos2 = new FileOutputStream(path, true);
            fos2.write("DEF".getBytes());
            fos2.close();

            FileInputStream fis = new FileInputStream(path);
            byte[] buf = new byte[6];
            int n = fis.read(buf);
            fis.close();

            if (n != 6) return 0;
            String result = new String(buf, 0, n);
            if ("ABCDEF".equals(result)) return 1;
            return 0;
        } catch (Exception e) {
            return 0;
        }
    }

    // ---------------------------------------------------------------
    // Test 3: ByteArrayOutputStream and ByteArrayInputStream round-trip
    // ---------------------------------------------------------------
    public static int testByteArrayStreams() {
        try {
            ByteArrayOutputStream baos = new ByteArrayOutputStream();
            baos.write(65); // 'A'
            baos.write(66); // 'B'
            baos.write(67); // 'C'
            byte[] written = baos.toByteArray();
            if (written.length != 3) return 0;
            if (written[0] != 65 || written[1] != 66 || written[2] != 67) return 0;

            ByteArrayInputStream bais = new ByteArrayInputStream(written);
            int a = bais.read();
            int b = bais.read();
            int c = bais.read();
            int eof = bais.read();
            if (a != 65 || b != 66 || c != 67 || eof != -1) return 0;

            return 1; // success
        } catch (Exception e) {
            return 0;
        }
    }

    // ---------------------------------------------------------------
    // Test 4: ByteArrayOutputStream bulk write and size
    // ---------------------------------------------------------------
    public static int testByteArrayBulkWrite() {
        try {
            ByteArrayOutputStream baos = new ByteArrayOutputStream();
            byte[] data = new byte[] { 10, 20, 30, 40, 50 };
            baos.write(data, 1, 3); // write bytes 20, 30, 40
            if (baos.size() != 3) return 0;
            byte[] result = baos.toByteArray();
            if (result[0] != 20 || result[1] != 30 || result[2] != 40) return 0;
            return 1;
        } catch (Exception e) {
            return 0;
        }
    }

    // ---------------------------------------------------------------
    // Test 5: BufferedReader.readLine() via InputStreamReader
    // ---------------------------------------------------------------
    public static int testBufferedReaderReadLine() {
        String path = "io_test_lines.txt";
        try {
            // Write two lines
            FileOutputStream fos = new FileOutputStream(path);
            fos.write("line1\nline2\n".getBytes());
            fos.close();

            // Read lines
            FileInputStream fis = new FileInputStream(path);
            InputStreamReader isr = new InputStreamReader(fis);
            BufferedReader br = new BufferedReader(isr);
            String line1 = br.readLine();
            String line2 = br.readLine();
            String line3 = br.readLine(); // should be null
            br.close();

            if (!"line1".equals(line1)) return 0;
            if (!"line2".equals(line2)) return 0;
            if (line3 != null) return 0;
            return 1;
        } catch (Exception e) {
            return 0;
        }
    }

    // ---------------------------------------------------------------
    // Test 6: FileInputStream.available()
    // ---------------------------------------------------------------
    public static int testFileAvailable() {
        String path = "io_test_avail.txt";
        try {
            FileOutputStream fos = new FileOutputStream(path);
            fos.write(new byte[] { 1, 2, 3, 4, 5 });
            fos.close();

            FileInputStream fis = new FileInputStream(path);
            int avail = fis.available();
            fis.close();

            if (avail >= 5) return 1;
            return 0;
        } catch (Exception e) {
            return 0;
        }
    }

    // ---------------------------------------------------------------
    // Test 7: FileInputStream read single byte
    // ---------------------------------------------------------------
    public static int testFileReadSingleByte() {
        String path = "io_test_single.txt";
        try {
            FileOutputStream fos = new FileOutputStream(path);
            fos.write(42);
            fos.write(99);
            fos.close();

            FileInputStream fis = new FileInputStream(path);
            int b1 = fis.read();
            int b2 = fis.read();
            int eof = fis.read();
            fis.close();

            if (b1 == 42 && b2 == 99 && eof == -1) return 1;
            return 0;
        } catch (Exception e) {
            return 0;
        }
    }

    // ---------------------------------------------------------------
    // Test 8: ByteBuffer allocate and put/get
    // ---------------------------------------------------------------
    public static int testByteBufferAllocate() {
        try {
            ByteBuffer buf = ByteBuffer.allocate(16);
            if (buf.capacity() != 16) return 0;
            if (buf.position() != 0) return 0;
            if (buf.limit() != 16) return 0;

            buf.put((byte) 10);
            buf.put((byte) 20);
            if (buf.position() != 2) return 0;

            buf.flip();
            if (buf.limit() != 2) return 0;
            if (buf.position() != 0) return 0;

            byte a = buf.get();
            byte b = buf.get();
            if (a != 10 || b != 20) return 0;

            return 1;
        } catch (Exception e) {
            return 0;
        }
    }

    // ---------------------------------------------------------------
    // Test 9: ByteBuffer putInt / getInt
    // ---------------------------------------------------------------
    public static int testByteBufferInt() {
        try {
            ByteBuffer buf = ByteBuffer.allocate(8);
            buf.putInt(0x12345678);
            buf.flip();
            int v = buf.getInt();
            if (v == 0x12345678) return 1;
            return 0;
        } catch (Exception e) {
            return 0;
        }
    }

    // ---------------------------------------------------------------
    // Test 10: ByteBuffer wrap
    // ---------------------------------------------------------------
    public static int testByteBufferWrap() {
        try {
            byte[] data = new byte[] { 1, 2, 3, 4 };
            ByteBuffer buf = ByteBuffer.wrap(data);
            if (buf.capacity() != 4) return 0;
            if (buf.get() != 1) return 0;
            if (buf.get() != 2) return 0;
            if (buf.remaining() != 2) return 0;
            return 1;
        } catch (Exception e) {
            return 0;
        }
    }

    // ---------------------------------------------------------------
    // Test 11: ByteArrayOutputStream.toString()
    // ---------------------------------------------------------------
    public static int testByteArrayToString() {
        try {
            ByteArrayOutputStream baos = new ByteArrayOutputStream();
            baos.write("hello".getBytes());
            String s = baos.toString();
            if ("hello".equals(s)) return 1;
            return 0;
        } catch (Exception e) {
            return 0;
        }
    }

    // ---------------------------------------------------------------
    // Test 12: InputStream hierarchy check — ByteArrayInputStream
    //          should be assignable to InputStream
    // ---------------------------------------------------------------
    public static int testInputStreamHierarchy() {
        ByteArrayInputStream bais = new ByteArrayInputStream(new byte[] { 1, 2, 3 });
        // If the hierarchy is correct, this cast works
        InputStream is = bais;
        try {
            int v = is.read();
            if (v == 1) return 1;
            return 0;
        } catch (Exception e) {
            return 0;
        }
    }
}
