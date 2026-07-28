/*
 * ByteReadProbe - per-byte stream read cost, the operation that dominates a
 * Tomcat webapp deploy on this VM.
 *
 * A `--stack-dump-on-timeout` sample taken 90 s into ONE
 * TestHostConfigAutomaticDeploymentAddition test method landed here:
 *
 *   ContextConfig.processAnnotationsJar
 *     -> tomcat.util.bcel.classfile.ConstantPool.<init>
 *        -> Constant.readConstant
 *           -> java.io.BufferedInputStream.read()      <-- one call PER BYTE
 *              -> read1
 *
 * Tomcat's own BCEL class parser reads every class in every scanned JAR one
 * byte at a time through a `DataInputStream` over a `BufferedInputStream`, and
 * JDK 25's `BufferedInputStream.read()` takes its `InternalLock` (or
 * `synchronized (this)`) on EVERY call.
 *
 * The four cases isolate the layers:
 *   raw        - FileInputStream.read()            (native call per byte)
 *   buffered   - BufferedInputStream.read()        (+ lock + bytecode per byte)
 *   data       - DataInputStream.readUnsignedByte  (what BCEL actually calls)
 *   bulk       - readAllBytes + array indexing     (the floor)
 *
 * Usage:  cratonvm -cp <dir> ByteReadProbe [file] [rounds]
 */
import java.io.BufferedInputStream;
import java.io.DataInputStream;
import java.io.File;
import java.io.FileInputStream;
import java.io.IOException;
import java.io.InputStream;

public class ByteReadProbe {

    static long sink;

    static double raw(File f) throws IOException {
        long t0 = System.nanoTime();
        long n = 0;
        try (InputStream in = new FileInputStream(f)) {
            int b;
            while ((b = in.read()) >= 0) { n += b; }
        }
        long dt = System.nanoTime() - t0;
        sink += n;
        return dt / (double) f.length();
    }

    static double buffered(File f) throws IOException {
        long t0 = System.nanoTime();
        long n = 0;
        try (InputStream in = new BufferedInputStream(new FileInputStream(f))) {
            int b;
            while ((b = in.read()) >= 0) { n += b; }
        }
        long dt = System.nanoTime() - t0;
        sink += n;
        return dt / (double) f.length();
    }

    static double data(File f) throws IOException {
        long t0 = System.nanoTime();
        long n = 0;
        try (DataInputStream in = new DataInputStream(new BufferedInputStream(new FileInputStream(f)))) {
            for (long i = f.length(); i > 0; i--) { n += in.readUnsignedByte(); }
        }
        long dt = System.nanoTime() - t0;
        sink += n;
        return dt / (double) f.length();
    }

    /** BufferedInputStream over an in-memory source: no file descriptor, no
     *  native read behind the buffer. Isolates "is the buffering working" from
     *  "is the per-byte Java wrapper expensive". */
    static double bufferedOverArray(byte[] all) {
        long t0 = System.nanoTime();
        long n = 0;
        try (InputStream in = new BufferedInputStream(new java.io.ByteArrayInputStream(all))) {
            int b;
            while ((b = in.read()) >= 0) { n += b; }
        } catch (IOException e) {
            throw new RuntimeException(e);
        }
        long dt = System.nanoTime() - t0;
        sink += n;
        return dt / (double) all.length;
    }

    /** The in-memory source on its own, for the floor of the case above. */
    static double arrayStream(byte[] all) throws IOException {
        long t0 = System.nanoTime();
        long n = 0;
        InputStream in = new java.io.ByteArrayInputStream(all);
        int b;
        while ((b = in.read()) >= 0) { n += b; }
        long dt = System.nanoTime() - t0;
        sink += n;
        return dt / (double) all.length;
    }

    static double bulk(File f) throws IOException {
        long t0 = System.nanoTime();
        long n = 0;
        byte[] all;
        try (InputStream in = new FileInputStream(f)) { all = in.readAllBytes(); }
        for (byte b : all) { n += (b & 0xff); }
        long dt = System.nanoTime() - t0;
        sink += n;
        return dt / (double) f.length();
    }

    public static void main(String[] args) throws Exception {
        File f = args.length > 0 ? new File(args[0]) : null;
        if (f == null || !f.isFile()) {
            // Default: a file of our own making, so the probe is self-contained.
            f = File.createTempFile("byteread", ".bin");
            f.deleteOnExit();
            byte[] buf = new byte[256 * 1024];
            for (int i = 0; i < buf.length; i++) { buf[i] = (byte) i; }
            try (java.io.FileOutputStream out = new java.io.FileOutputStream(f)) { out.write(buf); }
        }
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 3;
        System.out.println("file=" + f + " size=" + f.length());
        byte[] mem;
        try (InputStream in = new FileInputStream(f)) { mem = in.readAllBytes(); }
        for (int r = 1; r <= rounds; r++) {
            double bu = bulk(f);
            double ra = raw(f);
            double bf = buffered(f);
            double da = data(f);
            double as = arrayStream(mem);
            double ba = bufferedOverArray(mem);
            System.out.println(String.format(
                    "round %d  bulk=%.3f raw=%.3f buffered=%.3f data=%.3f | arrayStream=%.3f bufferedOverArray=%.3f  ns/byte",
                    r, bu, ra, bf, da, as, ba));
        }
        System.out.println("sink=" + sink);
    }
}
