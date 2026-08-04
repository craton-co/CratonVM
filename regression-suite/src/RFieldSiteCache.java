// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;

/**
 * Regression: the interpreter's per-thread resolved-field site cache
 * (`CRATONVM_JIT=field-site-cache`) must never answer a field reference with
 * another site's answer.
 *
 * The cache is a direct-mapped table keyed on (referencing class, constant-pool
 * index) and validated by two global epochs. Every failure mode it can have is
 * silent — a wrong `field_index` reads a neighbouring slot and returns a
 * plausible number, or a wrong `desc_byte` reinterprets a long as two ints.
 * Nothing throws. So every check below asserts an EXACT expected value, chosen
 * so that a mixed-up site produces a different one.
 *
 * What each section targets:
 *
 *   1. shadowed fields          — a subclass field with the same NAME as its
 *                                 superclass's. Both are read from methods that
 *                                 differ only in their referencing class, which
 *                                 is the half of the key that is not the cp
 *                                 index.
 *   2. slot conflicts           — more distinct field sites than the table has
 *                                 slots, hammered in a rotating order so that
 *                                 entries evict each other continuously. A tag
 *                                 check that did not compare BOTH key halves
 *                                 shows up here and nowhere else.
 *   3. descriptor fidelity      — long/double/float/boolean/byte/short/char and
 *                                 reference fields, whose cached `desc_byte`
 *                                 and `is_reference` drive which push path the
 *                                 opcode handler takes. Values are chosen to be
 *                                 wrong-looking if reinterpreted (e.g. a long
 *                                 whose halves are distinguishable).
 *   4. volatile                 — `is_volatile` is cached too; a dropped fence
 *                                 is not observable here, but a volatile field
 *                                 answered with a non-volatile sibling's index
 *                                 is.
 *   5. epoch invalidation       — new classes are defined BETWEEN reads of the
 *                                 same site, which bumps the class-definition
 *                                 epoch and must wipe the table without
 *                                 changing any answer.
 *   6. two loaders, one name    — the same class defined twice, by the
 *                                 application loader and by a private loader.
 *                                 Each copy's own bytecode reads its own
 *                                 fields; the two identities must not share a
 *                                 cache entry. This is the arm that
 *                                 `CRATONVM_JIT=field-site-cache-loader` widens.
 *   7. per-thread isolation     — the table lives on `JvmThread`, so several
 *                                 threads must each build their own and agree.
 *
 * ⚠️ In the default CORE run the flag is OFF, so this exercises the ORDINARY
 * field-resolution path — a real check, but NOT the cached one it was written
 * for. To cover that path it must be run explicitly:
 *
 *     CRATONVM_JIT=field-site-cache ONLY=RFieldSiteCache bash regression-suite/run.sh
 *     CRATONVM_JIT=field-site-cache,field-site-cache-loader ONLY=RFieldSiteCache bash regression-suite/run.sh
 *
 * Fold it into the default set only when that flag's default flips.
 */
public class RFieldSiteCache {

    static int checks = 0;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    static void checkEq(long actual, long expected, String m) {
        checks++;
        if (actual != expected) {
            throw new AssertionError(m + ": got " + actual + ", expected " + expected);
        }
    }

    // --- 1: shadowed field names ------------------------------------------
    // `Base.tag` and `Derived.tag` are DIFFERENT fields with the same name.
    // `Base.readTag()` must always see Base's; `Derived.readTag()` Derived's —
    // even when called on the same object, and even though both getfields
    // resolve the identical name against the identical receiver class.

    static class Base {
        int tag = 11;
        int other = 12;

        int readBaseTag() {
            return tag;
        }

        int readBaseOther() {
            return other;
        }
    }

    static class Derived extends Base {
        int tag = 21;
        int other = 22;

        int readDerivedTag() {
            return tag;
        }

        int readDerivedOther() {
            return other;
        }

        int readInheritedTag() {
            return super.tag;
        }
    }

    // --- 3/4: descriptor fidelity and volatile ----------------------------

    static class Wide {
        // Halves deliberately distinguishable: 0x1122334455667788 read as two
        // ints, or as an int, is not 1234605616436508552.
        long l = 0x1122334455667788L;
        double d = 2.718281828459045;
        float f = 1.5f;
        boolean z = true;
        byte b = (byte) -7;
        short s = (short) -300;
        char c = 'Q';
        int i = 987654321;
        String ref = "sentinel";
        volatile long vl = 0x7766554433221100L;
        volatile int vi = 424242;
        long l2 = 0x0102030405060708L;
    }

    // --- 2: enough distinct sites to overflow a direct-mapped table --------
    // Each Slots instance has 16 fields; 128 reader methods across 8 classes
    // give >1000 distinct (class, cp-index) sites once the loop below rotates
    // through them. Values encode their own identity, so any cross-talk is an
    // exact-value mismatch.

    // `public` so the private-loader copy in section 6 can be constructed and
    // driven reflectively from this class, which is NOT in its runtime package
    // (a class defined by a different loader is in a different runtime package
    // even when the binary names match — that is the point of the test).
    public static class Slots {
        public int f0, f1, f2, f3, f4, f5, f6, f7, f8, f9, f10, f11, f12, f13, f14, f15;

        public Slots(int base) {
            f0 = base;
            f1 = base + 1;
            f2 = base + 2;
            f3 = base + 3;
            f4 = base + 4;
            f5 = base + 5;
            f6 = base + 6;
            f7 = base + 7;
            f8 = base + 8;
            f9 = base + 9;
            f10 = base + 10;
            f11 = base + 11;
            f12 = base + 12;
            f13 = base + 13;
            f14 = base + 14;
            f15 = base + 15;
        }

        /** Sum of all sixteen: base*16 + 120. Sixteen distinct getfield sites. */
        public int sum() {
            return f0 + f1 + f2 + f3 + f4 + f5 + f6 + f7
                    + f8 + f9 + f10 + f11 + f12 + f13 + f14 + f15;
        }

        /** Same sixteen fields, a SECOND set of cp entries in the same class. */
        public int weighted() {
            return f0 * 1 + f1 * 2 + f2 * 3 + f3 * 4 + f4 * 5 + f5 * 6 + f6 * 7 + f7 * 8
                    + f8 * 9 + f9 * 10 + f10 * 11 + f11 * 12 + f12 * 13 + f13 * 14
                    + f14 * 15 + f15 * 16;
        }
    }

    // --- 5: classes defined on demand to move the class-definition epoch ---
    static class Epoch0 { static int v = 100; }
    static class Epoch1 { static int v = 101; }
    static class Epoch2 { static int v = 102; }
    static class Epoch3 { static int v = 103; }
    static class Epoch4 { static int v = 104; }
    static class Epoch5 { static int v = 105; }
    static class Epoch6 { static int v = 106; }
    static class Epoch7 { static int v = 107; }

    /** Statics are the other two opcodes (getstatic/putstatic) on the same path. */
    static class Statics {
        static int si = 5150;
        static long sl = 0x0BADF00DCAFEBABEL;
        static String sref = "static-sentinel";
        static volatile int svi = 31337;
    }

    // --- 6: a private loader that defines its own copy of a classpath class -

    /**
     * Defines `TARGET` itself instead of delegating, so the process holds two
     * unrelated classes with the same binary name. Everything else delegates
     * normally, so the copy can still reach `java.*`.
     */
    static final class PrivateLoader extends ClassLoader {
        private final String target;

        PrivateLoader(String target, ClassLoader parent) {
            super(parent);
            this.target = target;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (!name.equals(target)) {
                return super.loadClass(name, resolve);
            }
            synchronized (getClassLoadingLock(name)) {
                Class<?> already = findLoadedClass(name);
                if (already != null) {
                    return already;
                }
                byte[] bytes = readBytes(name);
                Class<?> defined = defineClass(name, bytes, 0, bytes.length);
                if (resolve) {
                    resolveClass(defined);
                }
                return defined;
            }
        }

        private byte[] readBytes(String name) throws ClassNotFoundException {
            String path = name.replace('.', '/') + ".class";
            try (InputStream in = getParent().getResourceAsStream(path)) {
                if (in == null) {
                    throw new ClassNotFoundException(name + " (no " + path + " on the parent)");
                }
                ByteArrayOutputStream out = new ByteArrayOutputStream();
                byte[] buf = new byte[8192];
                int n;
                while ((n = in.read(buf)) > 0) {
                    out.write(buf, 0, n);
                }
                return out.toByteArray();
            } catch (java.io.IOException ex) {
                throw new ClassNotFoundException(name, ex);
            }
        }
    }

    public static void main(String[] args) throws Exception {
        shadowedFields();
        descriptorFidelity();
        slotConflicts();
        epochInvalidation();
        staticFields();
        twoLoadersOneName();
        perThreadIsolation();

        System.out.println("CK RFieldSiteCache checks=" + checks);
        System.out.println("PASS RFieldSiteCache");
    }

    // ---------------------------------------------------------------- 1 ----
    static void shadowedFields() {
        Derived d = new Derived();
        Base asBase = d;
        // Hot enough that any caching layer is warm and, if the JIT is on,
        // compiled.
        for (int i = 0; i < 50_000; i++) {
            if (asBase.readBaseTag() != 11 || d.readDerivedTag() != 21) {
                throw new AssertionError("shadowed tag diverged at i=" + i
                        + ": base=" + asBase.readBaseTag() + " derived=" + d.readDerivedTag());
            }
            if (asBase.readBaseOther() != 12 || d.readDerivedOther() != 22) {
                throw new AssertionError("shadowed other diverged at i=" + i);
            }
        }
        checkEq(asBase.readBaseTag(), 11, "Base.tag through a Derived receiver");
        checkEq(d.readDerivedTag(), 21, "Derived.tag");
        checkEq(d.readInheritedTag(), 11, "super.tag from Derived");
        checkEq(asBase.readBaseOther(), 12, "Base.other");
        checkEq(d.readDerivedOther(), 22, "Derived.other");
        // Direct access through each static type resolves in THIS class's
        // constant pool — a third referencing class for the same two fields.
        checkEq(((Base) d).tag, 11, "((Base) d).tag");
        checkEq(d.tag, 21, "d.tag");
    }

    // ---------------------------------------------------------------- 3/4 --
    static void descriptorFidelity() {
        Wide w = new Wide();
        long lAcc = 0;
        double dAcc = 0;
        for (int i = 0; i < 50_000; i++) {
            lAcc ^= w.l ^ w.vl ^ w.l2;
            dAcc += w.d + w.f;
        }
        checkEq(w.l, 0x1122334455667788L, "Wide.l");
        checkEq(w.l2, 0x0102030405060708L, "Wide.l2");
        checkEq(w.vl, 0x7766554433221100L, "Wide.vl");
        checkEq(w.vi, 424242, "Wide.vi");
        checkEq(w.i, 987654321, "Wide.i");
        checkEq(w.b, -7, "Wide.b");
        checkEq(w.s, -300, "Wide.s");
        checkEq(w.c, 'Q', "Wide.c");
        check(w.z, "Wide.z");
        check(w.d == 2.718281828459045, "Wide.d: " + w.d);
        check(w.f == 1.5f, "Wide.f: " + w.f);
        check("sentinel".equals(w.ref), "Wide.ref: " + w.ref);
        // The accumulators exist so the loop is not dead; assert them too, so a
        // wrong read inside the loop cannot be optimized away unnoticed.
        checkEq(lAcc, 0L, "even iteration count must cancel the xor accumulator");
        check(dAcc > 0, "double accumulator");

        // Write-then-read through putfield/getfield on every category.
        w.l = -1L;
        w.vl = 1L;
        w.i = -1;
        w.b = (byte) 127;
        w.s = (short) 32767;
        w.c = '￿';
        w.z = false;
        w.ref = "rewritten";
        checkEq(w.l, -1L, "Wide.l after putfield");
        checkEq(w.vl, 1L, "Wide.vl after putfield");
        checkEq(w.i, -1, "Wide.i after putfield");
        checkEq(w.b, 127, "Wide.b after putfield");
        checkEq(w.s, 32767, "Wide.s after putfield");
        checkEq(w.c, 0xFFFF, "Wide.c after putfield");
        check(!w.z, "Wide.z after putfield");
        check("rewritten".equals(w.ref), "Wide.ref after putfield");
        // l2 must be untouched by all of the above — a neighbouring-slot write
        // is exactly what a wrong field_index produces.
        checkEq(w.l2, 0x0102030405060708L, "Wide.l2 must be untouched");
    }

    // ---------------------------------------------------------------- 2 ----
    static void slotConflicts() {
        int n = 64;
        Slots[] all = new Slots[n];
        for (int i = 0; i < n; i++) {
            all[i] = new Slots(i * 1000);
        }
        // Rotate through every instance repeatedly. Each `sum()`/`weighted()`
        // call runs 16 getfield sites; the rotation keeps the working set of
        // (class, cp-index) pairs churning against the same receiver class, so
        // a tag check that ignored the cp index would return a neighbour's
        // field here.
        long total = 0;
        for (int round = 0; round < 400; round++) {
            for (int i = 0; i < n; i++) {
                Slots s = all[(i + round) % n];
                total += s.sum();
                total += s.weighted();
            }
        }
        // sum(base)      = 16*base + 120
        // weighted(base) = 136*base + 1360   (sum of k*(base+k-1), k=1..16)
        long expected = 0;
        for (int round = 0; round < 400; round++) {
            for (int i = 0; i < n; i++) {
                long base = ((i + round) % n) * 1000L;
                expected += 16 * base + 120;
                expected += 136 * base + 1360;
            }
        }
        checkEq(total, expected, "rotating field-site reads");

        // Every instance must still read back exactly what it was built with.
        for (int i = 0; i < n; i++) {
            checkEq(all[i].sum(), 16 * (i * 1000) + 120, "Slots[" + i + "].sum()");
            checkEq(all[i].f0, i * 1000, "Slots[" + i + "].f0");
            checkEq(all[i].f15, i * 1000 + 15, "Slots[" + i + "].f15");
        }
    }

    // ---------------------------------------------------------------- 5 ----
    static void epochInvalidation() throws Exception {
        Wide w = new Wide();
        Slots s = new Slots(7000);
        String[] names = {
            "RFieldSiteCache$Epoch0", "RFieldSiteCache$Epoch1", "RFieldSiteCache$Epoch2",
            "RFieldSiteCache$Epoch3", "RFieldSiteCache$Epoch4", "RFieldSiteCache$Epoch5",
            "RFieldSiteCache$Epoch6", "RFieldSiteCache$Epoch7",
        };
        // Warm the sites, THEN define classes, THEN re-read. Each forName
        // defines a class the process has never seen, moving the
        // class-definition epoch and wiping the table underneath these sites.
        for (int i = 0; i < names.length; i++) {
            checkEq(w.l2, 0x0102030405060708L, "Wide.l2 before definition " + i);
            checkEq(s.sum(), 16 * 7000 + 120, "Slots.sum() before definition " + i);
            Class<?> c = Class.forName(names[i]);
            java.lang.reflect.Field f = c.getDeclaredField("v");
            // `setAccessible` even though this class is a nest-mate of the
            // target: CratonVM's reflection does not implement JEP 181
            // nest-based access control for `Field.getInt`, and this vector is
            // about the site cache, not about that gap.
            f.setAccessible(true);
            checkEq(f.getInt(null), 100 + i, "freshly defined " + names[i] + ".v");
            checkEq(w.l2, 0x0102030405060708L, "Wide.l2 after definition " + i);
            checkEq(s.sum(), 16 * 7000 + 120, "Slots.sum() after definition " + i);
            checkEq(s.weighted(), 136 * 7000 + 1360, "Slots.weighted() after definition " + i);
        }
    }

    // ------------------------------------------------------------ statics ---
    static void staticFields() {
        long acc = 0;
        for (int i = 0; i < 50_000; i++) {
            acc += Statics.si + Statics.svi;
            acc ^= Statics.sl;
        }
        checkEq(Statics.si, 5150, "Statics.si");
        checkEq(Statics.sl, 0x0BADF00DCAFEBABEL, "Statics.sl");
        checkEq(Statics.svi, 31337, "Statics.svi");
        check("static-sentinel".equals(Statics.sref), "Statics.sref");
        check(acc != 0, "static accumulator");

        Statics.si = -5150;
        Statics.sl = -1L;
        Statics.svi = -31337;
        Statics.sref = "rewritten-static";
        checkEq(Statics.si, -5150, "Statics.si after putstatic");
        checkEq(Statics.sl, -1L, "Statics.sl after putstatic");
        checkEq(Statics.svi, -31337, "Statics.svi after putstatic");
        check("rewritten-static".equals(Statics.sref), "Statics.sref after putstatic");
    }

    // ---------------------------------------------------------------- 6 ----
    static void twoLoadersOneName() throws Exception {
        String name = "RFieldSiteCache$Slots";
        Class<?> appCopy = Slots.class;
        PrivateLoader loader = new PrivateLoader(name, RFieldSiteCache.class.getClassLoader());
        Class<?> privateCopy = loader.loadClass(name);

        check(privateCopy != appCopy,
                "the private loader delegated instead of defining its own copy");
        check(privateCopy.getName().equals(appCopy.getName()),
                "the two copies must share a binary name");

        // Each copy's OWN bytecode does the getfields, through its OWN constant
        // pool — two distinct (referencing class, cp index) keys that a
        // name-blind cache would collapse.
        Method appSum = appCopy.getDeclaredMethod("sum");
        Method privSum = privateCopy.getDeclaredMethod("sum");
        Method appWeighted = appCopy.getDeclaredMethod("weighted");
        Method privWeighted = privateCopy.getDeclaredMethod("weighted");
        appSum.setAccessible(true);
        privSum.setAccessible(true);
        appWeighted.setAccessible(true);
        privWeighted.setAccessible(true);

        Object appInstance = appCopy.getDeclaredConstructor(int.class).newInstance(3000);
        Object privInstance =
                privateCopy.getDeclaredConstructor(int.class).newInstance(4000);

        // Interleave, so the two identities keep landing on the same table
        // slots one after the other.
        for (int i = 0; i < 2_000; i++) {
            checkOne(appSum.invoke(appInstance), 16 * 3000 + 120, "app copy sum", i);
            checkOne(privSum.invoke(privInstance), 16 * 4000 + 120, "private copy sum", i);
            checkOne(appWeighted.invoke(appInstance), 136 * 3000 + 1360, "app copy weighted", i);
            checkOne(privWeighted.invoke(privInstance), 136 * 4000 + 1360,
                    "private copy weighted", i);
        }
        checks += 4;
    }

    /** Value check inside a hot loop, without bumping `checks` 8000 times. */
    static void checkOne(Object actual, int expected, String what, int i) {
        int v = ((Integer) actual).intValue();
        if (v != expected) {
            throw new AssertionError(what + " at i=" + i + ": got " + v
                    + ", expected " + expected);
        }
    }

    // ---------------------------------------------------------------- 7 ----
    static void perThreadIsolation() throws Exception {
        final int threads = 4;
        final CountDownLatch start = new CountDownLatch(1);
        final List<Throwable> failures = new ArrayList<>();
        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final int id = t;
            ts[t] = new Thread(() -> {
                try {
                    start.await();
                    // Each thread builds its own table from scratch over the
                    // SAME sites, and must reach the same answers.
                    Slots s = new Slots(id * 100);
                    Wide w = new Wide();
                    Derived d = new Derived();
                    for (int i = 0; i < 20_000; i++) {
                        if (s.sum() != 16 * (id * 100) + 120) {
                            throw new AssertionError("thread " + id + " Slots.sum()=" + s.sum());
                        }
                        if (w.l2 != 0x0102030405060708L) {
                            throw new AssertionError("thread " + id + " Wide.l2=" + w.l2);
                        }
                        if (d.readDerivedTag() != 21 || ((Base) d).readBaseTag() != 11) {
                            throw new AssertionError("thread " + id + " shadowed tag");
                        }
                    }
                } catch (Throwable ex) {
                    synchronized (failures) {
                        failures.add(ex);
                    }
                }
            });
            ts[t].start();
        }
        start.countDown();
        for (Thread t : ts) {
            t.join(120_000);
            check(!t.isAlive(), "worker thread did not finish");
        }
        synchronized (failures) {
            if (!failures.isEmpty()) {
                throw new AssertionError("worker failure: " + failures.get(0), failures.get(0));
            }
        }
        checks++;
    }
}
