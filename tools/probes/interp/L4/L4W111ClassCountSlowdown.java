import java.io.*;
import java.lang.classfile.*;
import java.lang.constant.*;
import java.lang.invoke.MethodType;
import java.nio.file.*;

/**
 * Does the VM slow down once many classes are defined?
 *
 *   java L4W111ClassCountSlowdown gen N file      (HotSpot: writes N class files to one blob)
 *   cratonvm L4W111ClassCountSlowdown run file [k] [init|define] [spinSeconds]
 *
 * "run" times three loops (static call, allocation, MethodType intern hit), defines the first
 * k classes of the blob (default: all) in a user loader (initializing each when "init" is
 * given), then times the same loops again. Each loop's median of 5 reps is printed before and
 * after, plus the after/before ratio, so one process compares itself. With spinSeconds the
 * "after" loops then repeat for that long (a window to attach a sampler to), after a "SPIN" line.
 */
public class L4W111ClassCountSlowdown {
    static final int CALLS = 2_000_000, ALLOCS = 1_000_000, MTS = 200_000, REPS = 5;

    public static void main(String[] a) throws Exception {
        if (a[0].equals("gen")) { gen(Integer.parseInt(a[1]), a[2]); return; }
        byte[][] blobs = read(a[1]);
        int k = a.length > 2 ? Integer.parseInt(a[2]) : blobs.length;
        boolean init = a.length > 3 && a[3].equals("init");
        long[] before = timeAll();
        long t0 = System.nanoTime();
        Loader l = new Loader();
        long sum = 0;
        for (int i = 0; i < k; i++) {
            Class<?> c = l.def("gen.C" + i, blobs[i]);
            if (init) sum += (int) c.getMethod("m").invoke(null);
        }
        long defMs = (System.nanoTime() - t0) / 1_000_000;
        long[] after = timeAll();
        System.out.println("defined=" + k + " init=" + init + " defineMs=" + defMs + " sum=" + sum);
        String[] names = {"static-call", "alloc", "methodtype"};
        for (int i = 0; i < 3; i++)
            System.out.printf("%-12s before=%6.2f ns  after=%6.2f ns  ratio=%.2f%n", names[i],
                    before[i] / 1000.0, after[i] / 1000.0, (double) after[i] / before[i]);
        if (a.length > 4) {
            System.out.println("SPIN");
            long end = System.nanoTime() + Long.parseLong(a[4]) * 1_000_000_000L;
            while (System.nanoTime() < end) timeAll();
        }
        System.out.println("sink=" + sink);
    }

    static final class Loader extends ClassLoader {
        Class<?> def(String n, byte[] b) { return defineClass(n, b, 0, b.length); }
    }

    static int sink;
    static int s(int x) { return x + 1; }
    static final class Box { int v; Box(int v) { this.v = v; } }

    /** Per-op times in picoseconds (median of REPS), for the three loops. */
    static long[] timeAll() {
        long[][] r = new long[3][REPS];
        for (int rep = 0; rep < REPS; rep++) {
            long t = System.nanoTime();
            int x = 0;
            for (int i = 0; i < CALLS; i++) x = s(x);
            r[0][rep] = (System.nanoTime() - t) * 1000 / CALLS;
            sink += x;
            t = System.nanoTime();
            Box b = null;
            for (int i = 0; i < ALLOCS; i++) b = new Box(i);
            r[1][rep] = (System.nanoTime() - t) * 1000 / ALLOCS;
            sink += b.v;
            t = System.nanoTime();
            MethodType mt = null;
            for (int i = 0; i < MTS; i++) mt = MethodType.methodType(int.class, String.class);
            r[2][rep] = (System.nanoTime() - t) * 1000 / MTS;
            sink += mt.parameterCount();
        }
        long[] med = new long[3];
        for (int i = 0; i < 3; i++) { java.util.Arrays.sort(r[i]); med[i] = r[i][REPS / 2]; }
        return med;
    }

    static byte[][] read(String f) throws IOException {
        try (DataInputStream in = new DataInputStream(new BufferedInputStream(new FileInputStream(f)))) {
            byte[][] out = new byte[in.readInt()][];
            for (int i = 0; i < out.length; i++) { out[i] = new byte[in.readInt()]; in.readFully(out[i]); }
            return out;
        }
    }

    /** Each class: a static int field, a static int m(), an instance field and a constructor. */
    static void gen(int n, String f) throws IOException {
        try (DataOutputStream out = new DataOutputStream(new BufferedOutputStream(new FileOutputStream(f)))) {
            out.writeInt(n);
            for (int i = 0; i < n; i++) {
                ClassDesc cd = ClassDesc.of("gen.C" + i);
                final int v = i;
                byte[] b = ClassFile.of().build(cd, cb -> cb
                        .withFlags(ClassFile.ACC_PUBLIC)
                        .withField("sf", ConstantDescs.CD_int, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC)
                        .withField("f", ConstantDescs.CD_long, ClassFile.ACC_PUBLIC)
                        .withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC, c -> c
                                .aload(0).invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                                .return_())
                        .withMethodBody("m", MethodTypeDesc.of(ConstantDescs.CD_int), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, c -> c
                                .ldc(v).dup().putstatic(cd, "sf", ConstantDescs.CD_int).ireturn()));
                out.writeInt(b.length);
                out.write(b);
            }
        }
    }
}
