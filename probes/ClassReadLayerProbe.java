/*
 * ClassReadLayerProbe — which layer of the class-file read costs the 280x?
 *
 * `AnnotationScanCostProbe` prices Tomcat's whole annotation scan (BCEL parse
 * of every .class in a JAR) at ~1,800 us/class against HotSpot's ~6.5 us — the
 * webapp-deploy wall. That probe cannot say WHICH layer is expensive, and the
 * obvious candidates behave very differently on this VM:
 *
 *   - `DataInputStream.readUnsignedShort` / `readInt` are CratonVM NATIVES, so
 *     each costs a native-funnel entry (~330-810 ns) rather than bytecode;
 *   - `BufferedInputStream.read()` is JDK bytecode that takes a lock on every
 *     call (JDK 25 InternalLock, else `synchronized`) — 529 ns for an
 *     uncontended monitor here, 10.6 us for a ReentrantLock
 *     (`probes/AqsBreakdownProbe.java`);
 *   - `DataInputStream.readUTF` is JDK bytecode with a per-char decode loop.
 *
 * Each rung below isolates one of those over the SAME bytes, so the ratios
 * between rungs on one VM are meaningful even on a loaded host. Run on both
 * VMs and compare rung by rung.
 *
 *   javac -d <out> probes/ClassReadLayerProbe.java
 *   <vm> -cp <out> ClassReadLayerProbe [bytes] [reps]
 */

import java.io.BufferedInputStream;
import java.io.ByteArrayInputStream;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.util.Locale;

public class ClassReadLayerProbe {

    public static void main(String[] args) throws Exception {
        int size = args.length > 0 ? Integer.parseInt(args[0]) : 16 * 1024;
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 40;

        byte[] raw = new byte[size];
        for (int i = 0; i < size; i++) {
            raw[i] = (byte) (i * 31 + 7);
        }
        // A block of short UTF strings, the shape a constant pool actually has.
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        DataOutputStream dos = new DataOutputStream(bos);
        int utfCount = 0;
        while (bos.size() < size) {
            dos.writeUTF("org/apache/example/Type" + utfCount + ";method" + utfCount);
            utfCount++;
        }
        dos.flush();
        byte[] utfs = bos.toByteArray();

        warm(raw, utfs, utfCount, Math.max(reps / 8, 2));

        row("ByteArrayInputStream.read()          per byte",
                timeSingleByte(raw, reps, false), (long) size * reps);
        row("BufferedInputStream.read()           per byte",
                timeSingleByte(raw, reps, true), (long) size * reps);
        row("BufferedInputStream.read(byte[],0,n) per byte",
                timeBulk(raw, reps), (long) size * reps);
        row("DataInputStream.readUnsignedShort    per call",
                timeUnsignedShort(raw, reps, true), (long) (size / 2) * reps);
        row("  same, over a ByteArrayInputStream  per call",
                timeUnsignedShort(raw, reps, false), (long) (size / 2) * reps);
        row("DataInputStream.readUTF              per call",
                timeUtf(utfs, utfCount, reps, true), (long) utfCount * reps);
        row("  same, over a ByteArrayInputStream  per call",
                timeUtf(utfs, utfCount, reps, false), (long) utfCount * reps);
    }

    private static void warm(byte[] raw, byte[] utfs, int utfCount, int reps) throws Exception {
        timeSingleByte(raw, reps, false);
        timeSingleByte(raw, reps, true);
        timeBulk(raw, reps);
        timeUnsignedShort(raw, reps, true);
        timeUnsignedShort(raw, reps, false);
        timeUtf(utfs, utfCount, reps, true);
        timeUtf(utfs, utfCount, reps, false);
    }

    private static InputStream open(byte[] data, boolean buffered) {
        InputStream in = new ByteArrayInputStream(data);
        return buffered ? new BufferedInputStream(in) : in;
    }

    private static long timeSingleByte(byte[] data, int reps, boolean buffered) throws Exception {
        long t0 = System.nanoTime();
        int sink = 0;
        for (int r = 0; r < reps; r++) {
            InputStream in = open(data, buffered);
            int b;
            while ((b = in.read()) >= 0) {
                sink += b;
            }
            in.close();
        }
        long d = System.nanoTime() - t0;
        if (sink == Integer.MIN_VALUE) {
            throw new IllegalStateException();
        }
        return d;
    }

    private static long timeBulk(byte[] data, int reps) throws Exception {
        byte[] buf = new byte[512];
        long t0 = System.nanoTime();
        int sink = 0;
        for (int r = 0; r < reps; r++) {
            InputStream in = open(data, true);
            int n;
            while ((n = in.read(buf, 0, buf.length)) > 0) {
                sink += n;
            }
            in.close();
        }
        long d = System.nanoTime() - t0;
        if (sink == Integer.MIN_VALUE) {
            throw new IllegalStateException();
        }
        return d;
    }

    private static long timeUnsignedShort(byte[] data, int reps, boolean buffered) throws Exception {
        long t0 = System.nanoTime();
        int sink = 0;
        for (int r = 0; r < reps; r++) {
            DataInputStream in = new DataInputStream(open(data, buffered));
            for (int i = 0; i < data.length / 2; i++) {
                sink += in.readUnsignedShort();
            }
            in.close();
        }
        long d = System.nanoTime() - t0;
        if (sink == Integer.MIN_VALUE) {
            throw new IllegalStateException();
        }
        return d;
    }

    private static long timeUtf(byte[] data, int count, int reps, boolean buffered) throws Exception {
        long t0 = System.nanoTime();
        int sink = 0;
        for (int r = 0; r < reps; r++) {
            DataInputStream in = new DataInputStream(open(data, buffered));
            for (int i = 0; i < count; i++) {
                sink += in.readUTF().length();
            }
            in.close();
        }
        long d = System.nanoTime() - t0;
        if (sink == Integer.MIN_VALUE) {
            throw new IllegalStateException();
        }
        return d;
    }

    private static void row(String name, long ns, long ops) {
        System.out.println(String.format(Locale.US, "PROBE %-46s %10.2f ms  %9.1f ns/op", name,
                ns / 1e6, (double) ns / ops));
    }
}
