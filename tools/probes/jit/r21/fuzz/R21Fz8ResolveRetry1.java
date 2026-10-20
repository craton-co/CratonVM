// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 22, lane fuzz8: a class that its loader refuses the FIRST
// time it is asked for, and serves afterwards. Every constant-pool site of
// the class that saw the refusal keeps failing with the same error and the
// same cause, cold and hot; a class loaded later resolves the name; and a
// record whose component type was refused once answers on the next call.
//
// Risk targeted: wave 21 item 6 of r21-review-w21-findings-20261019.md (the
// sticky resolution memo: (b) record-component descriptors are NOT recorded,
// (c) `RecordedResolutionFailure` keeps the cause and the rethrow carries it;
// R21ResolutionErrorCause is the HotSpot-confirmed single-site version), plus
// the compiled throw path of an unresolvable `new` in a hot loop with a
// handler (`spin`), and HotSpot's null fast path: `instanceof` / `checkcast`
// of null never resolve the class, so they neither fail nor ask the loader.
//
// Classes (package r21fz8r, built with java.lang.classfile, one loader):
//   Dep    refused on the first request, then served (v() = 7, F = 11)
//   Dep2   refused on the first request, then served
//   User   sites: make (new), call (invokestatic), getF (getstatic), lit (ldc),
//          arr (anewarray), isNull (instanceof null), castNull (checkcast null),
//          spin(n) (new in a loop, handler calls check)
//   User2, User3   the same bytes under other names (their own constant pools)
//   R      final record, one component `d` of type Dep2
//
// Run:
//   javac -d out R21Fz8ResolveRetry1.java
//   java -Xmx256m -cp out R21Fz8ResolveRetry1                     # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz8ResolveRetry1                 # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz8ResolveRetry1
//   cratonvm --nojit -Xmx256m -cp out R21Fz8ResolveRetry1
// HotSpot expectation (by reading JVMS 5.4.3, ConstantPool::throw_resolution_error,
// SystemDictionary::handle_resolution_exception, TemplateTable::instanceof /
// checkcast and java_lang_reflect_RecordComponent::create; not yet run), with
// E = "java.lang.NoClassDefFoundError: r21fz8r/Dep cause=java.lang.ClassNotFoundException: r21fz8r.Dep":
//   null-cold isNull=int 0 castNull=null asked=0
//   cold make=E, then call, getF, lit, arr each = E (5 lines); asked=1
//   hot sites same=20000 spin same=20000 null=40000 asked=1
//   user2 make=obj r21fz8r.Dep call=int 7 getF=int 11 lit=class r21fz8r.Dep arr=array 3 of r21fz8r.Dep asked=2
//   user again: every site = E (5 lines); spin same=40000 asked=2 forName=true
//   user3 make=obj r21fz8r.Dep call=int 7 asked=2
//   record isRecord=true
//   record 1 java.lang.NoClassDefFoundError: r21fz8r/Dep2 cause=java.lang.ClassNotFoundException: r21fz8r.Dep2 asked=1
//   record 2 d:r21fz8r.Dep2 accessor=d asked=2
//   record 3 d:r21fz8r.Dep2 accessor=d asked=2

import java.lang.classfile.ClassFile;
import java.lang.classfile.Label;
import java.lang.classfile.attribute.RecordAttribute;
import java.lang.classfile.attribute.RecordComponentInfo;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.RecordComponent;
import java.util.HashMap;
import java.util.Map;

public class R21Fz8ResolveRetry1 {
    static final String P = "r21fz8r.";
    static final ClassDesc DEP = ClassDesc.of(P + "Dep");
    static final ClassDesc DEP2 = ClassDesc.of(P + "Dep2");
    static final ClassDesc MAIN = ClassDesc.of("R21Fz8ResolveRetry1");
    static final MethodTypeDesc MTD_I = MethodTypeDesc.of(ConstantDescs.CD_int);
    static final MethodTypeDesc MTD_O = MethodTypeDesc.of(ConstantDescs.CD_Object);

    static String first;
    static int same;

    // Called from the handler of User.spin.
    public static void check(Throwable t) {
        if ((t + " cause=" + t.getCause()).equals(first)) {
            same++;
        }
    }

    static final class Loader extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();
        final Map<String, Integer> refuse = new HashMap<>();
        final Map<String, Integer> asked = new HashMap<>();

        Loader() {
            super(R21Fz8ResolveRetry1.class.getClassLoader());
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
                int left = refuse.getOrDefault(name, 0);
                if (left > 0) {
                    refuse.put(name, left - 1);
                    throw new ClassNotFoundException(name);
                }
                byte[] b = bytes.get(name);
                if (b == null) {
                    throw new ClassNotFoundException(name);
                }
                return defineClass(name, b, 0, b.length);
            }
        }
    }

    static byte[] dep() {
        return ClassFile.of().build(DEP, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withField("F", ConstantDescs.CD_int, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC);
            cb.withMethodBody(ConstantDescs.CLASS_INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_STATIC,
                    c -> c.bipush(11).putstatic(DEP, "F", ConstantDescs.CD_int).return_());
            cb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                    c -> c.aload(0).invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                            ConstantDescs.MTD_void).return_());
            cb.withMethodBody("v", MTD_I, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    c -> c.bipush(7).ireturn());
        });
    }

    static byte[] dep2() {
        return ClassFile.of().build(DEP2, cb -> cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER));
    }

    static byte[] user(String simple) {
        int ps = ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC;
        return ClassFile.of().build(ClassDesc.of(P + simple), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("make", MTD_O, ps, c -> c.new_(DEP).dup()
                    .invokespecial(DEP, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void).areturn());
            cb.withMethodBody("call", MTD_I, ps, c -> c.invokestatic(DEP, "v", MTD_I).ireturn());
            cb.withMethodBody("getF", MTD_I, ps, c -> c.getstatic(DEP, "F", ConstantDescs.CD_int).ireturn());
            cb.withMethodBody("lit", MethodTypeDesc.of(ConstantDescs.CD_Class), ps, c -> c.ldc(DEP).areturn());
            cb.withMethodBody("arr", MTD_O, ps, c -> c.iconst_3().anewarray(DEP).areturn());
            cb.withMethodBody("isNull", MTD_I, ps, c -> c.aconst_null().instanceOf(DEP).ireturn());
            cb.withMethodBody("castNull", MTD_O, ps, c -> c.aconst_null().checkcast(DEP).areturn());
            // static void spin(int n) { for (int i = 0; i < n; i++) try { new Dep(); } catch (Throwable t) { check(t); } }
            cb.withMethodBody("spin", MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_int), ps, c -> {
                Label top = c.newLabel();
                Label end = c.newLabel();
                c.iconst_0().istore(1);
                c.labelBinding(top);
                c.iload(1).iload(0).if_icmpge(end);
                c.trying(t -> t.new_(DEP).dup()
                                .invokespecial(DEP, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void).pop(),
                        k -> k.catching(ConstantDescs.CD_Throwable, h -> h.invokestatic(MAIN, "check",
                                MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_Throwable))));
                c.iinc(1, 1).goto_(top);
                c.labelBinding(end);
                c.return_();
            });
        });
    }

    static byte[] record() {
        ClassDesc r = ClassDesc.of(P + "R");
        return ClassFile.of().build(r, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_FINAL | ClassFile.ACC_SUPER);
            cb.withSuperclass(ClassDesc.of("java.lang.Record"));
            cb.with(RecordAttribute.of(RecordComponentInfo.of("d", DEP2)));
            cb.withMethodBody("d", MethodTypeDesc.of(DEP2), ClassFile.ACC_PUBLIC,
                    c -> c.aconst_null().areturn());
        });
    }

    static final String[] SITES = {"make", "call", "getF", "lit", "arr"};

    static final class Sites {
        final MethodHandle[] mh = new MethodHandle[SITES.length];
        final MethodHandle isNull;
        final MethodHandle castNull;
        final MethodHandle spin;

        Sites(Class<?> c) throws ReflectiveOperationException {
            MethodHandles.Lookup l = MethodHandles.lookup();
            mh[0] = l.findStatic(c, "make", MethodType.methodType(Object.class));
            mh[1] = l.findStatic(c, "call", MethodType.methodType(int.class));
            mh[2] = l.findStatic(c, "getF", MethodType.methodType(int.class));
            mh[3] = l.findStatic(c, "lit", MethodType.methodType(Class.class));
            mh[4] = l.findStatic(c, "arr", MethodType.methodType(Object.class));
            isNull = l.findStatic(c, "isNull", MethodType.methodType(int.class));
            castNull = l.findStatic(c, "castNull", MethodType.methodType(Object.class));
            spin = l.findStatic(c, "spin", MethodType.methodType(void.class, int.class));
        }
    }

    static String run(MethodHandle mh) {
        try {
            Object r = mh.invoke();
            if (r == null) {
                return "null";
            }
            if (r instanceof Class<?> k) {
                return "class " + k.getName();
            }
            if (r instanceof Integer) {
                return "int " + r;
            }
            if (r.getClass().isArray()) {
                return "array " + java.lang.reflect.Array.getLength(r) + " of "
                        + r.getClass().getComponentType().getName();
            }
            return "obj " + r.getClass().getName();
        } catch (Throwable t) {
            return t + " cause=" + t.getCause();
        }
    }

    static String components(Class<?> rc) {
        try {
            StringBuilder sb = new StringBuilder();
            for (RecordComponent c : rc.getRecordComponents()) {
                sb.append(c.getName()).append(':').append(c.getType().getName())
                        .append(" accessor=").append(c.getAccessor() == null ? "null" : c.getAccessor().getName());
            }
            return sb.toString();
        } catch (Throwable t) {
            return t + " cause=" + t.getCause();
        }
    }

    public static void main(String[] args) throws Throwable {
        Loader ld = new Loader();
        ld.bytes.put(P + "Dep", dep());
        ld.bytes.put(P + "Dep2", dep2());
        ld.bytes.put(P + "User", user("User"));
        ld.bytes.put(P + "User2", user("User2"));
        ld.bytes.put(P + "User3", user("User3"));
        ld.bytes.put(P + "R", record());
        ld.refuse.put(P + "Dep", 1);
        ld.refuse.put(P + "Dep2", 1);

        Sites u = new Sites(Class.forName(P + "User", false, ld));
        System.out.println("null-cold isNull=" + run(u.isNull) + " castNull=" + run(u.castNull)
                + " asked=" + ld.asked("Dep"));

        for (int i = 0; i < SITES.length; i++) {
            String s = run(u.mh[i]);
            if (first == null) {
                first = s;
            }
            System.out.println("cold " + SITES[i] + "=" + s);
        }
        System.out.println("cold asked=" + ld.asked("Dep"));

        int sitesSame = 0;
        int nulls = 0;
        for (int i = 0; i < 20000; i++) {
            if (run(u.mh[i % SITES.length]).equals(first)) {
                sitesSame++;
            }
            if (run(u.isNull).equals("int 0")) {
                nulls++;
            }
            if (run(u.castNull).equals("null")) {
                nulls++;
            }
        }
        u.spin.invoke(20000);
        System.out.println("hot sites same=" + sitesSame + " spin same=" + same + " null=" + nulls
                + " asked=" + ld.asked("Dep"));

        Sites u2 = new Sites(Class.forName(P + "User2", false, ld));
        StringBuilder sb = new StringBuilder("user2");
        for (int i = 0; i < SITES.length; i++) {
            sb.append(' ').append(SITES[i]).append('=').append(run(u2.mh[i]));
        }
        System.out.println(sb + " asked=" + ld.asked("Dep"));

        for (int i = 0; i < SITES.length; i++) {
            System.out.println("user again " + SITES[i] + "=" + run(u.mh[i]));
        }
        u.spin.invoke(20000);
        Object made = u2.mh[0].invoke();
        System.out.println("user again spin same=" + same + " asked=" + ld.asked("Dep")
                + " forName=" + (Class.forName(P + "Dep", false, ld) == made.getClass()));

        Sites u3 = new Sites(Class.forName(P + "User3", false, ld));
        System.out.println("user3 make=" + run(u3.mh[0]) + " call=" + run(u3.mh[1])
                + " asked=" + ld.asked("Dep"));

        Class<?> rc = Class.forName(P + "R", false, ld);
        System.out.println("record isRecord=" + rc.isRecord());
        for (int i = 1; i <= 3; i++) {
            System.out.println("record " + i + " " + components(rc) + " asked=" + ld.asked("Dep2"));
        }
    }
}
