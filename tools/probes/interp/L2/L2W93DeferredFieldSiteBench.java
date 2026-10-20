// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench -- Interpreter round i4 wave 93, lane L2. `CRATONVM_JIT_DEFERRED_FIELD_SITES`
// (default OFF) with rows that REACH the deferred case.
//
// Why `L2W92DeferredFieldSiteBench` measured 0.95-1.00: its hot methods live in
// an APPLICATION-loader class, and the compile doors' threadless field peek
// (`field_access.rs` `resolve_field_ref`) LOADS an application-loader owner
// from the class path (`load_class_concurrent`) when the method compiles. The
// cold site resolved at compile time, so the method compiled with or without
// the switch and nothing was deferred. A field site a door cannot resolve is
// one whose owner no built-in loader can find: here, classes a user loader
// defines from bytes it alone holds (an optional dependency, a plugin, a
// generated class). Every class below is generated with java.lang.classfile
// and defined by `Loader` (child-first for its own names, parent = the
// application loader), so the cold owners `l2d.ColdS` / `l2d.ColdBox` are
// unknown to the flat store until the cold branch runs.
//
// Rows (NOT deterministic; medians of 7 interleaved reps):
//   static-cold  `l2d.HotS.applyAsInt`, cold `putstatic` + `getstatic` of
//                `l2d.ColdS` (method-entry door), ns/call
//   field-cold   `l2d.HotF.applyAsInt`, cold `new` + `putfield` + `getfield`
//                of `l2d.ColdBox` (method-entry door), ns/call
//   control      `l2d.HotC.applyAsInt`, the same body, its cold owner a
//                class-path class loaded up front (the compiled ceiling), ns/call
//   osr-cold     `l2d.Osr.applyAsInt(n, 0)`: ONE call of a 2M-iteration loop
//                whose body holds a cold `getstatic l2d.ColdS.BASE`, on a
//                FRESH loader per rep (a fresh method: only the OSR door can
//                compile it inside the call), ns per 1000 iterations
//   osr-control  `l2d.OsrC`, the same loop, its cold owner the loaded
//                class-path class, ns per 1000 iterations
// Expected: switch OFF -> static-cold / field-cold / osr-cold at interpreter
// speed, the controls well below; switch ON -> each within noise of its control.
//
// A/B (one binary, one variable; interleave the arms, take medians):
//   $J/javac -d out tools/probes/interp/L2/L2W93DeferredFieldSiteBench.java
//   $B --java-home $J/.. -cp out L2W93DeferredFieldSiteBench
//   CRATONVM_JIT_DEFERRED_FIELD_SITES=1 $B --java-home $J/.. -cp out L2W93DeferredFieldSiteBench
//   (each with --compatible too)
//   $J/java -cp out L2W93DeferredFieldSiteBench    (reference)
//
// Census (proves the case; run each arm once, --jdk-only):
//   CRATONVM_DBG_JITC=1 $B --java-home $J/.. -cp out L2W93DeferredFieldSiteBench 2>&1 \
//     | grep -E 'resolver-bail site=(static_)?field_resolve|osr field site unresolved|deferred-field-sites' | sort | uniq -c
// switch OFF prints
//   [cratonvm-jitc] resolver-bail site=static_field_resolve l2d/HotS.applyAsInt(II)I
//   [cratonvm-jitc] resolver-bail site=field_resolve l2d/HotF.applyAsInt(II)I   (if the `new` site did not refuse first)
//   [cratonvm-jitc] osr field site unresolved at pc=<p> static cp=<n>: refused l2d/Osr.applyAsInt(II)I
// and no line for l2d/HotC or l2d/OsrC; switch ON prints instead
//   [cratonvm-jitc] deferred-field-sites l2d/HotS.applyAsInt(II)I: 2 site(s) at pcs [..]
//   [cratonvm-jitc] deferred-field-sites l2d/HotF.applyAsInt(II)I: 2 site(s) at pcs [..]
//   [cratonvm-jitc] osr field site unresolved at pc=<p> static cp=<n>: deferred l2d/Osr.applyAsInt(II)I
//   [cratonvm-jitc] osr deferred-field-sites l2d/Osr.applyAsInt(II)I: 1 site(s) at pcs [<p>]
// (the OSR lines up to once per rep: each rep's class is fresh; a door memo
// keyed by name may print fewer). The hot loop then
// runs compiled: the static-cold / osr-cold rows drop to their controls.
//
// The last lines are deterministic:
//   check-static 1105
//   check-field 1113
//   check-osr true
//   check-osr-control true
public class L2W93DeferredFieldSiteBench {
    static final int CALLS = 200_000;
    static final int REPS = 7;
    static final int OSR_ITERS = 2_000_000;

    static final java.lang.constant.ClassDesc I = java.lang.constant.ConstantDescs.CD_int;
    static final java.lang.constant.MethodTypeDesc INT_INT_INT =
            java.lang.constant.MethodTypeDesc.of(I, I, I);
    static final java.lang.constant.ClassDesc OP =
            java.lang.constant.ClassDesc.of("java.util.function.IntBinaryOperator");
    static final java.lang.constant.ClassDesc COLD_S = java.lang.constant.ClassDesc.of("l2d.ColdS");
    static final java.lang.constant.ClassDesc COLD_BOX = java.lang.constant.ClassDesc.of("l2d.ColdBox");
    static final java.lang.constant.ClassDesc WARM =
            java.lang.constant.ClassDesc.of("L2W93DeferredFieldSiteBench$Warm");
    static final int PUBLIC = java.lang.classfile.ClassFile.ACC_PUBLIC;
    static final int STATIC = java.lang.classfile.ClassFile.ACC_STATIC;

    /// The control's cold owner: a class-path class, loaded before warm-up.
    public static class Warm {
        public static int seen;
        public static int BASE;

        static {
            BASE = 1100;
        }
    }

    static void ctor(java.lang.classfile.ClassBuilder clb) {
        clb.withMethodBody(java.lang.constant.ConstantDescs.INIT_NAME,
                java.lang.constant.ConstantDescs.MTD_void, PUBLIC,
                cb -> cb.aload(0)
                        .invokespecial(java.lang.constant.ConstantDescs.CD_Object,
                                java.lang.constant.ConstantDescs.INIT_NAME,
                                java.lang.constant.ConstantDescs.MTD_void)
                        .return_());
    }

    /// `public static int seen, BASE` with `BASE = 1100` in `<clinit>`.
    static byte[] coldStatic() {
        return java.lang.classfile.ClassFile.of().build(COLD_S, clb -> clb
                .withFlags(PUBLIC)
                .withField("seen", I, PUBLIC | STATIC)
                .withField("BASE", I, PUBLIC | STATIC)
                .withMethodBody(java.lang.constant.ConstantDescs.CLASS_INIT_NAME,
                        java.lang.constant.ConstantDescs.MTD_void, STATIC,
                        cb -> cb.sipush(1100).putstatic(COLD_S, "BASE", I).return_()));
    }

    /// `public int v` and a public no-arg constructor.
    static byte[] coldBox() {
        return java.lang.classfile.ClassFile.of().build(COLD_BOX, clb -> {
            clb.withFlags(PUBLIC).withField("v", I, PUBLIC);
            ctor(clb);
        });
    }

    /// `applyAsInt(i, cold)`: sixteen mixing rounds, then (cold == 1 only) the
    /// cold site: `kind` 'S' / 'C' a `putstatic seen` + `getstatic BASE` of
    /// `owner`, 'F' a `new ColdBox` + `putfield v` + `getfield v`.
    static byte[] hot(String name, char kind, java.lang.constant.ClassDesc owner) {
        java.lang.constant.ClassDesc self = java.lang.constant.ClassDesc.of(name);
        return java.lang.classfile.ClassFile.of().build(self, clb -> {
            clb.withFlags(PUBLIC).withInterfaceSymbols(OP);
            ctor(clb);
            clb.withMethodBody("applyAsInt", INT_INT_INT, PUBLIC, cb -> {
                java.lang.classfile.Label hotExit = cb.newLabel();
                cb.iload(1).istore(3);
                for (int k = 0; k < 16; k++) {
                    cb.iload(3).bipush(31).imul().bipush(k).iadd().istore(3);
                    cb.iload(3).iload(3).bipush(7).iushr().ixor().istore(3);
                }
                cb.iload(2).iconst_1().if_icmpne(hotExit);
                if (kind == 'F') {
                    cb.new_(COLD_BOX).dup()
                            .invokespecial(COLD_BOX, java.lang.constant.ConstantDescs.INIT_NAME,
                                    java.lang.constant.ConstantDescs.MTD_void)
                            .astore(4);
                    cb.aload(4).sipush(1100).iload(1).iadd().putfield(COLD_BOX, "v", I);
                    cb.aload(4).getfield(COLD_BOX, "v", I).ireturn();
                } else {
                    cb.iload(3).putstatic(owner, "seen", I);
                    cb.getstatic(owner, "BASE", I).iload(1).iadd().ireturn();
                }
                cb.labelBinding(hotExit);
                cb.iload(3).ireturn();
            });
        });
    }

    /// `applyAsInt(n, cold)`: `n` mixing rounds in ONE loop whose body, when
    /// cold == 1, adds `owner.BASE` (a cold `getstatic` inside the loop).
    static byte[] osr(String name, java.lang.constant.ClassDesc owner) {
        java.lang.constant.ClassDesc self = java.lang.constant.ClassDesc.of(name);
        return java.lang.classfile.ClassFile.of().build(self, clb -> {
            clb.withFlags(PUBLIC).withInterfaceSymbols(OP);
            ctor(clb);
            clb.withMethodBody("applyAsInt", INT_INT_INT, PUBLIC, cb -> {
                java.lang.classfile.Label head = cb.newLabel();
                java.lang.classfile.Label skip = cb.newLabel();
                java.lang.classfile.Label end = cb.newLabel();
                cb.iconst_0().istore(3);
                cb.iconst_0().istore(4);
                cb.labelBinding(head);
                cb.iload(4).iload(1).if_icmpge(end);
                cb.iload(3).bipush(31).imul().iload(4).iadd().istore(3);
                cb.iload(3).iload(3).bipush(7).iushr().ixor().istore(3);
                cb.iload(2).iconst_1().if_icmpne(skip);
                cb.getstatic(owner, "BASE", I).iload(3).iadd().istore(3);
                cb.labelBinding(skip);
                cb.iinc(4, 1);
                cb.goto_(head);
                cb.labelBinding(end);
                cb.iload(3).ireturn();
            });
        });
    }

    /// The Java twin of `osr`'s loop, for the check lines.
    static int osrRef(int n, int cold) {
        int a = 0;
        for (int k = 0; k < n; k++) {
            a = a * 31 + k;
            a ^= a >>> 7;
            if (cold == 1) {
                a = 1100 + a;
            }
        }
        return a;
    }

    static final java.util.Map<String, byte[]> BYTES = new java.util.HashMap<>();

    static {
        BYTES.put("l2d.ColdS", coldStatic());
        BYTES.put("l2d.ColdBox", coldBox());
        BYTES.put("l2d.HotS", hot("l2d.HotS", 'S', COLD_S));
        BYTES.put("l2d.HotF", hot("l2d.HotF", 'F', COLD_BOX));
        BYTES.put("l2d.HotC", hot("l2d.HotC", 'C', WARM));
        BYTES.put("l2d.Osr", osr("l2d.Osr", COLD_S));
        BYTES.put("l2d.OsrC", osr("l2d.OsrC", WARM));
    }

    /// Child-first for the generated names, parent = the application loader.
    static final class Loader extends ClassLoader {
        Loader() {
            super(L2W93DeferredFieldSiteBench.class.getClassLoader());
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                byte[] b = BYTES.get(name);
                if (b == null) {
                    return super.loadClass(name, resolve);
                }
                return defineClass(name, b, 0, b.length);
            }
        }
    }

    static java.util.function.IntBinaryOperator make(ClassLoader loader, String name)
            throws Exception {
        return (java.util.function.IntBinaryOperator) loader.loadClass(name)
                .getConstructor().newInstance();
    }

    static long timeStatic(java.util.function.IntBinaryOperator op) {
        int sink = 0;
        long t0 = System.nanoTime();
        for (int i = 0; i < CALLS; i++) {
            sink += op.applyAsInt(i, 0);
        }
        long t = System.nanoTime() - t0;
        if (sink == 7) {
            System.out.println("!");
        }
        return t;
    }

    static long timeField(java.util.function.IntBinaryOperator op) {
        int sink = 0;
        long t0 = System.nanoTime();
        for (int i = 0; i < CALLS; i++) {
            sink += op.applyAsInt(i, 0);
        }
        long t = System.nanoTime() - t0;
        if (sink == 7) {
            System.out.println("!");
        }
        return t;
    }

    static long timeControl(java.util.function.IntBinaryOperator op) {
        int sink = 0;
        long t0 = System.nanoTime();
        for (int i = 0; i < CALLS; i++) {
            sink += op.applyAsInt(i, 0);
        }
        long t = System.nanoTime() - t0;
        if (sink == 7) {
            System.out.println("!");
        }
        return t;
    }

    /// One call of a fresh class's loop.
    static long timeOsr(String name) throws Exception {
        java.util.function.IntBinaryOperator op = make(new Loader(), name);
        long t0 = System.nanoTime();
        int r = op.applyAsInt(OSR_ITERS, 0);
        long t = System.nanoTime() - t0;
        if (r == 7) {
            System.out.println("!");
        }
        return t;
    }

    static long median(long[] xs) {
        long[] c = xs.clone();
        java.util.Arrays.sort(c);
        return c[c.length / 2];
    }

    public static void main(String[] args) throws Exception {
        // The controls' cold owner is loaded and initialised up front.
        Warm.seen = 0;
        Loader loader = new Loader();
        java.util.function.IntBinaryOperator hotS = make(loader, "l2d.HotS");
        java.util.function.IntBinaryOperator hotF = make(loader, "l2d.HotF");
        java.util.function.IntBinaryOperator hotC = make(loader, "l2d.HotC");
        long[] s = new long[REPS];
        long[] f = new long[REPS];
        long[] c = new long[REPS];
        long[] o = new long[REPS];
        long[] oc = new long[REPS];
        for (int r = 0; r < REPS; r++) {
            s[r] = timeStatic(hotS);
            f[r] = timeField(hotF);
            c[r] = timeControl(hotC);
            o[r] = timeOsr("l2d.Osr");
            oc[r] = timeOsr("l2d.OsrC");
        }
        System.out.println("static-cold " + median(s) / CALLS + " ns/call");
        System.out.println("field-cold " + median(f) / CALLS + " ns/call");
        System.out.println("control " + median(c) / CALLS + " ns/call");
        System.out.println("osr-cold " + median(o) / (OSR_ITERS / 1000) + " ns/1k-iter");
        System.out.println("osr-control " + median(oc) / (OSR_ITERS / 1000) + " ns/1k-iter");
        System.out.println("check-static " + hotS.applyAsInt(5, 1));
        System.out.println("check-field " + hotF.applyAsInt(13, 1));
        System.out.println("check-osr " + (make(new Loader(), "l2d.Osr").applyAsInt(1000, 1) == osrRef(1000, 1)));
        System.out.println("check-osr-control " + (make(new Loader(), "l2d.OsrC").applyAsInt(1000, 1) == osrRef(1000, 1)));
    }
}
