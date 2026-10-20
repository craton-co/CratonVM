// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// probe -- Interpreter round i4 wave 98, lane L2. The deferred compiled field
// site (`CRATONVM_JIT_DEFERRED_FIELD_SITES`, `jit_field_site_cp`) and the
// optimizing tier's unresolved-field trap (`CRATONVM_JIT_IR_FIELD_SITE_TRAP`),
// both default ON since round i4, on the LINKAGE answers L2W92/L2W95 do not
// cover. One generated method `Hot.run(II)I`, defined with all its owners by a
// child-first in-memory loader (so no compile door can see an owner before
// the site runs), is made hot with mode 0; every other mode is a cold site
// whose owner is not loaded when it compiles:
//
//   1 getstatic Owner.inst  (an instance field)     IncompatibleClassChangeError
//   2 null.getfield Owner.stat (a static field)     ICCE before the NPE
//   3 getstatic Owner.missing                       NoSuchFieldError
//   4 putstatic Owner.FIN (static final, from Hot)  IllegalAccessError
//   5 getstatic Owner.priv (private)                IllegalAccessError (text cut at " (")
//   6 null.putfield Owner.nope                      NoSuchFieldError before the NPE
//   7 getstatic Sub.X, X declared by interface Iface  initialises Iface only
//   8 getstatic Sub.Y, Y declared by superclass Sup   initialises Sup only
//   9 getstatic Rec.v; Rec.<clinit> calls run(0,10) first, which reads Rec.v
//     during the class's own initialisation (the default, 0, so +1 = 1)
//  11 getstatic Slow.v while another thread is inside Slow.<clinit>: waits
//
// Each error mode runs twice (the second touch must answer the same), then the
// method is warmed again (a trap recompile) and the error modes run once more.
//
// Run (both modes, JIT and --nojit; stdout must equal HotSpot's):
//   $J/javac -d out tools/probes/interp/L2/L2W98DeferredFieldLinkage.java
//   $J/java -cp out L2W98DeferredFieldLinkage
//   $B --java-home $J/.. -cp out L2W98DeferredFieldLinkage
// Census that the compiled arms were reached:
//   CRATONVM_DBG_JITC=1 $B ... 2>&1 | grep -E 'deferred-field-sites|field-site trap'
//
// HOTSPOT_EXPECTED_BEGIN
// warm ok
// m1 java.lang.IncompatibleClassChangeError: Expected static field l2w98.Owner.inst
// m1 java.lang.IncompatibleClassChangeError: Expected static field l2w98.Owner.inst
// m2 java.lang.IncompatibleClassChangeError: Expected non-static field l2w98.Owner.stat
// m2 java.lang.IncompatibleClassChangeError: Expected non-static field l2w98.Owner.stat
// m3 java.lang.NoSuchFieldError: Class l2w98.Owner does not have member field 'int missing'
// m3 java.lang.NoSuchFieldError: Class l2w98.Owner does not have member field 'int missing'
// m4 java.lang.IllegalAccessError: Update to static final field l2w98.Owner.FIN attempted from a different class (l2w98.Hot) than the field's declaring class
// m4 java.lang.IllegalAccessError: Update to static final field l2w98.Owner.FIN attempted from a different class (l2w98.Hot) than the field's declaring class
// m5 java.lang.IllegalAccessError: class l2w98.Hot tried to access private field l2w98.Owner.priv
// m5 java.lang.IllegalAccessError: class l2w98.Hot tried to access private field l2w98.Owner.priv
// m6 java.lang.NoSuchFieldError: Class l2w98.Owner does not have member field 'int nope'
// m6 java.lang.NoSuchFieldError: Class l2w98.Owner does not have member field 'int nope'
// Iface.<clinit>
// m7 77
// Sup.<clinit>
// m8 88
// Rec.<clinit>
// m9 500 seenDuring=1
// m11 900
// log [Iface, Sup, Rec]
// rewarm ok
// m1 java.lang.IncompatibleClassChangeError: Expected static field l2w98.Owner.inst
// m2 java.lang.IncompatibleClassChangeError: Expected non-static field l2w98.Owner.stat
// m3 java.lang.NoSuchFieldError: Class l2w98.Owner does not have member field 'int missing'
// m4 java.lang.IllegalAccessError: Update to static final field l2w98.Owner.FIN attempted from a different class (l2w98.Hot) than the field's declaring class
// m5 java.lang.IllegalAccessError: class l2w98.Hot tried to access private field l2w98.Owner.priv
// m6 java.lang.NoSuchFieldError: Class l2w98.Owner does not have member field 'int nope'
// m7 77
// m8 88
// HOTSPOT_EXPECTED_END
import java.lang.classfile.ClassBuilder;
import java.lang.classfile.ClassFile;
import java.lang.classfile.CodeBuilder;
import java.lang.classfile.Label;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.util.ArrayList;
import java.util.Collections;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.CountDownLatch;
import java.util.function.Consumer;
import java.util.function.IntBinaryOperator;

public class L2W98DeferredFieldLinkage {
    static final ClassDesc I = ConstantDescs.CD_int;
    static final int PUBLIC = ClassFile.ACC_PUBLIC;
    static final int STATIC = ClassFile.ACC_STATIC;
    static final int FINAL = ClassFile.ACC_FINAL;
    static final String PKG = "l2w98.";
    static final ClassDesc SELF = ClassDesc.of("L2W98DeferredFieldLinkage");
    static final MethodTypeDesc LOG_DESC =
            MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_String);

    /// Called from the generated `<clinit>`s.
    public static final List<String> LOG = Collections.synchronizedList(new ArrayList<>());
    public static final CountDownLatch SLOW_STARTED = new CountDownLatch(1);

    public static void clinit(String name) {
        System.out.println(name + ".<clinit>");
        LOG.add(name);
    }

    public static void slowStarted() throws InterruptedException {
        SLOW_STARTED.countDown();
        Thread.sleep(300);
    }

    static ClassDesc d(String n) {
        return ClassDesc.of(PKG + n);
    }

    static void ctor(ClassBuilder clb, ClassDesc sup) {
        clb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, PUBLIC,
                cb -> cb.aload(0).invokespecial(sup, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                        .return_());
    }

    static byte[] cls(String name, ClassDesc sup, Consumer<ClassBuilder> body) {
        return ClassFile.of().build(d(name), clb -> {
            clb.withFlags(PUBLIC | ClassFile.ACC_SUPER).withSuperclass(sup);
            body.accept(clb);
        });
    }

    static Map<String, byte[]> classes() {
        Map<String, byte[]> m = new HashMap<>();
        m.put("Owner", cls("Owner", ConstantDescs.CD_Object, clb -> {
            clb.withField("inst", I, PUBLIC);
            clb.withField("stat", I, PUBLIC | STATIC);
            clb.withField("FIN", I, PUBLIC | STATIC | FINAL);
            clb.withField("priv", I, ClassFile.ACC_PRIVATE | STATIC);
            ctor(clb, ConstantDescs.CD_Object);
            clb.withMethodBody(ConstantDescs.CLASS_INIT_NAME, ConstantDescs.MTD_void, STATIC,
                    cb -> cb.bipush(5).putstatic(d("Owner"), "FIN", I).return_());
        }));
        m.put("Iface", ClassFile.of().build(d("Iface"), clb -> {
            clb.withFlags(PUBLIC | ClassFile.ACC_INTERFACE | ClassFile.ACC_ABSTRACT);
            clb.withField("X", I, PUBLIC | STATIC | FINAL);
            clb.withMethodBody(ConstantDescs.CLASS_INIT_NAME, ConstantDescs.MTD_void, STATIC,
                    cb -> cb.ldc("Iface").invokestatic(SELF, "clinit", LOG_DESC)
                            .bipush(77).putstatic(d("Iface"), "X", I).return_());
        }));
        m.put("Sup", cls("Sup", ConstantDescs.CD_Object, clb -> {
            clb.withField("Y", I, PUBLIC | STATIC);
            ctor(clb, ConstantDescs.CD_Object);
            clb.withMethodBody(ConstantDescs.CLASS_INIT_NAME, ConstantDescs.MTD_void, STATIC,
                    cb -> cb.ldc("Sup").invokestatic(SELF, "clinit", LOG_DESC)
                            .bipush(88).putstatic(d("Sup"), "Y", I).return_());
        }));
        m.put("Sub", cls("Sub", d("Sup"), clb -> {
            clb.withInterfaceSymbols(d("Iface"));
            ctor(clb, d("Sup"));
            clb.withMethodBody(ConstantDescs.CLASS_INIT_NAME, ConstantDescs.MTD_void, STATIC,
                    cb -> cb.ldc("Sub").invokestatic(SELF, "clinit", LOG_DESC).return_());
        }));
        m.put("Rec", cls("Rec", ConstantDescs.CD_Object, clb -> {
            clb.withField("v", I, PUBLIC | STATIC);
            clb.withField("seenDuring", I, PUBLIC | STATIC);
            ctor(clb, ConstantDescs.CD_Object);
            clb.withMethodBody(ConstantDescs.CLASS_INIT_NAME, ConstantDescs.MTD_void, STATIC,
                    cb -> cb.ldc("Rec").invokestatic(SELF, "clinit", LOG_DESC)
                            .iconst_0().bipush(10)
                            .invokestatic(d("Hot"), "run", MethodTypeDesc.of(I, I, I))
                            .putstatic(d("Rec"), "seenDuring", I)
                            .sipush(500).putstatic(d("Rec"), "v", I).return_());
        }));
        m.put("Slow", cls("Slow", ConstantDescs.CD_Object, clb -> {
            clb.withField("v", I, PUBLIC | STATIC);
            ctor(clb, ConstantDescs.CD_Object);
            clb.withMethodBody(ConstantDescs.CLASS_INIT_NAME, ConstantDescs.MTD_void, STATIC,
                    cb -> cb.invokestatic(SELF, "slowStarted", ConstantDescs.MTD_void)
                            .sipush(900).putstatic(d("Slow"), "v", I).return_());
        }));
        m.put("Hot", cls("Hot", ConstantDescs.CD_Object, clb -> {
            clb.withInterfaceSymbols(ClassDesc.of("java.util.function.IntBinaryOperator"));
            ctor(clb, ConstantDescs.CD_Object);
            clb.withMethodBody("applyAsInt", MethodTypeDesc.of(I, I, I), PUBLIC,
                    cb -> cb.iload(1).iload(2)
                            .invokestatic(d("Hot"), "run", MethodTypeDesc.of(I, I, I)).ireturn());
            clb.withMethodBody("run", MethodTypeDesc.of(I, I, I), PUBLIC | STATIC, cb -> {
                cb.iload(0).istore(2);
                for (int k = 0; k < 8; k++) {
                    cb.iload(2).bipush(31).imul().bipush(k).iadd().istore(2);
                }
                // Reads first, stores / calls last: the optimizing tier plants
                // its trap only where nothing committed precedes the site.
                site(cb, 1, c -> c.getstatic(d("Owner"), "inst", I).ireturn());
                site(cb, 2, c -> c.aconst_null().getfield(d("Owner"), "stat", I).ireturn());
                site(cb, 3, c -> c.getstatic(d("Owner"), "missing", I).ireturn());
                site(cb, 5, c -> c.getstatic(d("Owner"), "priv", I).ireturn());
                site(cb, 7, c -> c.getstatic(d("Sub"), "X", I).ireturn());
                site(cb, 8, c -> c.getstatic(d("Sub"), "Y", I).ireturn());
                site(cb, 9, c -> c.getstatic(d("Rec"), "v", I).ireturn());
                site(cb, 10, c -> c.getstatic(d("Rec"), "v", I).iconst_1().iadd().ireturn());
                site(cb, 11, c -> c.getstatic(d("Slow"), "v", I).ireturn());
                site(cb, 12, c -> c.getstatic(d("Rec"), "seenDuring", I).ireturn());
                site(cb, 4, c -> c.iconst_3().putstatic(d("Owner"), "FIN", I).iconst_0().ireturn());
                site(cb, 6, c -> c.aconst_null().iconst_1().putfield(d("Owner"), "nope", I)
                        .iconst_0().ireturn());
                cb.iload(2).ireturn();
            });
        }));
        return m;
    }

    /// `if (mode == k) { body }`, the mode being local 1.
    static void site(CodeBuilder cb, int k, Consumer<CodeBuilder> body) {
        Label skip = cb.newLabel();
        cb.iload(1).bipush(k).if_icmpne(skip);
        body.accept(cb);
        cb.labelBinding(skip);
    }

    static int mix(int i) {
        int x = i;
        for (int k = 0; k < 8; k++) {
            x = x * 31 + k;
        }
        return x;
    }

    static final class MapLoader extends ClassLoader {
        final Map<String, byte[]> bytes;

        MapLoader(Map<String, byte[]> bytes) {
            super("l2w98", L2W98DeferredFieldLinkage.class.getClassLoader());
            this.bytes = bytes;
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            if (name.startsWith(PKG)) {
                byte[] b = bytes.get(name.substring(PKG.length()));
                if (b != null) {
                    return defineClass(name, b, 0, b.length);
                }
            }
            throw new ClassNotFoundException(name);
        }
    }

    static boolean warm(IntBinaryOperator op) {
        boolean ok = true;
        for (int i = 0; i < 60_000; i++) {
            if (op.applyAsInt(i, 0) != mix(i)) {
                ok = false;
            }
        }
        return ok;
    }

    static void err(IntBinaryOperator op, int mode) {
        try {
            System.out.println("m" + mode + " returned " + op.applyAsInt(0, mode));
        } catch (Throwable t) {
            String msg = String.valueOf(t.getMessage());
            int cut = msg.indexOf(" (");
            if (mode == 5 && cut >= 0) {
                msg = msg.substring(0, cut);
            }
            System.out.println("m" + mode + " " + t.getClass().getName() + ": " + msg);
        }
    }

    public static void main(String[] args) throws Exception {
        MapLoader loader = new MapLoader(classes());
        IntBinaryOperator hot = (IntBinaryOperator) loader.loadClass(PKG + "Hot")
                .getConstructor().newInstance();
        System.out.println(warm(hot) ? "warm ok" : "warm MISMATCH");
        int[] errorModes = {1, 2, 3, 4, 5, 6};
        for (int mode : errorModes) {
            err(hot, mode);
            err(hot, mode);
        }
        System.out.println("m7 " + hot.applyAsInt(0, 7));
        System.out.println("m8 " + hot.applyAsInt(0, 8));
        int v = hot.applyAsInt(0, 9);
        System.out.println("m9 " + v + " seenDuring=" + hot.applyAsInt(0, 12));
        Thread t = new Thread(() -> {
            try {
                Class.forName(PKG + "Slow", true, loader);
            } catch (ClassNotFoundException e) {
                throw new RuntimeException(e);
            }
        });
        t.start();
        SLOW_STARTED.await();
        System.out.println("m11 " + hot.applyAsInt(0, 11));
        t.join();
        System.out.println("log " + LOG);
        System.out.println(warm(hot) ? "rewarm ok" : "rewarm MISMATCH");
        for (int mode : errorModes) {
            err(hot, mode);
        }
        System.out.println("m7 " + hot.applyAsInt(0, 7));
        System.out.println("m8 " + hot.applyAsInt(0, 8));
    }
}
