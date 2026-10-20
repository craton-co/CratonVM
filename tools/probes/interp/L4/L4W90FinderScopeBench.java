// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i4 wave 90, lane L4: the method finder's per-class and
// per-handle cost, one row at a time so the `CRATONVM_DBG=mh-finder-census`
// census (with `CRATONVM_DBG_INTERP_FRAMES=1`) can be read per row. Every
// iteration defines a fresh class (untimed); the rows are
// `L4W88FindConstructorSplitBench`'s, plus the A/B rows of
// `CRATONVM_MH_METHOD_FINDER_MINT_INIT_REFILE` (default off):
//
//   find-fresh                findConstructor on a class just defined
//   find-inited               findConstructor on an initialized class
//   invoke-first              first invoke() of a constructor handle found
//                             untimed (the invoke initializes the class)
//   invoke-first-inited       the same on an initialized class
//   findStatic-find-only      findStatic on a fresh class
//   findStatic-invoke-first   first invoke() of a static handle found untimed
//                             (initializes, then ensureInitialized/updateForm)
//   findStatic-warm-class     A/B ROW: findStatic + invoke() untimed, then the
//                             same findStatic timed. Switch off: the memo line
//                             was filed while the class was uninitialized, so
//                             the JDK body runs again. Switch on: the line is
//                             re-filed with the handle's barrier-free form and
//                             minted.
//   findStatic-warm-uninvoked CONTROL: findStatic untimed, class initialized
//                             by Class.forName (handle never invoked), then the
//                             same findStatic timed. Both arms run the body
//                             (the handle still holds the barrier form).
//   findStatic-warm-third     CONTROL: findStatic + invoke() + findStatic
//                             untimed, the third findStatic timed: a memo hit
//                             in both arms.
//   find-predefined           (wave 106) findConstructor on classes defined
//                             in one batch before the timed loop: no define
//                             between two timed calls
//   find-predefined-churn     the same, with one unrelated class defined
//                             (untimed, another loader) before each call
//   findStatic-predefined     findStatic on batch-defined classes
//   find-predefined-interned  (wave 107) find-predefined, with the class's
//                             constructor-handle type ()Fs<N> interned
//                             (MethodType.methodType(c), untimed) before the
//                             timed call: removes the one per-class
//                             allocation chain of the body by reading
//
// Each warm row also checks the timed answer (outside the timed region): a
// handle distinct from the first, of type ()int, whose invoke() answers 7,
// whose toString() is "MethodHandle()int" and which revealDirect cracks as
// REF_invokeStatic "seven" of the class.
//
// Output: "TIME <name> med <us> min <us> us/op" (median first; filtered by
// the battery and A/B scripts as every "TIME " line) and "checksum <name> <n>"
// (deterministic; in HOTSPOT_EXPECTED below for a run with no arguments).
// Before wave 107 the timing lines started with "row", so an A/B that kept only
// "TIME " lines read nothing.
// Args, in any order: a number = iterations per row (default 300); "all" or a
// comma-separated list of row names = the rows to run (default all); "time" is
// accepted and ignored (timings always print). So `300 find-fresh`,
// `find-fresh 300` and `find-fresh,find-predefined` all work. An unknown row
// name is an error (exit 2), so a typo cannot silently print nothing.
//
// Run: javac -d . L4W90FinderScopeBench.java
//      java -cp . L4W90FinderScopeBench                       (HotSpot reference)
//      cratonvm --jdk-only -cp . L4W90FinderScopeBench
//      CRATONVM_MH_METHOD_FINDER_MINT_INIT_REFILE=1 cratonvm --jdk-only -cp . L4W90FinderScopeBench
//      CRATONVM_DBG=mh-finder-census CRATONVM_DBG_INTERP_FRAMES=1 \
//          cratonvm --jdk-only -cp . L4W90FinderScopeBench 300 find-fresh
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// checksum find-fresh 300
// checksum find-inited 300
// checksum invoke-first 300
// checksum invoke-first-inited 300
// checksum findStatic-find-only 300
// checksum findStatic-invoke-first 300
// checksum findStatic-warm-class 300
// checksum findStatic-warm-uninvoked 300
// checksum findStatic-warm-third 300
// checksum find-predefined 300
// checksum find-predefined-churn 300
// checksum findStatic-predefined 300
// checksum find-predefined-interned 300
// HOTSPOT_EXPECTED_END
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandleInfo;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.Arrays;
import java.util.HashSet;
import java.util.Locale;
import java.util.Set;

public class L4W90FinderScopeBench {
    static final MethodTypeDesc VOID = MethodTypeDesc.of(ConstantDescs.CD_void);
    static final MethodType CTOR = MethodType.methodType(void.class);
    static final MethodType INT = MethodType.methodType(int.class);
    static final MethodHandles.Lookup LOOKUP = MethodHandles.lookup();

    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W90FinderScopeBench.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static int classes;

    /** A public class with a public no-arg constructor and `public static int seven()`. */
    static byte[] build(String name) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody(ConstantDescs.INIT_NAME, VOID, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME, VOID)
                            .return_());
            cb.withMethodBody("seven", MethodTypeDesc.of(ConstantDescs.CD_int),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> code.bipush(7).ireturn());
        });
    }

    static double median(long[] xs) {
        long[] c = xs.clone();
        Arrays.sort(c);
        int m = c.length / 2;
        return c.length % 2 == 1 ? c[m] : (c[m - 1] + c[m]) / 2.0;
    }

    static double min(long[] xs) {
        long best = Long.MAX_VALUE;
        for (long x : xs) best = Math.min(best, x);
        return best;
    }

    interface Prep {
        Object prepare(Class<?> c) throws Throwable;
    }

    interface Timed {
        Object run(Class<?> c, Object prepared) throws Throwable;
    }

    interface Check {
        /** Answers 1 when the timed step produced what it should (untimed). */
        int check(Class<?> c, Object prepared, Object answer) throws Throwable;
    }

    static Set<String> selected;

    static final java.util.List<String> ROWS = Arrays.asList("find-fresh", "find-inited", "invoke-first",
            "invoke-first-inited", "findStatic-find-only", "findStatic-invoke-first", "findStatic-warm-class",
            "findStatic-warm-uninvoked", "findStatic-warm-third", "find-predefined", "find-predefined-churn",
            "findStatic-predefined", "find-predefined-interned");

    static void printTime(String name, long[] t) {
        System.out.printf(Locale.ROOT, "TIME %s med %.3f min %.3f us/op%n", name, median(t) / 1000.0,
                min(t) / 1000.0);
    }

    static void row(String name, int n, boolean print, boolean init, Prep prep, Timed timed, Check check)
            throws Throwable {
        if (selected != null && !selected.contains(name)) {
            return;
        }
        Loader loader = new Loader();
        long[] t = new long[n];
        int ok = 0;
        for (int i = 0; i < n; i++) {
            String cls = "Fs" + (classes++);
            Class<?> c = loader.define(cls, build(cls));
            if (init) {
                Class.forName(cls, true, loader);
            }
            Object prepared = prep == null ? null : prep.prepare(c);
            long t0 = System.nanoTime();
            Object answer = timed.run(c, prepared);
            t[i] = System.nanoTime() - t0;
            ok += check.check(c, prepared, answer);
        }
        if (print) {
            printTime(name, t);
            System.out.println("checksum " + name + " " + ok);
        }
    }

    /**
     * Interpreter round i5 wave 106 (lane L4): the same timed step on classes
     * defined in one batch BEFORE the timed loop (untimed), so no define runs
     * between two timed calls. With {@code churn}, one unrelated class is
     * defined (untimed, in another loader) before each timed call. Each class
     * is still one no finder has seen.
     */
    static void rowPredefined(String name, int n, boolean print, boolean churn, Timed timed, Check check)
            throws Throwable {
        rowPredefined(name, n, print, churn, null, timed, check);
    }

    static void rowPredefined(String name, int n, boolean print, boolean churn, Prep pre, Timed timed,
            Check check) throws Throwable {
        if (selected != null && !selected.contains(name)) {
            return;
        }
        Loader loader = new Loader();
        Loader other = new Loader();
        Class<?>[] cs = new Class<?>[n];
        for (int i = 0; i < n; i++) {
            String cls = "Fs" + (classes++);
            cs[i] = loader.define(cls, build(cls));
        }
        long[] t = new long[n];
        int ok = 0;
        for (int i = 0; i < n; i++) {
            if (churn) {
                String cls = "Fs" + (classes++);
                other.define(cls, build(cls));
            }
            Object prepared = pre == null ? null : pre.prepare(cs[i]);
            long t0 = System.nanoTime();
            Object answer = timed.run(cs[i], prepared);
            t[i] = System.nanoTime() - t0;
            ok += check.check(cs[i], prepared, answer);
        }
        if (print) {
            printTime(name, t);
            System.out.println("checksum " + name + " " + ok);
        }
    }

    /** The timed answer of a warm findStatic row: a fresh, working, crackable handle. */
    static int checkStatic(Class<?> c, Object prepared, Object answer) throws Throwable {
        MethodHandle h = (MethodHandle) answer;
        if (h == prepared || h.type() != INT || (int) h.invoke() != 7) {
            return 0;
        }
        if (!"MethodHandle()int".equals(h.toString())) {
            return 0;
        }
        MethodHandleInfo info = LOOKUP.revealDirect(h);
        return info.getReferenceKind() == MethodHandleInfo.REF_invokeStatic
                && info.getName().equals("seven")
                && info.getDeclaringClass() == c
                && info.getMethodType() == INT ? 1 : 0;
    }

    public static void main(String[] args) throws Throwable {
        int n = 300;
        for (String a : args) {
            if (a.equals("time") || a.equals("all") || a.isEmpty()) {
                continue;
            }
            if (a.chars().allMatch(Character::isDigit)) {
                n = Integer.parseInt(a);
                continue;
            }
            if (selected == null) {
                selected = new HashSet<>();
            }
            for (String r : a.split(",")) {
                if (!r.isEmpty()) {
                    selected.add(r);
                }
            }
        }
        if (selected != null) {
            for (String r : selected) {
                if (!ROWS.contains(r)) {
                    System.out.println("unknown row " + r + "; rows: " + ROWS);
                    System.exit(2);
                }
            }
        }
        Prep findCtor = c -> LOOKUP.findConstructor(c, CTOR);
        Prep findStatic = c -> LOOKUP.findStatic(c, "seven", INT);
        Timed findCtorTimed = (c, p) -> LOOKUP.findConstructor(c, CTOR);
        Timed findStaticTimed = (c, p) -> LOOKUP.findStatic(c, "seven", INT);
        Timed invokeCtor = (c, p) -> ((MethodHandle) p).invoke();
        Timed invokeStatic = (c, p) -> (Object) (int) ((MethodHandle) p).invoke();
        Check isCtorHandle = (c, p, a) -> ((MethodHandle) a).type().parameterCount() == 0
                && ((MethodHandle) a).type().returnType() == c ? 1 : 0;
        Check isInstance = (c, p, a) -> a != null && a.getClass() == c ? 1 : 0;
        Check isStaticHandle = (c, p, a) -> ((MethodHandle) a).type() == INT ? 1 : 0;
        Check isSeven = (c, p, a) -> Integer.valueOf(7).equals(a) ? 1 : 0;
        for (int pass = 0; pass < 2; pass++) {
            boolean print = pass == 1;
            int count = print ? n : 40;
            row("find-fresh", count, print, false, null, findCtorTimed, isCtorHandle);
            row("find-inited", count, print, true, null, findCtorTimed, isCtorHandle);
            row("invoke-first", count, print, false, findCtor, invokeCtor, isInstance);
            row("invoke-first-inited", count, print, true, findCtor, invokeCtor, isInstance);
            row("findStatic-find-only", count, print, false, null, findStaticTimed, isStaticHandle);
            row("findStatic-invoke-first", count, print, false, findStatic, invokeStatic, isSeven);
            row("findStatic-warm-class", count, print, false, c -> {
                MethodHandle h = LOOKUP.findStatic(c, "seven", INT);
                h.invoke();
                return h;
            }, findStaticTimed, L4W90FinderScopeBench::checkStatic);
            row("findStatic-warm-uninvoked", count, print, false, c -> {
                MethodHandle h = LOOKUP.findStatic(c, "seven", INT);
                Class.forName(c.getName(), true, c.getClassLoader());
                return h;
            }, findStaticTimed, L4W90FinderScopeBench::checkStatic);
            row("findStatic-warm-third", count, print, false, c -> {
                MethodHandle h = LOOKUP.findStatic(c, "seven", INT);
                h.invoke();
                return LOOKUP.findStatic(c, "seven", INT);
            }, findStaticTimed, L4W90FinderScopeBench::checkStatic);
            rowPredefined("find-predefined", count, print, false, findCtorTimed, isCtorHandle);
            rowPredefined("find-predefined-churn", count, print, true, findCtorTimed, isCtorHandle);
            rowPredefined("findStatic-predefined", count, print, false, findStaticTimed, isStaticHandle);
            rowPredefined("find-predefined-interned", count, print, false, c -> MethodType.methodType(c),
                    findCtorTimed, isCtorHandle);
        }
    }
}
