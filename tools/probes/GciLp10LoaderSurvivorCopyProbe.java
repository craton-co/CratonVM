// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.util.function.Supplier;

/**
 * gci i10/lp: a young-collection workload whose SURVIVORS are instances of a
 * class defined by a user {@code ClassLoader}.
 *
 * <p>Page: {@code docs/internal/gc/gci-i9yl-moving-copy-takes-the-loader-pin-lock-per-copied-object-RETIRED-UNFIXED-20261018.md}.
 * Both Generational moving young evacuators follow the instance -> defining
 * loader edge from every object they copy, through the process-global
 * {@code loader_pin} registry (read lock + hash probe per copied object once
 * any user loader has defined a class). This probe makes nearly every copied
 * object such an instance, so the per-object lookup is the copy's dominant
 * extra cost and {@code CRATONVM_GEN_YOUNG_LOADER_PIN_SNAPSHOT=1} (one snapshot
 * per copy) is measurable in the {@code [gcpause]} line's {@code evac_drain} /
 * {@code cheney_drain} row per {@code objects_copied}.
 *
 * <p>One loader defines {@link Payload} (its bytes read from the classpath).
 * A ring of {@code ring} slots is overwritten {@code rounds} times with fresh
 * payload instances, each slot write followed by {@code garbage} discarded
 * allocations, so every young collection copies the ring entries allocated
 * since the previous one.
 *
 * <p>Deterministic stdout (the HotSpot reference):
 * <pre>
 *   lp10 ring=300000 rounds=40 loader-ok=true payload-ok=true checksum=&lt;n&gt;
 * </pre>
 * Diagnostics (elapsed milliseconds) on stderr only.
 * <pre>
 *   javac -d tools/probes tools/probes/GciLp10LoaderSurvivorCopyProbe.java
 *   java -XX:+UseSerialGC -Xmx256m -cp tools/probes GciLp10LoaderSurvivorCopyProbe 300000 40 3
 * </pre>
 * Usage: {@code GciLp10LoaderSurvivorCopyProbe [ring] [rounds] [garbage]}
 * (defaults 300000, 40, 3).
 */
public final class GciLp10LoaderSurvivorCopyProbe {

    /** The class the user loader defines; every survivor is one of these. */
    public static final class Payload implements Supplier<Object> {
        final int id;
        final long check;

        public Payload() {
            this(0);
        }

        Payload(int id) {
            this.id = id;
            this.check = mix(id);
        }

        static long mix(int id) {
            long x = id * 0x9E3779B97F4A7C15L;
            return x ^ (x >>> 29);
        }

        @Override
        public Object get() {
            return new Payload((id + 1) & 0xFFFFF);
        }

        @Override
        public String toString() {
            return check == mix(id) ? "ok" : "corrupt";
        }

        @Override
        public int hashCode() {
            return id;
        }

        @Override
        public boolean equals(Object o) {
            return o == this;
        }
    }

    /** Defines exactly one name itself and delegates everything else. */
    static final class OneClassLoader extends ClassLoader {
        private final String target;
        private final byte[] bytes;

        OneClassLoader(String target, byte[] bytes) {
            super(GciLp10LoaderSurvivorCopyProbe.class.getClassLoader());
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

    public static void main(String[] args) throws Exception {
        int ring = args.length > 0 ? Integer.parseInt(args[0]) : 300000;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 40;
        int garbage = args.length > 2 ? Integer.parseInt(args[2]) : 3;

        String name = GciLp10LoaderSurvivorCopyProbe.class.getName() + "$Payload";
        OneClassLoader loader = new OneClassLoader(name, readClassBytes(name));
        Class<?> pc = loader.loadClass(name);
        if (pc.getClassLoader() != loader || pc == Payload.class) {
            throw new IllegalStateException("the payload was not defined by the user loader");
        }

        Object[] slots = new Object[ring];
        @SuppressWarnings("unchecked")
        Supplier<Object> cur = (Supplier<Object>) pc.getDeclaredConstructor().newInstance();
        long t0 = System.nanoTime();
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < ring; i++) {
                Object next = cur.get();
                slots[i] = next;
                @SuppressWarnings("unchecked")
                Supplier<Object> s = (Supplier<Object>) next;
                cur = s;
                for (int g = 0; g < garbage; g++) {
                    if (cur.get() == null) {
                        throw new IllegalStateException("unreachable");
                    }
                }
            }
        }
        long elapsedMs = (System.nanoTime() - t0) / 1_000_000L;

        boolean loaderOk = true;
        boolean payloadOk = true;
        long checksum = 0;
        for (int i = 0; i < ring; i++) {
            Object o = slots[i];
            loaderOk &= o.getClass() == pc && o.getClass().getClassLoader() == loader;
            payloadOk &= "ok".equals(o.toString());
            checksum = checksum * 31 + o.hashCode();
        }
        System.err.println("lp10-diag elapsed-ms=" + elapsedMs);
        System.out.println("lp10 ring=" + ring
                + " rounds=" + rounds
                + " loader-ok=" + loaderOk
                + " payload-ok=" + payloadOk
                + " checksum=" + checksum);
    }

    static byte[] readClassBytes(String binaryName) throws Exception {
        String res = binaryName.replace('.', '/') + ".class";
        try (InputStream in =
                GciLp10LoaderSurvivorCopyProbe.class.getClassLoader().getResourceAsStream(res)) {
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
}
