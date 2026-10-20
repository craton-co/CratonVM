// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.List;
import java.util.function.IntUnaryOperator;

/**
 * gci i10/gr (2026-10-13): a class-loader-heavy shape whose old collections are
 * STOP-THE-WORLD majors that take the true-root young seed, for item 4 of
 * {@code docs/internal/gc/gch-h4pc-old-mark-and-true-root-review-findings-RETIRED-UNFIXED-20261018.md}
 * (the {@code loader_pin} map rebuilt by {@code TrueRootYoung::build} every
 * true-root major).
 *
 * <p>Why a new shape: vi5-og Q ran {@code GenR5W5ConcUnloadProbe}, whose old
 * collections are all CONCURRENT cycles, so no {@code old_gen_gc_inner} ran and no
 * {@code [gcphase] old-gen young-seed:} line printed (VOID). Here every round
 * ends in {@code System.gc()}: on CratonVM's Generational backend that is a
 * REQUESTED stop-the-world major (non-moving by default, true-root seed armed by
 * {@code CRATONVM_GC_FULL_GC_TRUE_ROOTS}, default ON; moving P-C under
 * {@code CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG=1}).
 *
 * <p>The shape: {@code L} live user class loaders, each defining its own copy
 * of {@link Payload} and holding one instance (so {@code L} loader-pin rows),
 * {@code R} rounds; each round replaces every eighth loader (the replaced ones
 * die), churns young garbage keeping a slice of it across the major, calls
 * {@code System.gc()}, and reads every live payload back.
 *
 * <p>Deterministic stdout (HotSpot {@code -XX:+UseSerialGC} prints the same):
 * <pre>
 *   gr10 loaders=L rounds=R bad=0 checksum=C
 * </pre>
 * Stderr (not compared): {@code [gr10] dropped=D cleared=K ms=T}.
 * Usage: {@code GciGr10LoaderMajorProbe [loaders=1000] [rounds=12]}.
 */
public final class GciGr10LoaderMajorProbe {

    /** Defined once per loader. {@code applyAsInt(0)}: 1 when the body reads
     *  back intact, else 0; any other argument: a hash of the body. */
    public static final class Payload implements IntUnaryOperator {
        final int id;
        final int[] body = new int[8];

        public Payload() {
            this(0);
        }

        public Payload(int id) {
            this.id = id;
            for (int i = 0; i < body.length; i++) {
                body[i] = id * 31 + i;
            }
        }

        @Override
        public int applyAsInt(int op) {
            if (op == 0) {
                for (int i = 0; i < body.length; i++) {
                    if (body[i] != id * 31 + i) {
                        return 0;
                    }
                }
                return 1;
            }
            int h = id;
            for (int i = 0; i < body.length; i++) {
                h = h * 31 + body[i];
            }
            return h;
        }
    }

    /** Defines exactly one name itself and delegates everything else. */
    static final class Throwaway extends ClassLoader {
        private final String target;
        private final byte[] bytes;

        Throwaway(String target, byte[] bytes) {
            super(GciGr10LoaderMajorProbe.class.getClassLoader());
            this.target = target;
            this.bytes = bytes;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (name.equals(target)) {
                synchronized (this) {
                    Class<?> c = findLoadedClass(name);
                    if (c == null) {
                        c = defineClass(name, bytes, 0, bytes.length);
                    }
                    if (resolve) {
                        resolveClass(c);
                    }
                    return c;
                }
            }
            return super.loadClass(name, resolve);
        }
    }

    static volatile Object sink;

    static byte[] readClassBytes(String binaryName) throws Exception {
        String res = binaryName.replace('.', '/') + ".class";
        try (InputStream in = GciGr10LoaderMajorProbe.class.getClassLoader().getResourceAsStream(res)) {
            if (in == null) {
                throw new IllegalStateException("class bytes not on the classpath: " + res);
            }
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            byte[] buf = new byte[4096];
            int n;
            while ((n = in.read(buf)) > 0) {
                out.write(buf, 0, n);
            }
            return out.toByteArray();
        }
    }

    /** A fresh loader, its own Payload class, and one instance of it. */
    static Object make(String name, byte[] bytes, int id) throws Exception {
        Throwaway loader = new Throwaway(name, bytes);
        Class<?> c = loader.loadClass(name);
        if (c.getClassLoader() != loader) {
            throw new IllegalStateException("the payload was not defined by its own loader");
        }
        return c.getConstructor(int.class).newInstance(id);
    }

    public static void main(String[] args) throws Exception {
        int loaders = args.length > 0 ? Integer.parseInt(args[0]) : 1000;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 12;
        String name = GciGr10LoaderMajorProbe.class.getName() + "$Payload";
        byte[] bytes = readClassBytes(name);
        long t0 = System.nanoTime();
        Object[] holders = new Object[loaders];
        int nextId = 0;
        for (int i = 0; i < loaders; i++) {
            holders[i] = make(name, bytes, nextId++);
        }
        List<WeakReference<ClassLoader>> dropped = new ArrayList<>();
        long checksum = 17;
        int bad = 0;
        for (int r = 0; r < rounds; r++) {
            for (int i = r % 8; i < loaders; i += 8) {
                dropped.add(new WeakReference<>(holders[i].getClass().getClassLoader()));
                holders[i] = make(name, bytes, nextId++);
            }
            Object[] keep = new Object[4096];
            for (int k = 0; k < 262144; k++) {
                long[] o = new long[16];
                o[0] = k;
                if ((k & 63) == 0) {
                    keep[(k >>> 6) & 4095] = o;
                }
            }
            sink = keep;
            System.gc();
            for (int i = 0; i < loaders; i++) {
                IntUnaryOperator p = (IntUnaryOperator) holders[i];
                if (p.applyAsInt(0) != 1) {
                    bad++;
                }
                checksum = checksum * 31 + p.applyAsInt(1);
            }
            Object[] kept = (Object[]) sink;
            for (int j = 0; j < kept.length; j++) {
                long[] o = (long[]) kept[j];
                if (o == null || o[0] != ((long) j << 6)) {
                    bad++;
                }
            }
        }
        sink = null;
        int cleared = 0;
        for (WeakReference<ClassLoader> w : dropped) {
            if (w.refersTo(null)) {
                cleared++;
            }
        }
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        System.out.println("gr10 loaders=" + loaders + " rounds=" + rounds + " bad=" + bad
                + " checksum=" + checksum);
        System.err.println("[gr10] dropped=" + dropped.size() + " cleared=" + cleared + " ms=" + ms);
    }
}
