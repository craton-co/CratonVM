import org.springframework.util.ConcurrentReferenceHashMap;

import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;

/**
 * Repro for docs/known-issues/springboot/repeatablecontainers-method-cache-classcastexception.md.
 *
 * Mirrors RepeatableContainers$StandardRepeatableContainers exactly:
 * a Map<Class<?>, Object> where computeIfAbsent either caches a real
 * java.lang.reflect.Method or a private static final Object NONE sentinel,
 * and callers do `result != NONE ? (Method) result : null`.
 *
 * Drives it with many distinct Class keys (to force Segment resize /
 * restructuring), repeated lookups (cache-hit path), and optionally
 * multiple threads, checking on every read that the returned object is
 * EXACTLY what was cached for that key (identity-checked against a
 * per-key expected value), not merely "castable".
 */
public class CrhmRepro {
    private static final Object NONE = new Object();

    // Map from Class -> either a Method (for "repeatable" keys) or NONE.
    private static final ConcurrentReferenceHashMap<Class<?>, Object> cache =
            new ConcurrentReferenceHashMap<>();

    // Ground truth: what SHOULD be cached for each key, decided once.
    private static final java.util.Map<Class<?>, Object> expected =
            new java.util.concurrent.ConcurrentHashMap<>();

    static Object compute(Class<?> key) {
        // Deterministic 50/50 split keyed on identity hash so repeated calls
        // for the same key are consistent (mirrors computeRepeatedAnnotationsMethod's
        // determinism for a given annotation type).
        Object result;
        if ((System.identityHashCode(key) & 1) == 0) {
            try {
                result = Object.class.getMethod("toString");
            } catch (NoSuchMethodException e) {
                throw new RuntimeException(e);
            }
        } else {
            result = NONE;
        }
        return result;
    }

    static Object lookup(Class<?> key) {
        Object result = cache.computeIfAbsent(key, CrhmRepro::compute);
        return (result != NONE) ? result : null;
    }

    public static void main(String[] args) throws Exception {
        int numKeys = args.length > 0 ? Integer.parseInt(args[0]) : 4000;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 20;
        int threads = args.length > 2 ? Integer.parseInt(args[2]) : 4;

        // Build a large pool of DISTINCT Class objects using dynamically
        // generated classes (so we have thousands of distinct keys, forcing
        // the ConcurrentReferenceHashMap through its Segment resize/purge
        // machinery, same as a huge real Spring app touching many annotation
        // types) via a trivial custom ClassLoader defining synthetic classes.
        System.out.println("DEBUG numKeys=" + numKeys + " rounds=" + rounds + " threads=" + threads);
        List<Class<?>> keys = new ArrayList<>(numKeys);
        for (int i = 0; i < numKeys; i++) {
            keys.add(makeClass(i));
        }
        System.out.println("DEBUG keys.size()=" + keys.size());
        for (Class<?> k : keys) {
            expected.put(k, compute(k));
        }
        System.out.println("DEBUG expected.size()=" + expected.size());

        AtomicInteger mismatches = new AtomicInteger(0);
        AtomicInteger cces = new AtomicInteger(0);
        AtomicLong ops = new AtomicLong(0);

        for (int round = 0; round < rounds; round++) {
            Thread[] ts = new Thread[threads];
            CountDownLatch latch = new CountDownLatch(threads);
            for (int t = 0; t < threads; t++) {
                final int tid = t;
                ts[t] = new Thread(() -> {
                    try {
                        for (int i = tid; i < keys.size(); i += threads) {
                            Class<?> key = keys.get(i);
                            Object exp = expected.get(key);
                            try {
                                Object got = cache.computeIfAbsent(key, CrhmRepro::compute);
                                ops.incrementAndGet();
                                if (got != NONE) {
                                    // Mirror the real bug site: an explicit cast that
                                    // throws ClassCastException if `got` is neither
                                    // NONE nor a Method.
                                    Method m = (Method) got;
                                    if (exp == NONE || !m.equals(exp)) {
                                        mismatches.incrementAndGet();
                                        System.out.println("MISMATCH key=" + key.getName()
                                                + " expected=" + exp + " got=" + got);
                                    }
                                } else if (exp != NONE) {
                                    mismatches.incrementAndGet();
                                    System.out.println("MISMATCH(NONE) key=" + key.getName()
                                            + " expected=" + exp);
                                }
                            } catch (ClassCastException cce) {
                                cces.incrementAndGet();
                                System.out.println("CCE key=" + key.getName() + " : " + cce);
                            }
                        }
                    } finally {
                        latch.countDown();
                    }
                });
            }
            for (Thread th : ts) th.start();
            latch.await();
            if (round % 5 == 0) {
                System.out.println("round " + round + " ops=" + ops.get()
                        + " mismatches=" + mismatches.get() + " cces=" + cces.get());
                System.gc();
            }
        }

        System.out.println("DONE ops=" + ops.get() + " mismatches=" + mismatches.get()
                + " cces=" + cces.get() + " cacheSize=" + cache.size());
        if (mismatches.get() > 0 || cces.get() > 0) {
            System.out.println("REPRO: BUG REPRODUCED");
            System.exit(1);
        } else {
            System.out.println("REPRO: no bug observed");
        }
    }

    static final class DefiningLoader extends ClassLoader {
        DefiningLoader() { super(CrhmRepro.class.getClassLoader()); }
        Class<?> define(String name, byte[] bytecode) {
            return defineClass(name, bytecode, 0, bytecode.length);
        }
    }

    // Minimal per-i synthetic class generation via bytecode, so each key is a
    // genuinely distinct java.lang.Class (distinct identity hash, distinct
    // classloader-scoped ClassId on CratonVM) rather than reusing a handful
    // of JDK classes. A FRESH ClassLoader per class avoids name collisions
    // and keeps each key's identity independent of any single loader's state.
    static Class<?> makeClass(int i) throws Exception {
        String name = "Synthetic" + i;
        byte[] bytecode = buildClass(name);
        return new DefiningLoader().define(name, bytecode);
    }

    static byte[] buildClass(String name) {
        // Hand-rolled minimal class file: public final class <name> extends Object {}
        // Avoids pulling in ASM as a dependency for this throwaway repro.
        String internalName = name;
        java.io.ByteArrayOutputStream bos = new java.io.ByteArrayOutputStream();
        try (java.io.DataOutputStream out = new java.io.DataOutputStream(bos)) {
            out.writeInt(0xCAFEBABE);
            out.writeShort(0); // minor
            out.writeShort(61); // major = Java 17 (fits any JDK we run on; hidden-class API allows any <= current)
            // Constant pool: #1 Class this, #2 Utf8 name, #3 Class Object, #4 Utf8 java/lang/Object
            out.writeShort(5); // constant_pool_count = 5 (indices 1..4 used)
            // #1 Utf8 internalName
            out.writeByte(1); writeUtf8(out, internalName);
            // #2 Class -> #1
            out.writeByte(7); out.writeShort(1);
            // #3 Utf8 java/lang/Object
            out.writeByte(1); writeUtf8(out, "java/lang/Object");
            // #4 Class -> #3
            out.writeByte(7); out.writeShort(3);
            out.writeShort(0x0031); // access: PUBLIC | FINAL | SUPER
            out.writeShort(2); // this_class = #2
            out.writeShort(4); // super_class = #4
            out.writeShort(0); // interfaces_count
            out.writeShort(0); // fields_count
            out.writeShort(0); // methods_count
            out.writeShort(0); // attributes_count
        } catch (Exception e) {
            throw new RuntimeException(e);
        }
        return bos.toByteArray();
    }

    static void writeUtf8(java.io.DataOutputStream out, String s) throws Exception {
        byte[] b = s.getBytes("UTF-8");
        out.writeShort(b.length);
        out.write(b);
    }
}
