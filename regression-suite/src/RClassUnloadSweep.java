// Class unloading on the `System.gc()` path, reduced to one file.
//
// `TestDefaultInstanceManager.testClassUnloading` has failed four times, each
// time diagnosed against a Tomcat checkout, three compiled JSPs and a 17-second
// run — and each time closed without a scheduled vector, which is how the
// third fix's repaired predicate was disarmed by a one-line default flip with
// nothing to notice. The 08-01 writeup says so in its own words.
//
// This is the same mechanism with nothing else in it: define a class in a
// throwaway loader, use it, drop every strong reference, collect, and ask
// whether the `WeakReference<Class>` cleared. That is exactly what Tomcat's
// annotation cache does — it is keyed on `Class` through a weak map, and the
// assertion that failed (`expected:<8> but was:<9>`) is that map still holding
// an entry for a class whose loader is gone.
//
// It is a suite vector rather than a probe because the suite diffs it against
// HotSpot: "the reference cleared" is a claim about a real JVM's behaviour, and
// asserting it in isolation would pin CratonVM to whatever CratonVM does.
//
// Deliberately NOT asserting a count of collections. HotSpot does not promise
// to unload on any particular `System.gc()`, so the loop below gives both VMs
// the same generous budget and prints only the final boolean; a vector that
// depended on unloading happening on cycle 1 would be measuring scheduling.

import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.lang.ref.WeakReference;

public class RClassUnloadSweep {

    /** The class that must become unloadable. Nothing references it after the
     *  block below returns — no instance, no `Class` object, no loader. */
    public static class Payload {
        public int touched;

        public int work(int n) {
            // A body, so the class is genuinely used rather than merely defined:
            // an unused class can be unloaded by paths a used one cannot.
            int acc = 0;
            for (int i = 0; i < n; i++) {
                acc += i * 31;
            }
            touched = acc;
            return acc;
        }
    }

    /** Defines exactly one name itself and delegates everything else, so the
     *  payload's `Class` and this loader die together. */
    static final class Throwaway extends ClassLoader {
        private final String target;
        private final byte[] bytes;

        Throwaway(String target, byte[] bytes, ClassLoader parent) {
            super(parent);
            this.target = target;
            this.bytes = bytes;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (name.equals(target)) {
                Class<?> c = findLoadedClass(name);
                if (c == null) {
                    c = defineClass(name, bytes, 0, bytes.length);
                }
                if (resolve) {
                    resolveClass(c);
                }
                return c;
            }
            return super.loadClass(name, resolve);
        }
    }

    static byte[] readClassBytes(String binaryName) throws Exception {
        String res = binaryName.replace('.', '/') + ".class";
        try (InputStream in = RClassUnloadSweep.class.getClassLoader().getResourceAsStream(res)) {
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

    /** Load, use and drop the payload. Everything it touches is confined to
     *  this frame, so on return the only reference left is the weak one. */
    static WeakReference<Class<?>> defineUseAndDrop(byte[] bytes) throws Exception {
        String name = RClassUnloadSweep.class.getName() + "$Payload";
        Throwaway loader = new Throwaway(name, bytes, RClassUnloadSweep.class.getClassLoader());
        Class<?> c = loader.loadClass(name);
        Object instance = c.getDeclaredConstructor().newInstance();
        c.getMethod("work", int.class).invoke(instance, 1000);
        return new WeakReference<>(c);
    }

    /** Allocation between collections: a young generation with nothing in it
     *  can be swept by a path that never runs on a real workload's. */
    static volatile Object churnSink;

    static void churn() {
        for (int i = 0; i < 20000; i++) {
            churnSink = new byte[128];
        }
        churnSink = null;
    }

    /** Bare `new Object()` from a JIT-compiled loop, which is a DIFFERENT
     *  allocation shape from `churn()` above and the only one this vector's
     *  own defect leaves behind.
     *
     *  An empty object — `ClassId(0)`, kind `Object`, `num_slots = 0`, hash not
     *  yet minted — is HEADER_SIZE all-zero bytes, indistinguishable by header
     *  bytes from reclaimed memory. The generational young walk used to read a
     *  run of them as evidence it had left the object grid and unwind every
     *  reclaim decision taken since the last anchor; the loader this vector
     *  drops is one of the decisions thrown away, so its span is never zeroed
     *  and the sweep's `word0 != 0` liveness proxy answers "live". That is the
     *  mechanism behind `TestDefaultInstanceManager`'s fourth recurrence and
     *  behind a `BatchingConnectionTest` young-GC livelock, and it is worth
     *  stating exactly why the ORIGINAL `churn()` cannot see it: a
     *  `byte[128]`'s header carries its length, so it is never all-zero.
     *
     *  JIT-compiled on purpose. The interpreter mints the identity hash
     *  eagerly, so an interpreted `new Object()` has a non-zero header and is
     *  the wrong shape — which is why the defect this pins was invisible under
     *  `--nojit`. The warm-up below is what gets the loop compiled.
     *
     *  Measured with `CRATONVM_GC_NO_EMPTY_OBJECT_RUN=1` (the in-binary A/B
     *  opt-out for the recovery), `-XX:+UseGenerationalGC`: without this method
     *  the vector prints `unloaded=true` in BOTH arms — it could not see the
     *  bug at all — and with it, `true` with the recovery on and `false` with
     *  it off. */
    static void emptyChurn(int n) {
        Object keep = null;
        for (int i = 0; i < n; i++) {
            Object o = new Object();
            // Keep one per 1024 so the loop cannot be optimised away wholesale,
            // while leaving the other 1023 immediately dead — which is the run
            // of empty objects the walk has to step over.
            if ((i & 1023) == 0) {
                keep = o;
            }
        }
        churnSink = keep;
        churnSink = null;
    }

    /** The whole probe, minus the printing, so the Generational twin
     *  (`RClassUnloadSweepGen`) runs byte-for-byte the same thing under a
     *  different collector instead of a second copy that can drift. */
    static boolean unloadedUnderChurn() throws Exception {
        // Compile `emptyChurn` BEFORE the payload dies: the shape this vector
        // exists for is the JIT's zero-header allocation, and a loop that only
        // gets hot after the interesting collection contributes nothing.
        for (int w = 0; w < 40; w++) {
            emptyChurn(20000);
        }
        byte[] bytes = readClassBytes(RClassUnloadSweep.class.getName() + "$Payload");
        WeakReference<Class<?>> ref = defineUseAndDrop(bytes);

        boolean cleared = false;
        for (int round = 0; round < 12 && !cleared; round++) {
            churn();
            emptyChurn(200000);
            System.gc();
            cleared = ref.get() == null;
        }
        return cleared;
    }

    public static void main(String[] args) throws Exception {
        boolean cleared = unloadedUnderChurn();

        // The one observable, and it must be on a `CK ` line. run.sh:218
        // filters both VMs' output through `grep -aE '^(PASS|CK) '` before
        // diffing, so the comment that used to stand here — "printed as a bare
        // boolean so the suite's HotSpot diff is the assertion" — described a
        // line the harness deleted. What survived was the constant
        // `PASS RClassUnloadSweep`, and a CratonVM that never unloaded a class
        // at all passed.
        //
        // DIFF-ONLY ON PURPOSE, unlike the other vectors repaired alongside it.
        // Whether a weak reference has actually been cleared is a GC-policy
        // outcome, not a language guarantee, so a local `check(cleared)` would
        // turn a legitimate collector configuration into a red. HotSpot clears
        // it within the 12 rounds here (measured, 2/2), so a CratonVM printing
        // `false` is a real divergence and the diff is the right instrument.
        System.out.println("CK RClassUnloadSweep payload.class.unloaded=" + cleared);
        System.out.println("PASS RClassUnloadSweep");
    }
}
