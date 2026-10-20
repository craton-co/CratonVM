// JIT round 21 wave 1, lane typecheck: compiled type checks, correctness matrices and time rows.
//
// Targets (r20 handoff perf table): `instanceof` subclass / interface (R20PAllocTypecheck)
// 1583 / 1539 ns (207 / 243 after CRATONVM_JIT_TYPECHECK_DISPLAY_MISS) vs HotSpot 27 / 117;
// checkcast 165-386 vs 53-139; Class.isAssignableFrom 93-215 vs 2-16 (R20PNatAssignable; 5-6 after
// CRATONVM_JIT_CLASS_IS_ASSIGNABLE_INLINE); the opt-in getClass() == fold (lost 105 vs 25).
// This probe times ONE check per op (an op = one element of a 1024-element array), so its rows
// read directly as ns per check, and pins every answer the compiled arms give against the
// interpreter's first answer and against HotSpot.
//
// Correctness sections (each first computed cold -- the first executions are interpreted --
// then re-run 600-3000 times so the JIT compiles the checking method, and compared):
//   instanceof   17 objects x 25 targets (null, subclasses at depth 1-4, a final leaf, multi-level
//                interfaces I3 -> I2 -> I1, a hidden class, a lambda, primitive / reference /
//                nested / interface-typed arrays) as one bit row per object;
//   checkcast    the same matrix, and the ClassCastException message of every refused pair;
//   aastore      13 array component types x the 17 objects, and every ArrayStoreException message;
//   isInstance / cast / isAssignableFrom over 23 classes (primitive, array, interface, hidden);
//   getClass ==  the fold shapes (`==` / `!=` against a class constant; null raises NPE).
// Hidden / lambda class names carry an address: `/0x<hex>` is printed as `/0x?`.
//
// Run lines (build: javac -d tools/probes/out tools/probes/jit/r21/R21TypecheckRows.java; the
// hidden class is defined from the compiled R21TcHiddenImpl.class, so keep the output dir whole):
//   P=tools/probes/out
//   cratonvm -Xmx512m -cp $P R21TypecheckRows                        # jdk-only (launcher default)
//   cratonvm --compatible -Xmx512m -cp $P R21TypecheckRows           # both modes, same lines
//   java -Xmx512m -cp $P R21TypecheckRows                            # HotSpot 25
// Arms (r21 typecheck switches, both opt-in): unset vs
//   CRATONVM_JIT_SP_INSTANCEOF_DISPLAY_ARM=1   (single-pass instanceof display hit / miss arm)
//   CRATONVM_JIT_IR_AASTORE_DISPLAY_ARM=1      (IR aastore subclass / implementor store check)
//   CRATONVM_JIT_TYPECHECK_ARRAY_TARGETS=1     (IR instanceof / checkcast against `[LT;`)
//   CRATONVM_JIT_IR_CHECKCAST_AFTER_INSTANCEOF=1 (no cast after a proving instanceof: pattern-match)
//   CRATONVM_JIT_CHECKCAST_ENSURE_DISPLAY=1    (jit_checkcast publishes the receiver display)
//   CRATONVM_JIT_SP_INSTANCEOF_DISPLAY_ARM / CRATONVM_JIT_SP_AASTORE_DISPLAY_ARM /
//   CRATONVM_JIT_SP_TYPECHECK_ARRAY_TARGETS=1  (single-pass twins; pair with CRATONVM_C2_ACCEPT=never
//                                               to time the baseline bodies)
//   CRATONVM_JIT_IR_INSTANCEOF_HEADER_READ=1 [+ CRATONVM_JIT_IR_INSTANCEOF_OFF_CHAIN=1]
//                                              (the IR instanceof as a header read in the memory model)
// Correctness switch (default ON): CRATONVM_JIT_INSTANCEOF_FINAL_MISS_VERDICT=0 must leave every
// non-`time` line unchanged here (the probe has no Unmodifiable* stamp).
// and, for the getClass rows, CRATONVM_JIT_IR_GETCLASS_EXACT_FOLD=1 with and without
// CRATONVM_C2_ACCEPT=always (the acceptance-gate theory of the r20 loss).
// Engagement (one run, not for timing): CRATONVM_DBG_JITC=1, grep `display arm` -- `ir instanceof
// display arm: target=`, `sp instanceof display arm:`, `ir aastore display arm: emitted`,
// `getclass-exact-fold (ir)`.
//
// HotSpot expectation: every non-`time` line identical (diff them). Time rows under C2: 1-3 ns
// for the class rows, 1-5 for the interface rows (secondary-supers cache), ~1-2 for aastore,
// isInstance / isAssignableFrom / cast against a constant class ~1-3, getclass-eq ~1.
// `instanceof-lambda-iface` and the array-covariant rows are expected slow on CratonVM today
// (lambda proxies have no display; array targets always call the helper) -- see
// docs/internal/proposals/jit/jit-r21-typecheck-proposals.md.

import java.io.InputStream;
import java.io.Serializable;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.Arrays;
import java.util.Locale;

interface R21TcI1 {}

interface R21TcI2 extends R21TcI1 {}

interface R21TcI3 extends R21TcI2 {}

interface R21TcJ1 {}

interface R21TcFn extends R21TcI1 {
    int f();
}

abstract class R21TcBase implements R21TcJ1 {
    int v;
}

class R21TcSub extends R21TcBase implements R21TcI3 {}

class R21TcSubSub extends R21TcSub {}

final class R21TcLeaf extends R21TcSubSub {}

class R21TcOther implements R21TcI1 {}

class R21TcPlain {}

// Defined again, as a HIDDEN class, from its own class-file bytes (see `hiddenInstance`).
class R21TcHiddenImpl extends R21TcBase implements R21TcI2 {}

public class R21TypecheckRows {
    static final int REPS = 9;
    static final int OPS = 1024;
    static int sink;

    static final String[] OBJ_NAMES = {
        "null", "Sub", "SubSub", "Leaf", "Other", "Plain", "String", "Integer", "Hidden", "Lambda",
        "int[]", "String[]", "Sub[]", "SubSub[]", "Object[]", "Base[][]", "I2[]",
    };

    static Object[] objects(Object hidden) {
        R21TcFn lambda = () -> 42;
        return new Object[] {
            null, new R21TcSub(), new R21TcSubSub(), new R21TcLeaf(), new R21TcOther(), new R21TcPlain(),
            "s", Integer.valueOf(7), hidden, lambda,
            new int[1], new String[1], new R21TcSub[1], new R21TcSubSub[1], new Object[1],
            new R21TcBase[1][1], new R21TcI2[1],
        };
    }

    static final int TARGETS = 25;
    static final String TARGET_NAMES =
        "Object Base Sub SubSub Leaf Other I1 I2 I3 J1 Fn Runnable CharSequence Number Object[] Base[] "
            + "Sub[] I1[] I3[] int[] Cloneable Serializable Object[][] Base[][] Comparable";

    static boolean inst(int t, Object o) {
        switch (t) {
            case 0: return o instanceof Object;
            case 1: return o instanceof R21TcBase;
            case 2: return o instanceof R21TcSub;
            case 3: return o instanceof R21TcSubSub;
            case 4: return o instanceof R21TcLeaf;
            case 5: return o instanceof R21TcOther;
            case 6: return o instanceof R21TcI1;
            case 7: return o instanceof R21TcI2;
            case 8: return o instanceof R21TcI3;
            case 9: return o instanceof R21TcJ1;
            case 10: return o instanceof R21TcFn;
            case 11: return o instanceof Runnable;
            case 12: return o instanceof CharSequence;
            case 13: return o instanceof Number;
            case 14: return o instanceof Object[];
            case 15: return o instanceof R21TcBase[];
            case 16: return o instanceof R21TcSub[];
            case 17: return o instanceof R21TcI1[];
            case 18: return o instanceof R21TcI3[];
            case 19: return o instanceof int[];
            case 20: return o instanceof Cloneable;
            case 21: return o instanceof Serializable;
            case 22: return o instanceof Object[][];
            case 23: return o instanceof R21TcBase[][];
            case 24: return o instanceof Comparable;
            default: throw new IllegalArgumentException("target " + t);
        }
    }

    static Object cast(int t, Object o) {
        switch (t) {
            case 0: return (Object) o;
            case 1: return (R21TcBase) o;
            case 2: return (R21TcSub) o;
            case 3: return (R21TcSubSub) o;
            case 4: return (R21TcLeaf) o;
            case 5: return (R21TcOther) o;
            case 6: return (R21TcI1) o;
            case 7: return (R21TcI2) o;
            case 8: return (R21TcI3) o;
            case 9: return (R21TcJ1) o;
            case 10: return (R21TcFn) o;
            case 11: return (Runnable) o;
            case 12: return (CharSequence) o;
            case 13: return (Number) o;
            case 14: return (Object[]) o;
            case 15: return (R21TcBase[]) o;
            case 16: return (R21TcSub[]) o;
            case 17: return (R21TcI1[]) o;
            case 18: return (R21TcI3[]) o;
            case 19: return (int[]) o;
            case 20: return (Cloneable) o;
            case 21: return (Serializable) o;
            case 22: return (Object[][]) o;
            case 23: return (R21TcBase[][]) o;
            case 24: return (Comparable<?>) o;
            default: throw new IllegalArgumentException("target " + t);
        }
    }

    static final String ARRAY_NAMES = "Base[] Sub[] I1[] I2[] J1[] Object[] String[] Number[] Base[][] "
        + "Comparable[] Fn[] Object[][] SubSub[]";

    static Object[] arrays() {
        return new Object[][] {
            new R21TcBase[1], new R21TcSub[1], new R21TcI1[1], new R21TcI2[1], new R21TcJ1[1],
            new Object[1], new String[1], new Number[1], new R21TcBase[1][], new Comparable<?>[1],
            new R21TcFn[1], new Object[1][], new R21TcSubSub[1],
        };
    }

    // One aastore site for every array type: the component is known only at run time.
    static void store(Object[] a, Object v) {
        a[0] = v;
    }

    static String mask(String s) {
        return s == null ? "null" : s.replaceAll("/0x[0-9a-fA-F]+", "/0x?");
    }

    static Object hiddenInstance() throws Throwable {
        byte[] bytes;
        try (InputStream in = R21TypecheckRows.class.getResourceAsStream("/R21TcHiddenImpl.class")) {
            if (in == null) {
                throw new IllegalStateException("R21TcHiddenImpl.class not on the class path");
            }
            bytes = in.readAllBytes();
        }
        MethodHandles.Lookup hl = MethodHandles.lookup().defineHiddenClass(bytes, true);
        return hl.findConstructor(hl.lookupClass(), MethodType.methodType(void.class)).invoke();
    }

    // ---- instanceof --------------------------------------------------------------------

    static String instRow(Object o) {
        StringBuilder sb = new StringBuilder(TARGETS);
        for (int t = 0; t < TARGETS; t++) {
            sb.append(inst(t, o) ? '1' : '0');
        }
        return sb.toString();
    }

    // ---- checkcast ---------------------------------------------------------------------

    static String castRow(Object o, boolean messages, String name) {
        StringBuilder sb = new StringBuilder(TARGETS);
        String[] targets = TARGET_NAMES.split(" ");
        for (int t = 0; t < TARGETS; t++) {
            try {
                Object r = cast(t, o);
                sb.append(r == o ? 'Y' : '!');
            } catch (ClassCastException e) {
                sb.append('C');
                if (messages) {
                    System.out.println("cce " + name + " -> " + targets[t] + ": " + mask(e.getMessage()));
                }
            }
        }
        return sb.toString();
    }

    // ---- aastore -----------------------------------------------------------------------

    static String storeRow(Object[] arrays, Object v, boolean messages, String name) {
        StringBuilder sb = new StringBuilder(arrays.length);
        String[] names = ARRAY_NAMES.split(" ");
        for (int a = 0; a < arrays.length; a++) {
            Object[] arr = (Object[]) arrays[a];
            try {
                store(arr, v);
                sb.append(arr[0] == v ? 'Y' : '!');
            } catch (ArrayStoreException e) {
                sb.append('A');
                if (messages) {
                    System.out.println("ase " + name + " -> " + names[a] + ": " + mask(e.getMessage()));
                }
            }
            arr[0] = null;
        }
        return sb.toString();
    }

    // ---- Class.isInstance / cast / isAssignableFrom ------------------------------------

    static Class<?>[] classes(Object hidden) {
        return new Class<?>[] {
            Object.class, R21TcBase.class, R21TcSub.class, R21TcSubSub.class, R21TcLeaf.class,
            R21TcOther.class, R21TcI1.class, R21TcI2.class, R21TcI3.class, R21TcJ1.class, R21TcFn.class,
            hidden.getClass(), int.class, Integer.class, Number.class, Object[].class, R21TcBase[].class,
            R21TcSub[].class, R21TcI1[].class, int[].class, Cloneable.class, Serializable.class,
            Comparable.class,
        };
    }

    static String assignRow(Class<?> c, Class<?>[] all) {
        StringBuilder sb = new StringBuilder(all.length);
        for (Class<?> s : all) {
            sb.append(c.isAssignableFrom(s) ? '1' : '0');
        }
        return sb.toString();
    }

    static String isInstanceRow(Class<?> c, Object[] objs) {
        StringBuilder sb = new StringBuilder(objs.length);
        for (Object o : objs) {
            sb.append(c.isInstance(o) ? '1' : '0');
        }
        return sb.toString();
    }

    static String classCastRow(Class<?> c, Object[] objs, boolean messages) {
        StringBuilder sb = new StringBuilder(objs.length);
        for (int i = 0; i < objs.length; i++) {
            try {
                Object r = c.cast(objs[i]);
                sb.append(r == objs[i] ? 'Y' : '!');
            } catch (ClassCastException e) {
                sb.append('C');
                if (messages) {
                    System.out.println("class-cast " + mask(c.getName()) + " <- " + OBJ_NAMES[i] + ": "
                        + mask(e.getMessage()));
                }
            }
        }
        return sb.toString();
    }

    // ---- getClass() == fold shapes -------------------------------------------------------

    static int gc(int t, Object o) {
        switch (t) {
            case 0: return o.getClass() == R21TcSub.class ? 1 : 0;
            case 1: return o.getClass() != R21TcSub.class ? 1 : 0;
            case 2: return o.getClass() == R21TcLeaf.class ? 1 : 0;
            case 3: return o.getClass() == String.class ? 1 : 0;
            case 4: return o.getClass() == Integer.class ? 1 : 0;
            case 5: return o.getClass() == Object[].class ? 1 : 0;
            case 6: return o.getClass() != R21TcBase.class ? 1 : 0;
            default: throw new IllegalArgumentException("gc " + t);
        }
    }

    static String gcRow(Object o) {
        StringBuilder sb = new StringBuilder(7);
        for (int t = 0; t < 7; t++) {
            try {
                sb.append((char) ('0' + gc(t, o)));
            } catch (NullPointerException e) {
                sb.append('N');
            }
        }
        return sb.toString();
    }

    // ---- warm comparison -----------------------------------------------------------------

    interface Row {
        String of(int i);
    }

    static void warm(String section, int n, String[] cold, int warmReps, Row row) {
        for (int r = 0; r < warmReps; r++) {
            for (int i = 0; i < n; i++) {
                String w = row.of(i);
                if (!w.equals(cold[i])) {
                    System.out.println("warm " + section + " MISMATCH rep=" + (r >= warmReps - 1 ? "last" : "early")
                        + " row=" + i + " cold=" + cold[i] + " warm=" + w);
                    return;
                }
            }
        }
        System.out.println("warm " + section + " ok");
    }

    // ---- timing kernels (one check per element) --------------------------------------------

    static int kInstBase(Object[] a, int reps) {
        int n = 0;
        for (int r = 0; r < reps; r++) {
            for (int i = 0; i < a.length; i++) {
                if (a[i] instanceof R21TcBase) {
                    n++;
                }
            }
        }
        return n;
    }

    static int kInstSub(Object[] a, int reps) {
        int n = 0;
        for (int r = 0; r < reps; r++) {
            for (int i = 0; i < a.length; i++) {
                if (a[i] instanceof R21TcSub) {
                    n++;
                }
            }
        }
        return n;
    }

    static int kInstI1(Object[] a, int reps) {
        int n = 0;
        for (int r = 0; r < reps; r++) {
            for (int i = 0; i < a.length; i++) {
                if (a[i] instanceof R21TcI1) {
                    n++;
                }
            }
        }
        return n;
    }

    static int kInstFn(Object[] a, int reps) {
        int n = 0;
        for (int r = 0; r < reps; r++) {
            for (int i = 0; i < a.length; i++) {
                if (a[i] instanceof R21TcFn) {
                    n++;
                }
            }
        }
        return n;
    }

    static int kInstBaseArray(Object[] a, int reps) {
        int n = 0;
        for (int r = 0; r < reps; r++) {
            for (int i = 0; i < a.length; i++) {
                if (a[i] instanceof R21TcBase[]) {
                    n++;
                }
            }
        }
        return n;
    }

    static int kPatternMatch(Object[] a, int reps) {
        int n = 0;
        for (int r = 0; r < reps; r++) {
            for (int i = 0; i < a.length; i++) {
                if (a[i] instanceof R21TcBase b) {
                    n += b.v + 1;
                }
            }
        }
        return n;
    }

    static int kCastBase(Object[] a, int reps) {
        int n = 0;
        for (int r = 0; r < reps; r++) {
            for (int i = 0; i < a.length; i++) {
                n += ((R21TcBase) a[i]).v;
            }
        }
        return n;
    }

    static int kCastI2(Object[] a, int reps) {
        int n = 0;
        for (int r = 0; r < reps; r++) {
            for (int i = 0; i < a.length; i++) {
                R21TcI2 x = (R21TcI2) a[i];
                if (x != null) {
                    n++;
                }
            }
        }
        return n;
    }

    static int kStore(Object[] dst, Object[] src, int reps) {
        for (int r = 0; r < reps; r++) {
            for (int i = 0; i < src.length; i++) {
                dst[i] = src[i];
            }
        }
        return dst.length;
    }

    static int kIsInstance(Class<?> c, Object[] a, int reps) {
        int n = 0;
        for (int r = 0; r < reps; r++) {
            for (int i = 0; i < a.length; i++) {
                if (c.isInstance(a[i])) {
                    n++;
                }
            }
        }
        return n;
    }

    static int kIsAssignable(Class<?> c, Class<?>[] a, int reps) {
        int n = 0;
        for (int r = 0; r < reps; r++) {
            for (int i = 0; i < a.length; i++) {
                if (c.isAssignableFrom(a[i])) {
                    n++;
                }
            }
        }
        return n;
    }

    static int kClassCast(Object[] a, int reps) {
        int n = 0;
        for (int r = 0; r < reps; r++) {
            for (int i = 0; i < a.length; i++) {
                n += R21TcBase.class.cast(a[i]).v;
            }
        }
        return n;
    }

    static int kGetClassEq(Object[] a, int reps) {
        int n = 0;
        for (int r = 0; r < reps; r++) {
            for (int i = 0; i < a.length; i++) {
                if (a[i].getClass() == R21TcSub.class) {
                    n++;
                }
            }
        }
        return n;
    }

    interface Kernel {
        int run(int reps);
    }

    static void time(String row, int reps, Kernel k) {
        int check = 0;
        for (int w = 0; w < 5; w++) {
            check = k.run(reps);
        }
        double[] ns = new double[REPS];
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            check = k.run(reps);
            long t1 = System.nanoTime();
            ns[r] = (t1 - t0) / ((double) reps * OPS);
        }
        sink += check;
        Arrays.sort(ns);
        System.out.println("check " + row + " " + check);
        System.out.printf(Locale.ROOT, "time %s %.1f ns/op%n", row, ns[REPS / 2]);
    }

    static Object[] fill(Object... kinds) {
        Object[] a = new Object[OPS];
        for (int i = 0; i < OPS; i++) {
            a[i] = kinds[i % kinds.length];
        }
        return a;
    }

    public static void main(String[] args) throws Throwable {
        Object hidden = hiddenInstance();
        Object[] objs = objects(hidden);
        int n = objs.length;
        System.out.println("hidden isHidden=" + hidden.getClass().isHidden() + " name=" + mask(hidden.getClass().getName()));
        System.out.println("targets " + TARGET_NAMES);

        // instanceof
        String[] inst = new String[n];
        for (int i = 0; i < n; i++) {
            inst[i] = instRow(objs[i]);
            System.out.println("instanceof " + OBJ_NAMES[i] + " " + inst[i]);
        }
        warm("instanceof", n, inst, 3000, i -> instRow(objs[i]));

        // checkcast
        String[] casts = new String[n];
        for (int i = 0; i < n; i++) {
            casts[i] = castRow(objs[i], true, OBJ_NAMES[i]);
            System.out.println("checkcast " + OBJ_NAMES[i] + " " + casts[i]);
        }
        warm("checkcast", n, casts, 600, i -> castRow(objs[i], false, null));

        // aastore
        Object[] arrs = arrays();
        System.out.println("arrays " + ARRAY_NAMES);
        String[] stores = new String[n];
        for (int i = 0; i < n; i++) {
            stores[i] = storeRow(arrs, objs[i], true, OBJ_NAMES[i]);
            System.out.println("aastore " + OBJ_NAMES[i] + " " + stores[i]);
        }
        warm("aastore", n, stores, 600, i -> storeRow(arrs, objs[i], false, null));

        // Class.isAssignableFrom / isInstance / cast
        Class<?>[] cls = classes(hidden);
        String[] assign = new String[cls.length];
        String[] isInst = new String[cls.length];
        String[] ccast = new String[cls.length];
        for (int c = 0; c < cls.length; c++) {
            assign[c] = assignRow(cls[c], cls);
            isInst[c] = isInstanceRow(cls[c], objs);
            ccast[c] = classCastRow(cls[c], objs, true);
            String name = mask(cls[c].getName());
            System.out.println("isAssignableFrom " + name + " " + assign[c]);
            System.out.println("isInstance " + name + " " + isInst[c]);
            System.out.println("Class.cast " + name + " " + ccast[c]);
        }
        warm("isAssignableFrom", cls.length, assign, 2000, c -> assignRow(cls[c], cls));
        warm("isInstance", cls.length, isInst, 2000, c -> isInstanceRow(cls[c], objs));
        warm("Class.cast", cls.length, ccast, 400, c -> classCastRow(cls[c], objs, false));
        try {
            R21TcBase.class.isAssignableFrom(null);
            System.out.println("isAssignableFrom(null) no exception");
        } catch (NullPointerException e) {
            System.out.println("isAssignableFrom(null) NullPointerException");
        }
        System.out.println("isInstance(null) " + R21TcBase.class.isInstance(null)
            + " cast(null) " + R21TcBase.class.cast(null));

        // getClass() ==
        String[] gcs = new String[n];
        for (int i = 0; i < n; i++) {
            gcs[i] = gcRow(objs[i]);
            System.out.println("getclass " + OBJ_NAMES[i] + " " + gcs[i]);
        }
        warm("getclass", n, gcs, 3000, i -> gcRow(objs[i]));

        // ---- time rows ----
        Object sub = new R21TcSub();
        Object subSub = new R21TcSubSub();
        Object leaf = new R21TcLeaf();
        Object other = new R21TcOther();
        Object plain = new R21TcPlain();
        Object[] exactSub = fill(sub);
        Object[] deepHits = fill(subSub, leaf, hidden);
        Object[] misses = fill(other, plain, "s");
        Object[] mixed = fill(sub, other, leaf, plain, subSub, "s");
        Object[] ifaceHits = fill(sub, subSub, leaf, other);
        Object[] ifaceMiss = fill(plain, "s", Integer.valueOf(3), hidden);
        R21TcFn lam = () -> 1;
        Object[] lambdas = fill(lam);
        Object[] subArrays = fill(new R21TcSub[1], new R21TcLeaf[1]);
        Object[] i2s = fill(sub, subSub, leaf, hidden);
        Object[] subs = fill(sub);
        Object[] deepStore = fill(subSub, leaf);
        R21TcSub[] subDst = new R21TcSub[OPS];
        R21TcBase[] baseDst = new R21TcBase[OPS];
        R21TcI1[] i1Dst = new R21TcI1[OPS];
        Object[] objDst = new Object[OPS];
        Class<?>[] clsMix = new Class<?>[OPS];
        Class<?>[] clsPick = {R21TcSub.class, R21TcLeaf.class, R21TcOther.class, String.class};
        for (int i = 0; i < OPS; i++) {
            clsMix[i] = clsPick[i % clsPick.length];
        }
        int reps = 400;
        time("instanceof-class-exact", reps, r -> kInstSub(exactSub, r));
        time("instanceof-subclass-hit", reps, r -> kInstBase(deepHits, r));
        time("instanceof-subclass-miss", reps, r -> kInstBase(misses, r));
        time("instanceof-subclass-mixed", reps, r -> kInstBase(mixed, r));
        time("instanceof-iface-hit", reps, r -> kInstI1(ifaceHits, r));
        time("instanceof-iface-miss", reps, r -> kInstI1(ifaceMiss, r));
        time("instanceof-lambda-iface", reps, r -> kInstFn(lambdas, r));
        time("instanceof-array-covariant", reps, r -> kInstBaseArray(subArrays, r));
        time("checkcast-subclass", reps, r -> kCastBase(deepHits, r));
        time("pattern-match-subclass", reps, r -> kPatternMatch(mixed, r));
        time("checkcast-iface", reps, r -> kCastI2(i2s, r));
        time("aastore-exact", reps, r -> kStore(subDst, subs, r));
        time("aastore-subclass", reps, r -> kStore(baseDst, deepStore, r));
        time("aastore-iface", reps, r -> kStore(i1Dst, ifaceHits, r));
        time("aastore-object", reps, r -> kStore(objDst, mixed, r));
        time("isinstance-subclass", reps, r -> kIsInstance(R21TcBase.class, mixed, r));
        time("isassignable-class", reps, r -> kIsAssignable(R21TcBase.class, clsMix, r));
        time("class-cast-subclass", reps, r -> kClassCast(deepHits, r));
        time("getclass-eq", reps, r -> kGetClassEq(mixed, r));
        System.out.println("done " + (sink != 0));
    }
}
