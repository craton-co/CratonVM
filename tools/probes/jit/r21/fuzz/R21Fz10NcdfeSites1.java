// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 25, lane fuzz10: resolution-error caching with its cause,
// through every bytecode that resolves a class entry, cold and hot, for two
// kinds of failure:
//   Gone  a class the loader never has (ClassNotFoundException -> NCDFE, a
//         LinkageError, recorded on the class entry of the asking class)
//   Boom  a class that loads fine but whose <clinit> throws (EIIE once, then
//         NCDFE "Could not initialize class" with the saved EIIE as cause);
//         anewarray / ldc / checkcast / instanceof of Boom never initialize it
//         and must keep working after it became erroneous.
//
// Classes (package fz10r, built with java.lang.classfile, one loader that
// counts every request for an fz10r name):
//   Boom   static int F; static int m(); <clinit> throws RuntimeException("boom")
//   User1, User2 (same bytes, own constant pools), sites:
//     gCall invokestatic Gone.m   gGet getstatic Gone.F   gNew new Gone
//     gArr  anewarray Gone        gCast checkcast Gone ("s")
//     gInst instanceof Gone ("s") gLit ldc Gone.class     gPut putstatic Gone.F
//     gMulti multianewarray [[Gone (its OWN class entry, an array name)
//     bArr bLit bCast bInst (no init)  bCall bGet bNew (init)
//     spinG(n) / spinB(n): a loop in the generated class whose body throws
//     (getstatic Gone.F / new Boom) and whose handler calls check(t)
//
// Risk targeted: the recorded-resolution-failure memo shared by the
// interpreter and compiled code (cause kept, a FRESH exception and a fresh
// cause per rethrow), the array-name entry of multianewarray, and an
// initialization error leaking into sites that never initialize.
//
// Run:
//   javac -d out R21Fz10NcdfeSites1.java
//   java -XX:-OmitStackTraceInFastThrow -Xmx256m -cp out R21Fz10NcdfeSites1   # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz10NcdfeSites1                              # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz10NcdfeSites1
//   cratonvm --nojit -Xmx256m -cp out R21Fz10NcdfeSites1
// (-XX:-OmitStackTraceInFastThrow: the hot bCast ClassCastException must keep
// its message on C2.)
//
// HotSpot 25 expectation (by reading JVMS 5.4.3 / 5.5,
// SystemDictionary::handle_resolution_exception, ConstantPool::
// save_and_throw_exception / throw_resolution_error, InstanceKlass::
// initialize_impl, java_lang_Throwable::create_initialization_error and
// SharedRuntime::generate_class_cast_message; not yet run). Loader hashes are
// printed as @*. With
//   G  = java.lang.NoClassDefFoundError: fz10r/Gone cause=java.lang.ClassNotFoundException: fz10r.Gone
//   GM = java.lang.NoClassDefFoundError: [[Lfz10r/Gone; cause=java.lang.ClassNotFoundException: fz10r.Gone
//   CC = java.lang.ClassCastException: class java.lang.String cannot be cast to class fz10r.Boom
//        (java.lang.String is in module java.base of loader 'bootstrap'; fz10r.Boom is in unnamed
//        module of loader 'fz10r' @*)                       (one line)
//   BI = java.lang.NoClassDefFoundError: Could not initialize class fz10r.Boom
//        cause=java.lang.ExceptionInInitializerError: Exception java.lang.RuntimeException: boom
//        [in thread "main"]                                  (one line)
// the lines are:
//   User1 cold gCall=G
//   User1 cold gGet=G
//   User1 cold gNew=G
//   User1 cold gArr=G
//   User1 cold gCast=G
//   User1 cold gInst=G
//   User1 cold gLit=G
//   User1 cold gPut=G
//   User1 cold gMulti=GM
//   User1 cold bArr=array 2 of fz10r.Boom
//   User1 cold bLit=class fz10r.Boom
//   User1 cold bCast=CC
//   User1 cold bInst=int 0
//   User1 cold bCall=java.lang.ExceptionInInitializerError cause=java.lang.RuntimeException: boom
//   User1 cold bGet=BI
//   User1 cold bNew=BI
//   User1 again bArr=array 2 of fz10r.Boom
//   User1 again bLit=class fz10r.Boom
//   User1 again bCast=CC
//   User1 again bInst=int 0
//   User1 fresh gone newThrowable=true newCause=true
//   User1 fresh boom newThrowable=true sameCause=true
//   User1 hot gCall same=20000 ... (one line per site, every site same=20000)
//   User1 spinG same=20000 spinB same=20000
//   User1 asked Gone=2 Boom=1
//   User2 cold: the User1 cold lines, except bCall=BI (Boom is already erroneous)
//   User2 asked Gone=4 Boom=1
// sameCause: InstanceKlass keeps ONE saved EIIE per erroneous class and every
// "Could not initialize" NCDFE carries that object; a resolution-error rethrow
// builds a new cause each time. Header lines most likely to need a fix after
// the first HotSpot run: GM, sameCause, the asked counts.

import java.lang.classfile.ClassFile;
import java.lang.classfile.Label;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.HashMap;
import java.util.Map;

public class R21Fz10NcdfeSites1 {
    static final int HOT = 20_000;
    static final String P = "fz10r.";
    static final ClassDesc GONE = ClassDesc.of(P + "Gone");
    static final ClassDesc BOOM = ClassDesc.of(P + "Boom");
    static final ClassDesc GONE_AA = GONE.arrayType(2);
    static final ClassDesc MAIN = ClassDesc.of("R21Fz10NcdfeSites1");
    static final ClassDesc RTE = ClassDesc.of("java.lang.RuntimeException");
    static final ClassDesc LINKAGE = ClassDesc.of("java.lang.LinkageError");
    static final MethodTypeDesc MTD_I = MethodTypeDesc.of(ConstantDescs.CD_int);
    static final MethodTypeDesc MTD_O = MethodTypeDesc.of(ConstantDescs.CD_Object);
    static final MethodTypeDesc MTD_I_I = MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_int);

    static String expect;
    static int same;

    // Called from the handlers of User.spinG / User.spinB.
    public static void check(Throwable t) {
        if (describe(t).equals(expect)) {
            same++;
        }
    }

    static final class Loader extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();
        final Map<String, Integer> asked = new HashMap<>();

        Loader() {
            super("fz10r", R21Fz10NcdfeSites1.class.getClassLoader());
        }

        int asked(String simple) {
            return asked.getOrDefault(P + simple, 0);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (!name.startsWith(P)) {
                return super.loadClass(name, resolve);
            }
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                asked.merge(name, 1, Integer::sum);
                byte[] b = bytes.get(name);
                if (b == null) {
                    throw new ClassNotFoundException(name);
                }
                return defineClass(name, b, 0, b.length);
            }
        }
    }

    static byte[] boom() {
        return ClassFile.of().build(BOOM, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withField("F", ConstantDescs.CD_int, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC);
            cb.withMethodBody(ConstantDescs.CLASS_INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_STATIC,
                    c -> c.new_(RTE).dup().ldc("boom")
                            .invokespecial(RTE, ConstantDescs.INIT_NAME,
                                    MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_String))
                            .athrow());
            cb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                    c -> c.aload(0).invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                            ConstantDescs.MTD_void).return_());
            cb.withMethodBody("m", MTD_I, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    c -> c.iconst_1().ireturn());
        });
    }

    static byte[] user(String simple) {
        int ps = ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC;
        return ClassFile.of().build(ClassDesc.of(P + simple), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("gCall", MTD_I, ps, c -> c.invokestatic(GONE, "m", MTD_I).ireturn());
            cb.withMethodBody("gGet", MTD_I, ps, c -> c.getstatic(GONE, "F", ConstantDescs.CD_int).ireturn());
            cb.withMethodBody("gNew", MTD_O, ps, c -> c.new_(GONE).dup()
                    .invokespecial(GONE, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void).areturn());
            cb.withMethodBody("gArr", MTD_O, ps, c -> c.iconst_2().anewarray(GONE).areturn());
            cb.withMethodBody("gCast", MTD_O, ps, c -> c.ldc("s").checkcast(GONE).areturn());
            cb.withMethodBody("gInst", MTD_I, ps, c -> c.ldc("s").instanceOf(GONE).ireturn());
            cb.withMethodBody("gLit", MTD_O, ps, c -> c.ldc(GONE).areturn());
            cb.withMethodBody("gPut", MTD_I, ps, c -> c.iconst_5().putstatic(GONE, "F", ConstantDescs.CD_int)
                    .iconst_0().ireturn());
            cb.withMethodBody("gMulti", MTD_O, ps, c -> c.iconst_2().iconst_2().multianewarray(GONE_AA, 2)
                    .areturn());
            cb.withMethodBody("bArr", MTD_O, ps, c -> c.iconst_2().anewarray(BOOM).areturn());
            cb.withMethodBody("bLit", MTD_O, ps, c -> c.ldc(BOOM).areturn());
            cb.withMethodBody("bCast", MTD_O, ps, c -> c.ldc("s").checkcast(BOOM).areturn());
            cb.withMethodBody("bInst", MTD_I, ps, c -> c.ldc("s").instanceOf(BOOM).ireturn());
            cb.withMethodBody("bCall", MTD_I, ps, c -> c.invokestatic(BOOM, "m", MTD_I).ireturn());
            cb.withMethodBody("bGet", MTD_I, ps, c -> c.getstatic(BOOM, "F", ConstantDescs.CD_int).ireturn());
            cb.withMethodBody("bNew", MTD_O, ps, c -> c.new_(BOOM).dup()
                    .invokespecial(BOOM, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void).areturn());
            // static int spinG(int n) { int i = 0; for (; i < n; i++) try { Gone.F; } catch (LinkageError t) { check(t); } return i; }
            cb.withMethodBody("spinG", MTD_I_I, ps, c -> spin(c, true));
            cb.withMethodBody("spinB", MTD_I_I, ps, c -> spin(c, false));
        });
    }

    static void spin(java.lang.classfile.CodeBuilder c, boolean gone) {
        Label top = c.newLabel();
        Label end = c.newLabel();
        c.iconst_0().istore(1);
        c.labelBinding(top);
        c.iload(1).iload(0).if_icmpge(end);
        c.trying(t -> {
            if (gone) {
                t.getstatic(GONE, "F", ConstantDescs.CD_int).pop();
            } else {
                t.new_(BOOM).dup().invokespecial(BOOM, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void).pop();
            }
        }, k -> k.catching(LINKAGE, h -> h.invokestatic(MAIN, "check",
                MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_Throwable))));
        c.iinc(1, 1).goto_(top);
        c.labelBinding(end);
        c.iload(1).ireturn();
    }

    static final String[] GSITES = {"gCall", "gGet", "gNew", "gArr", "gCast", "gInst", "gLit", "gPut", "gMulti"};
    static final String[] BSITES = {"bArr", "bLit", "bCast", "bInst", "bCall", "bGet", "bNew"};
    static final String[] BAGAIN = {"bArr", "bLit", "bCast", "bInst"};

    static String mask(String s) {
        return s == null ? "null" : s.replaceAll(" @[0-9a-fA-F]+", " @*");
    }

    static String describe(Throwable t) {
        String m = t.getMessage();
        Throwable c = t.getCause();
        String cause = c == null ? ""
                : " cause=" + c.getClass().getName() + (c.getMessage() == null ? "" : ": " + c.getMessage());
        return mask(t.getClass().getName() + (m == null ? "" : ": " + m) + cause);
    }

    static String show(Object r) {
        if (r == null) {
            return "null";
        }
        if (r instanceof Integer i) {
            return "int " + i;
        }
        if (r instanceof Class<?> k) {
            return "class " + k.getName();
        }
        if (r.getClass().isArray()) {
            return "array " + java.lang.reflect.Array.getLength(r) + " of " + r.getClass().getComponentType()
                    .getName();
        }
        return "obj " + r.getClass().getName();
    }

    static Throwable last;

    static String run(MethodHandle mh) {
        try {
            last = null;
            return show(mh.invoke());
        } catch (Throwable t) {
            last = t;
            return describe(t);
        }
    }

    static Map<String, MethodHandle> sites(Class<?> c) throws ReflectiveOperationException {
        MethodHandles.Lookup l = MethodHandles.lookup();
        Map<String, MethodHandle> m = new HashMap<>();
        for (java.lang.reflect.Method meth : c.getDeclaredMethods()) {
            if (meth.getParameterCount() == 0) {
                m.put(meth.getName(), l.unreflect(meth));
            }
        }
        m.put("spinG", l.findStatic(c, "spinG", MethodType.methodType(int.class, int.class)));
        m.put("spinB", l.findStatic(c, "spinB", MethodType.methodType(int.class, int.class)));
        return m;
    }

    public static void main(String[] args) throws Throwable {
        Loader l = new Loader();
        l.bytes.put(P + "Boom", boom());
        l.bytes.put(P + "User1", user("User1"));
        l.bytes.put(P + "User2", user("User2"));

        // ---- User1 ----
        Map<String, MethodHandle> s1 = sites(Class.forName(P + "User1", true, l));
        Map<String, String> cold = new HashMap<>();
        for (String s : GSITES) {
            String r = run(s1.get(s));
            cold.put(s, r);
            System.out.println("User1 cold " + s + "=" + r);
        }
        for (String s : BSITES) {
            String r = run(s1.get(s));
            cold.put(s, r);
            System.out.println("User1 cold " + s + "=" + r);
        }
        for (String s : BAGAIN) {
            System.out.println("User1 again " + s + "=" + run(s1.get(s)));
        }
        run(s1.get("gGet"));
        Throwable g1 = last;
        run(s1.get("gGet"));
        Throwable g2 = last;
        System.out.println("User1 fresh gone newThrowable=" + (g1 != g2) + " newCause="
                + (g1 != null && g2 != null && g1.getCause() != g2.getCause()));
        run(s1.get("bGet"));
        Throwable b1 = last;
        run(s1.get("bNew"));
        Throwable b2 = last;
        System.out.println("User1 fresh boom newThrowable=" + (b1 != b2) + " sameCause="
                + (b1 != null && b2 != null && b1.getCause() == b2.getCause()));

        // Hot: each site 20000 times, compared with its cold answer (bCall
        // compared with bGet's: after the first EIIE every init site is BI).
        for (String s : GSITES) {
            hot("User1", s, s1.get(s), cold.get(s));
        }
        for (String s : BSITES) {
            hot("User1", s, s1.get(s), cold.get(s.equals("bCall") ? "bGet" : s));
        }
        expect = cold.get("gGet");
        same = 0;
        int ng = (int) s1.get("spinG").invoke(HOT);
        int sg = same;
        expect = cold.get("bGet");
        same = 0;
        int nb = (int) s1.get("spinB").invoke(HOT);
        int sb = same;
        System.out.println("User1 spinG same=" + (ng == HOT ? sg : -ng) + " spinB same=" + (nb == HOT ? sb : -nb));
        System.out.println("User1 asked Gone=" + l.asked("Gone") + " Boom=" + l.asked("Boom"));

        // ---- User2: its own constant pool; Boom is already erroneous ----
        Map<String, MethodHandle> s2 = sites(Class.forName(P + "User2", true, l));
        for (String s : GSITES) {
            System.out.println("User2 cold " + s + "=" + run(s2.get(s)));
        }
        for (String s : BSITES) {
            System.out.println("User2 cold " + s + "=" + run(s2.get(s)));
        }
        System.out.println("User2 asked Gone=" + l.asked("Gone") + " Boom=" + l.asked("Boom"));
    }

    static void hot(String who, String s, MethodHandle mh, String want) {
        int ok = 0;
        String first = null;
        for (int i = 0; i < HOT; i++) {
            String r = run(mh);
            if (r.equals(want)) {
                ok++;
            } else if (first == null) {
                first = r;
            }
        }
        System.out.println(who + " hot " + s + " same=" + ok + (first == null ? "" : " firstDiff=" + first));
    }
}
