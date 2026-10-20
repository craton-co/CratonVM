// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 22, lane fuzz8: one set of classfile-built bytes defined
// twice as a hidden class, once with ClassOption.NESTMATE and once without.
// The payload reaches its host's private static method, private static field
// and private instance method, and the host reaches the payload's private
// method through its own Lookup. Then the naming answers of both classes:
// getName, getSimpleName, toString, getCanonicalName, getNestHost, the array
// class, and MethodType text that names them.
//
// Risk targeted: the hidden self-name rule of wave 21 (one rule from the
// recorded this_class, `hidden_self_name_rule`), the wave-20 hidden self CP
// owners (`CRATONVM_HIDDEN_SELF_CP_BY_ID`) on a class whose own pool also
// names its host, nestmate access checks from compiled code (`hot` loops
// 30000 times through `callHost`, a self call into a host private), and the
// --compatible name bridges that skip the declaring-class check for hidden
// classes (wave 21). The NESTMATE-less copy must fail every host-private
// access with IllegalAccessError (each attempt, cold and hot), and the host
// must get IllegalAccessException for its private.
//
// Hidden names carry an address suffix; every printed name has "/0x<hex>"
// replaced by "/0xH". Exception messages are not printed (they name the
// hidden class with its suffix): class names only.
//
// Run:
//   javac -d out R21Fz8HiddenNestmate1.java
//   java -Xmx256m -cp out R21Fz8HiddenNestmate1                     # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz8HiddenNestmate1                 # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz8HiddenNestmate1
//   cratonvm --nojit -Xmx256m -cp out R21Fz8HiddenNestmate1
// HotSpot expectation (by reading JDK 25 Class.java, MethodType.toString,
// Lookup.defineHiddenClass and JVMS 5.4.4 nestmate access; not yet run):
//   nm name=R21Fz8NmPayload/0xH simple=R21Fz8NmPayload/0xH str=class R21Fz8NmPayload/0xH canonical=null hidden=true mods=public pkg=""
//   nm nestHost=host hostIsNestmate=true arr=[LR21Fz8NmPayload/0xH; arrSimple=R21Fz8NmPayload/0xH[] mt=(int,R21Fz8NmPayload/0xH[])R21Fz8NmPayload/0xH methods=6
//   nm callHost=8 readSecret=3 callInst=107 selfCls=true hidPriv=42 forName=java.lang.ClassNotFoundException
//   nm hot=195000 hot=195000 counter=60001
//   solo name=... (the same naming lines as nm, except nestHost=self hostIsNestmate=false)
//   solo callHost=java.lang.IllegalAccessError readSecret=java.lang.IllegalAccessError callInst=java.lang.IllegalAccessError selfCls=true hidPriv=java.lang.IllegalAccessException forName=java.lang.ClassNotFoundException
//   solo hot=java.lang.IllegalAccessError hotIAE=20000 counter=60001
//   nm again callHost=8 hot=195000 counter=90002
//   distinct=true

import java.lang.classfile.ClassFile;
import java.lang.classfile.Label;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Modifier;

public class R21Fz8HiddenNestmate1 {
    private static int secret = 3;
    private static int counter;
    private final int base;

    R21Fz8HiddenNestmate1(int base) {
        this.base = base;
    }

    private static int hostPriv(int x) {
        counter++;
        return (x & 7) + secret;
    }

    private int inst(int x) {
        return base + x;
    }

    static final ClassDesc HOST = ClassDesc.of("R21Fz8HiddenNestmate1");
    static final ClassDesc SELF = ClassDesc.of("R21Fz8NmPayload");
    static final MethodTypeDesc MTD_II = MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_int);
    static final MethodTypeDesc MTD_I = MethodTypeDesc.of(ConstantDescs.CD_int);

    static byte[] payload() {
        return ClassFile.of().build(SELF, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("hidPriv", MTD_I, ClassFile.ACC_PRIVATE | ClassFile.ACC_STATIC,
                    c -> c.bipush(42).ireturn());
            cb.withMethodBody("callHost", MTD_II, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    c -> c.iload(0).invokestatic(HOST, "hostPriv", MTD_II).ireturn());
            cb.withMethodBody("readSecret", MTD_I, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    c -> c.getstatic(HOST, "secret", ConstantDescs.CD_int).ireturn());
            cb.withMethodBody("callInst",
                    MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_Object, ConstantDescs.CD_int),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    c -> c.aload(0).checkcast(HOST).iload(1).invokevirtual(HOST, "inst", MTD_II).ireturn());
            cb.withMethodBody("selfCls", MethodTypeDesc.of(ConstantDescs.CD_Class),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    c -> c.ldc(SELF).areturn());
            // static long hot(int n) { long acc = 0; for (int i = 0; i < n; i++) acc += callHost(i); return acc; }
            cb.withMethodBody("hot", MethodTypeDesc.of(ConstantDescs.CD_long, ConstantDescs.CD_int),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, c -> {
                        Label top = c.newLabel();
                        Label end = c.newLabel();
                        c.lconst_0().lstore(1).iconst_0().istore(3);
                        c.labelBinding(top);
                        c.iload(3).iload(0).if_icmpge(end);
                        c.lload(1).iload(3).invokestatic(SELF, "callHost", MTD_II).i2l().ladd().lstore(1);
                        c.iinc(3, 1).goto_(top);
                        c.labelBinding(end);
                        c.lload(1).lreturn();
                    });
        });
    }

    static String norm(String s) {
        return s == null ? "null" : s.replaceAll("/0x[0-9a-fA-F]+", "/0xH");
    }

    interface Call {
        Object call() throws Throwable;
    }

    static String safe(Call c) {
        try {
            return String.valueOf(c.call());
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    static String who(Class<?> k, Class<?> self) {
        if (k == R21Fz8HiddenNestmate1.class) {
            return "host";
        }
        if (k == self) {
            return "self";
        }
        return norm(k.getName());
    }

    static void naming(String tag, Class<?> c) {
        System.out.println(tag + " name=" + norm(c.getName()) + " simple=" + norm(c.getSimpleName())
                + " str=" + norm(c.toString()) + " canonical=" + c.getCanonicalName()
                + " hidden=" + c.isHidden() + " mods=" + Modifier.toString(c.getModifiers())
                + " pkg=\"" + c.getPackageName() + "\"");
        System.out.println(tag + " nestHost=" + who(c.getNestHost(), c)
                + " hostIsNestmate=" + R21Fz8HiddenNestmate1.class.isNestmateOf(c)
                + " arr=" + norm(c.arrayType().getName())
                + " arrSimple=" + norm(c.arrayType().getSimpleName())
                + " mt=" + norm(MethodType.methodType(c, int.class, c.arrayType()).toString())
                + " methods=" + c.getDeclaredMethods().length);
    }

    static void access(String tag, MethodHandles.Lookup l) throws Throwable {
        Class<?> c = l.lookupClass();
        MethodHandle callHost = l.findStatic(c, "callHost", MethodType.methodType(int.class, int.class));
        MethodHandle readSecret = l.findStatic(c, "readSecret", MethodType.methodType(int.class));
        MethodHandle callInst = l.findStatic(c, "callInst",
                MethodType.methodType(int.class, Object.class, int.class));
        MethodHandle selfCls = l.findStatic(c, "selfCls", MethodType.methodType(Class.class));
        MethodHandles.Lookup hostLookup = MethodHandles.lookup();
        System.out.println(tag + " callHost=" + safe(() -> (int) callHost.invokeExact(5))
                + " readSecret=" + safe(() -> (int) readSecret.invokeExact())
                + " callInst=" + safe(() -> (int) callInst.invoke((Object) new R21Fz8HiddenNestmate1(100), 7))
                + " selfCls=" + safe(() -> (Class<?>) selfCls.invoke() == c)
                + " hidPriv=" + safe(() -> (int) hostLookup.findStatic(c, "hidPriv",
                        MethodType.methodType(int.class)).invoke())
                + " forName=" + safe(() -> Class.forName(c.getName())));
    }

    static String hot(MethodHandles.Lookup l) throws Throwable {
        MethodHandle h = l.findStatic(l.lookupClass(), "hot", MethodType.methodType(long.class, int.class));
        return safe(() -> (long) h.invokeExact(30000));
    }

    public static void main(String[] args) throws Throwable {
        byte[] bytes = payload();
        MethodHandles.Lookup lk = MethodHandles.lookup();
        MethodHandles.Lookup nm = lk.defineHiddenClass(bytes, true, MethodHandles.Lookup.ClassOption.NESTMATE);
        naming("nm", nm.lookupClass());
        access("nm", nm);
        String h1 = hot(nm);
        String h2 = hot(nm);
        System.out.println("nm hot=" + h1 + " hot=" + h2 + " counter=" + counter);

        MethodHandles.Lookup solo = lk.defineHiddenClass(bytes, true);
        naming("solo", solo.lookupClass());
        access("solo", solo);
        String sh = hot(solo);
        MethodHandle sc = solo.findStatic(solo.lookupClass(), "callHost",
                MethodType.methodType(int.class, int.class));
        int iae = 0;
        for (int i = 0; i < 20000; i++) {
            try {
                int r = (int) sc.invokeExact(i);
                if (r < 0) {
                    iae -= 1000000;
                }
            } catch (IllegalAccessError e) {
                iae++;
            }
        }
        System.out.println("solo hot=" + sh + " hotIAE=" + iae + " counter=" + counter);

        MethodHandle nc = nm.findStatic(nm.lookupClass(), "callHost", MethodType.methodType(int.class, int.class));
        System.out.println("nm again callHost=" + safe(() -> (int) nc.invokeExact(5)) + " hot=" + hot(nm)
                + " counter=" + counter);
        System.out.println("distinct=" + (nm.lookupClass() != solo.lookupClass()));
    }
}
