// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 10, lane indy: the exception chain `Lookup.find*` raises
// when MHN_resolve refuses the caller a member (item 2 of
// docs/internal/fixed-bugs/r21-review-w68-native-findings-FIXED-20261019.md).
//
// HotSpot's MHN_resolve checks a METHOD's (and constructor's) access from the
// lookup class (`LinkResolver::check_method_accessability`) and raises
// IllegalAccessError; `MemberName.Factory.resolveOrFail` turns it into
// `IllegalAccessException: no such method: ...` whose cause carries the
// linkResolver text, e.g.
//   class q.C tried to access private method 'void p.D.priv()' (q.C and p.D
//   are in unnamed module of loader 'r21' @H)
// A field is not access-checked there: findGetter's refusal is the Java-side
// `member is private: ...` with no cause.
//
// Rows: findVirtual / findStatic / findConstructor / findGetter on private,
// package-private and protected members of
//   * p.D, from q.C (unrelated, other package) and q.S (subclass of p.D, other
//     package) -- classes spun with java.lang.classfile into one named loader
//     'r21' (its identity hash is printed as @H);
//   * JDK classes (java.util.ArrayList.grow, String.coder, ClassLoader.
//     findLoadedClass, Void.<init>, String.hash) from this class;
//   * a nested class, from this class (its nestmate: ok) and from the
//     top-level R21MhnOther (not a nestmate: refused);
//   * a hidden class (its /0x tail printed as /0xH), as holder and as caller.
// Each row prints `row <label>: ok` or the chain `| <class>: <message>` from
// the outermost exception in. Identity hashes are normalised to @H.
//
// Run:
//   javac -d out R21MhnAccessTexts.java
//   java -cp out R21MhnAccessTexts                                       # HotSpot 25
//   cratonvm -cp out R21MhnAccessTexts                                   # default (--jdk-only)
//   cratonvm --compatible -cp out R21MhnAccessTexts
//   CRATONVM_MHN_RESOLVE_ACCESS_TEXT=0 cratonvm -cp out R21MhnAccessTexts  # kill: the method rows'
//                                                                         # cause reads `tried to access
//                                                                         # method p.D.priv()V` (DIFF)
// HotSpot expectation: every line identical to the cratonvm default run.
// Known not reproduced by the VM: a user-defined loader's name in --compatible
// (the clause is left out there, as the bytecode texts leave it), and the
// nest-host failure suffix of a private refusal.

import java.lang.classfile.ClassBuilder;
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.Map;
import java.util.function.Consumer;

public class R21MhnAccessTexts {
    static final class Inner {
        private int f;
        private Inner(int x) { }
        Inner() { }
        private void im() { }
        private static void ims() { }
    }

    interface Find { Object run() throws Throwable; }

    static String norm(String s) {
        if (s == null) return "null";
        return s.replaceAll("@[0-9a-f]+", "@H").replaceAll("/0x[0-9a-f]+", "/0xH");
    }

    static void row(String label, Find f) {
        StringBuilder sb = new StringBuilder("row ").append(label).append(":");
        try {
            f.run();
            sb.append(" ok");
        } catch (Throwable e) {
            for (Throwable t = e; t != null; t = t.getCause()) {
                sb.append(" | ").append(t.getClass().getName()).append(": ").append(norm(t.getMessage()));
                if (t.getCause() == t) break;
            }
        }
        System.out.println(sb);
    }

    static final MethodType V = MethodType.methodType(void.class);

    static final class GenLoader extends ClassLoader {
        final Map<String, byte[]> bytes;
        GenLoader(Map<String, byte[]> bytes) {
            super("r21", R21MhnAccessTexts.class.getClassLoader());
            this.bytes = bytes;
        }
        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            byte[] b = bytes.get(name);
            if (b == null) throw new ClassNotFoundException(name);
            return defineClass(name, b, 0, b.length);
        }
    }

    static final ClassDesc CD_D = ClassDesc.of("p.D");
    static final int PUB = ClassFile.ACC_PUBLIC, PRI = ClassFile.ACC_PRIVATE,
            PRO = ClassFile.ACC_PROTECTED, STA = ClassFile.ACC_STATIC;

    static void ctor(ClassBuilder cb, ClassDesc sup, MethodTypeDesc type, int flags) {
        cb.withMethodBody(ConstantDescs.INIT_NAME, type, flags, cob -> cob.aload(0)
                .invokespecial(sup, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void).return_());
    }

    static void voidMethod(ClassBuilder cb, String name, int flags) {
        cb.withMethodBody(name, ConstantDescs.MTD_void, flags, cob -> cob.return_());
    }

    static void lookupMethod(ClassBuilder cb) {
        cb.withMethodBody("lk", MethodTypeDesc.of(ConstantDescs.CD_MethodHandles_Lookup), PUB | STA,
                cob -> cob.invokestatic(ConstantDescs.CD_MethodHandles, "lookup",
                        MethodTypeDesc.of(ConstantDescs.CD_MethodHandles_Lookup)).areturn());
    }

    static byte[] gen(String name, ClassDesc sup, Consumer<ClassBuilder> body) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(PUB | ClassFile.ACC_SUPER);
            cb.withSuperclass(sup);
            body.accept(cb);
        });
    }

    static MethodHandles.Lookup lk(Class<?> c) throws Throwable {
        return (MethodHandles.Lookup) c.getMethod("lk").invoke(null);
    }

    static void userRows() throws Throwable {
        Map<String, byte[]> m = new HashMap<>();
        m.put("p.D", gen("p.D", ConstantDescs.CD_Object, cb -> {
            ctor(cb, ConstantDescs.CD_Object, ConstantDescs.MTD_void, PUB);
            ctor(cb, ConstantDescs.CD_Object, MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_int), PRI);
            ctor(cb, ConstantDescs.CD_Object, MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_long), 0);
            ctor(cb, ConstantDescs.CD_Object, MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_String), PRO);
            voidMethod(cb, "priv", PRI);
            voidMethod(cb, "pkg", 0);
            voidMethod(cb, "prot", PRO);
            voidMethod(cb, "privS", PRI | STA);
            voidMethod(cb, "pkgS", STA);
            voidMethod(cb, "protS", PRO | STA);
            cb.withField("pf", ConstantDescs.CD_int, PRI);
            cb.withField("kf", ConstantDescs.CD_int, 0);
            cb.withField("tf", ConstantDescs.CD_int, PRO);
        }));
        m.put("q.C", gen("q.C", ConstantDescs.CD_Object, cb -> {
            ctor(cb, ConstantDescs.CD_Object, ConstantDescs.MTD_void, PUB);
            lookupMethod(cb);
        }));
        m.put("q.S", gen("q.S", CD_D, cb -> {
            ctor(cb, CD_D, ConstantDescs.MTD_void, PUB);
            lookupMethod(cb);
        }));
        GenLoader loader = new GenLoader(m);
        Class<?> d = loader.loadClass("p.D");
        for (String caller : new String[] {"q.C", "q.S"}) {
            MethodHandles.Lookup l = lk(loader.loadClass(caller));
            String c = caller + "->p.D";
            row(c + " findVirtual private", () -> l.findVirtual(d, "priv", V));
            row(c + " findVirtual package", () -> l.findVirtual(d, "pkg", V));
            row(c + " findVirtual protected", () -> l.findVirtual(d, "prot", V));
            row(c + " findStatic private", () -> l.findStatic(d, "privS", V));
            row(c + " findStatic package", () -> l.findStatic(d, "pkgS", V));
            row(c + " findStatic protected", () -> l.findStatic(d, "protS", V));
            row(c + " findConstructor private", () -> l.findConstructor(d, MethodType.methodType(void.class, int.class)));
            row(c + " findConstructor package", () -> l.findConstructor(d, MethodType.methodType(void.class, long.class)));
            row(c + " findConstructor protected", () -> l.findConstructor(d, MethodType.methodType(void.class, String.class)));
            row(c + " findGetter private", () -> l.findGetter(d, "pf", int.class));
            row(c + " findGetter package", () -> l.findGetter(d, "kf", int.class));
            row(c + " findGetter protected", () -> l.findGetter(d, "tf", int.class));
        }
    }

    static void jdkRows() {
        MethodHandles.Lookup l = MethodHandles.lookup();
        row("jdk findVirtual private ArrayList.grow", () -> l.findVirtual(ArrayList.class, "grow",
                MethodType.methodType(Object[].class, int.class)));
        row("jdk findVirtual package String.coder", () -> l.findVirtual(String.class, "coder",
                MethodType.methodType(byte.class)));
        row("jdk findVirtual protected ClassLoader.findLoadedClass", () -> l.findVirtual(ClassLoader.class,
                "findLoadedClass", MethodType.methodType(Class.class, String.class)));
        row("jdk findConstructor private Void", () -> l.findConstructor(Void.class, V));
        row("jdk findGetter private String.hash", () -> l.findGetter(String.class, "hash", int.class));
    }

    static void nestRows() throws Throwable {
        MethodHandles.Lookup self = MethodHandles.lookup();
        MethodHandles.Lookup other = R21MhnOther.lk();
        for (Object[] side : new Object[][] {{"nestmate", self}, {"other", other}}) {
            String s = (String) side[0];
            MethodHandles.Lookup l = (MethodHandles.Lookup) side[1];
            row(s + " findVirtual private Inner.im", () -> l.findVirtual(Inner.class, "im", V));
            row(s + " findStatic private Inner.ims", () -> l.findStatic(Inner.class, "ims", V));
            row(s + " findConstructor private Inner(int)", () -> l.findConstructor(Inner.class,
                    MethodType.methodType(void.class, int.class)));
            row(s + " findGetter private Inner.f", () -> l.findGetter(Inner.class, "f", int.class));
        }
    }

    static void hiddenRows() throws Throwable {
        byte[] b = gen("R21MhnHid", ConstantDescs.CD_Object, cb -> {
            ctor(cb, ConstantDescs.CD_Object, ConstantDescs.MTD_void, PUB);
            voidMethod(cb, "hm", PRI);
            lookupMethod(cb);
        });
        MethodHandles.Lookup hl = MethodHandles.lookup().defineHiddenClass(b, true);
        Class<?> h = hl.lookupClass();
        MethodHandles.Lookup other = R21MhnOther.lk();
        row("other->hidden findVirtual private hm", () -> other.findVirtual(h, "hm", V));
        row("hidden->Inner findVirtual private im", () -> hl.findVirtual(Inner.class, "im", V));
    }

    public static void main(String[] args) throws Throwable {
        userRows();
        jdkRows();
        nestRows();
        hiddenRows();
        System.out.println("done");
    }
}

class R21MhnOther {
    static MethodHandles.Lookup lk() {
        return MethodHandles.lookup();
    }
}
