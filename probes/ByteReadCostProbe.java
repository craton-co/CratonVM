import java.io.BufferedInputStream;
import java.io.ByteArrayInputStream;
import java.io.DataInputStream;
import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;

/**
 * Prices the single operation the Tomcat deploy path spends its time in.
 *
 * `ContextConfig.processAnnotationsJar` -> `tomcat.util.bcel.classfile
 * .ConstantPool.<init>` -> `Constant.readConstant` reads class bytes ONE AT A
 * TIME through a `DataInputStream` over a `BufferedInputStream`. A webapp
 * deploy is dominated by that loop, so its per-byte cost is what a deploy-time
 * comparison against HotSpot actually measures.
 *
 * Reports ns/byte for four shapes so the cost can be attributed:
 *   1. DataInputStream.readUnsignedByte over BufferedInputStream(FileInputStream)
 *   2. BufferedInputStream(FileInputStream).read()          — no DataInputStream
 *   3. DataInputStream.readUnsignedByte over ByteArrayInputStream — no file I/O
 *   4. FileInputStream.read(byte[])                          — bulk baseline
 *
 * Run the same command line on HotSpot and CratonVM and compare columns; the
 * ratio between shapes localises the cost (stream layering vs the file fd vs
 * per-call dispatch).
 */
public class ByteReadCostProbe {

    private static final int SIZE = 512 * 1024;
    private static final int ROUNDS = 5;

    public static void main(String[] args) throws Exception {
        File f = File.createTempFile("byteread", ".bin");
        f.deleteOnExit();
        byte[] payload = new byte[SIZE];
        for (int i = 0; i < SIZE; i++) {
            payload[i] = (byte) i;
        }
        try (FileOutputStream o = new FileOutputStream(f)) {
            o.write(payload);
        }

        // Warm up every shape before the measured rounds.
        for (int i = 0; i < 2; i++) {
            dataOverBuffered(f);
            bufferedOnly(f);
            dataOverArray(payload);
            bulk(f);
        }

        report("DataInputStream.readUnsignedByte / BufferedInputStream / file", () -> dataOverBuffered(f));
        report("BufferedInputStream.read() / file", () -> bufferedOnly(f));
        report("DataInputStream.readUnsignedByte / ByteArrayInputStream", () -> dataOverArray(payload));
        report("FileInputStream.read(byte[8192]) bulk", () -> bulk(f));

        f.delete();
    }

    private interface Shape {
        long run() throws IOException;
    }

    private static void report(String label, Shape shape) throws Exception {
        long best = Long.MAX_VALUE;
        long checksum = 0;
        for (int r = 0; r < ROUNDS; r++) {
            long t0 = System.nanoTime();
            checksum = shape.run();
            long dt = System.nanoTime() - t0;
            best = Math.min(best, dt);
        }
        double nsPerByte = (double) best / SIZE;
        System.out.printf("%-58s %9.1f ns/byte  (%6.1f ms for %d KiB, checksum=%d)%n", label, nsPerByte,
                best / 1e6, SIZE / 1024, checksum);
    }

    private static long dataOverBuffered(File f) throws IOException {
        try (InputStream in = new FileInputStream(f);
                BufferedInputStream bis = new BufferedInputStream(in);
                DataInputStream dis = new DataInputStream(bis)) {
            long sum = 0;
            for (int i = 0; i < SIZE; i++) {
                sum += dis.readUnsignedByte();
            }
            return sum;
        }
    }

    private static long bufferedOnly(File f) throws IOException {
        try (InputStream in = new FileInputStream(f); BufferedInputStream bis = new BufferedInputStream(in)) {
            long sum = 0;
            for (int i = 0; i < SIZE; i++) {
                sum += bis.read();
            }
            return sum;
        }
    }

    private static long dataOverArray(byte[] payload) throws IOException {
        try (DataInputStream dis = new DataInputStream(new ByteArrayInputStream(payload))) {
            long sum = 0;
            for (int i = 0; i < SIZE; i++) {
                sum += dis.readUnsignedByte();
            }
            return sum;
        }
    }

    private static long bulk(File f) throws IOException {
        try (InputStream in = new FileInputStream(f)) {
            byte[] buf = new byte[8192];
            long sum = 0;
            int n;
            while ((n = in.read(buf)) > 0) {
                for (int i = 0; i < n; i++) {
                    sum += buf[i] & 0xff;
                }
            }
            return sum;
        }
    }
}
