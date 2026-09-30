// Interpreter round i1, wave 26, lane L5 — the edges of JVMS §5.4.4 at FIELD
// and METHOD resolution, beside `L5W25MemberAccessProbe`: what must still be
// ADMITTED now that `--jdk-only` refuses inaccessible members (protected
// access from a subclass in another package, through `this` and through the
// declaring class's name; a protected static inherited under the subclass's
// name; private access between two nested classes whose nest host was never
// loaded), and what must be REFUSED (protected members from a non-subclass,
// a package-private static from a class of the same package NAME but another
// loader, a public member of a package-private class of another package —
// the owner-class half), including in a method hot enough to be compiled.
//
// The `p` / `q` classes are generated with the java.lang.classfile API and
// defined by one loader; `p.Split` by a child loader that delegates `p.C` to
// the first. `L5W26Nest` is ordinary javac output: since JDK 11 javac emits a
// direct `invokestatic L5W26Nest$B.secret` in `L5W26Nest$A` (nestmates), and
// nothing here loads `L5W26Nest` itself. Only exception CLASSES are printed:
// HotSpot's messages name user loaders with an identity hash.
//
// Run (no setup):
//   javac -d out L5W26MemberAccessEdges.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W26MemberAccessEdges
// (the default mode, `--jdk-only`, is the one that enforces the check;
// `--compatible` admits and counts, and prints the pre-wave-26 values for
// the seven refused rows — 21, 22, 7, 12, 6, 13, 7 — and
// `compiled refusals: 0`).
//
// Expected HotSpot 25 output (compare verbatim):
//   protected via this: 5
//   protected via declaring name: 5
//   protected static via subclass name: 12
//   protected virtual via declaring name: 13
//   hidden-owner getstatic: java.lang.IllegalAccessError
//   hidden-owner invokestatic: java.lang.IllegalAccessError
//   package static from other package: java.lang.IllegalAccessError
//   protected static from non-subclass: java.lang.IllegalAccessError
//   protected static field from non-subclass: java.lang.IllegalAccessError
//   protected virtual from non-subclass: java.lang.IllegalAccessError
//   package static from other loader: java.lang.IllegalAccessError
//   nest siblings, host not loaded: 42
//   compiled refusals: 30000
//
// CratonVM before wave 26 (from the code, not run): every refused row printed
// its value and `compiled refusals: 0`. The admitted rows are the false
// denials a careless check would introduce; `nest siblings` in particular
// needs the unloaded-nest-host rule (`field_access::unvalidated_common_nest_host`).
// See docs/internal/fixed-bugs/interpreter-L5-member-access-is-not-checked-at-field-and-method-resolution-FIXED-20260930.md.

import java.lang.classfile.ClassFile;
import java.lang.classfile.CodeBuilder;
import java.lang.classfile.Label;
import java.lang.classfile.instruction.SwitchCase;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.function.Consumer;
import java.util.function.IntUnaryOperator;

public class L5W26MemberAccessEdges {
    static final ClassDesc C = ClassDesc.of("p.C");
    static final ClassDesc PKG = ClassDesc.of("p.Pkg");
    static final ClassDesc S = ClassDesc.of("q.S");
    static final ClassDesc IUO = ClassDesc.of("java.util.function.IntUnaryOperator");
    static final MethodTypeDesc INT = MethodTypeDesc.of(ConstantDescs.CD_int);
    static final MethodTypeDesc INT_INT =
            MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_int);
    static final int PUBLIC = ClassFile.ACC_PUBLIC;
    static final int PROTECTED = ClassFile.ACC_PROTECTED;
    static final int STATIC = ClassFile.ACC_STATIC;
    static final ClassDesc I = ConstantDescs.CD_int;

    /// `p.C`: protected instance field `f` (5), protected static `sf` (6),
    /// package-private static `pf` (7), protected static `sm()` (12),
    /// protected `im()` (13).
    static byte[] c() {
        return ClassFile.of().build(C, clb -> clb
                .withFlags(PUBLIC)
                .withField("f", I, PROTECTED)
                .withField("sf", I, PROTECTED | STATIC)
                .withField("pf", I, STATIC)
                .withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, PUBLIC,
                        cb -> cb.aload(0)
                                .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                        ConstantDescs.MTD_void)
                                .aload(0).iconst_5().putfield(C, "f", I)
                                .return_())
                .withMethodBody("sm", INT, PROTECTED | STATIC, cb -> cb.bipush(12).ireturn())
                .withMethodBody("im", INT, PROTECTED, cb -> cb.bipush(13).ireturn())
                .withMethodBody(ConstantDescs.CLASS_INIT_NAME, ConstantDescs.MTD_void, STATIC,
                        cb -> cb.bipush(6).putstatic(C, "sf", I)
                                .bipush(7).putstatic(C, "pf", I)
                                .return_()));
    }

    /// `p.Pkg`: a package-private class with a public static field `x` (21)
    /// and a public static method `m()` (22).
    static byte[] pkg() {
        return ClassFile.of().build(PKG, clb -> clb
                .withFlags(0)
                .withField("x", I, PUBLIC | STATIC)
                .withMethodBody("m", INT, PUBLIC | STATIC, cb -> cb.bipush(22).ireturn())
                .withMethodBody(ConstantDescs.CLASS_INIT_NAME, ConstantDescs.MTD_void, STATIC,
                        cb -> cb.bipush(21).putstatic(PKG, "x", I).return_()));
    }

    static void arm(CodeBuilder cb, Label at, Consumer<CodeBuilder> body) {
        cb.labelBinding(at);
        body.accept(cb);
        cb.ireturn();
    }

    /// A public `IntUnaryOperator` whose `applyAsInt(k)` runs `arms[k]`.
    static byte[] operator(ClassDesc name, ClassDesc superclass,
                           List<Consumer<CodeBuilder>> arms) {
        return ClassFile.of().build(name, clb -> clb
                .withFlags(PUBLIC)
                .withSuperclass(superclass)
                .withInterfaceSymbols(IUO)
                .withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, PUBLIC,
                        cb -> cb.aload(0)
                                .invokespecial(superclass, ConstantDescs.INIT_NAME,
                                        ConstantDescs.MTD_void)
                                .return_())
                .withMethodBody("applyAsInt", INT_INT, PUBLIC, cb -> {
                    Label[] labels = new Label[arms.size()];
                    List<SwitchCase> cases = new ArrayList<>();
                    for (int i = 0; i < labels.length; i++) {
                        labels[i] = cb.newLabel();
                        cases.add(SwitchCase.of(i, labels[i]));
                    }
                    Label dflt = cb.newLabel();
                    cb.iload(1).tableswitch(0, labels.length - 1, dflt, cases);
                    for (int i = 0; i < labels.length; i++) {
                        arm(cb, labels[i], arms.get(i));
                    }
                    cb.labelBinding(dflt).iconst_m1().ireturn();
                }));
    }

    /// `q.S extends p.C`: the admitted protected shapes, then two owner-class
    /// refusals and a package-private static of another package.
    static byte[] s() {
        return operator(S, C, List.of(
                cb -> cb.aload(0).getfield(S, "f", I),
                cb -> cb.aload(0).getfield(C, "f", I),
                cb -> cb.invokestatic(S, "sm", INT),
                cb -> cb.aload(0).invokevirtual(C, "im", INT),
                cb -> cb.getstatic(PKG, "x", I),
                cb -> cb.invokestatic(PKG, "m", INT),
                cb -> cb.getstatic(C, "pf", I)));
    }

    /// `q.Other` (not a subclass of `p.C`): protected members are refused.
    static byte[] other() {
        return operator(ClassDesc.of("q.Other"), ConstantDescs.CD_Object, List.of(
                cb -> cb.invokestatic(C, "sm", INT),
                cb -> cb.getstatic(C, "sf", I),
                cb -> cb.new_(C).dup()
                        .invokespecial(C, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                        .invokevirtual(C, "im", INT)));
    }

    /// `p.Split`, defined by the child loader: same package NAME as `p.C`,
    /// another runtime package.
    static byte[] split() {
        return operator(ClassDesc.of("p.Split"), ConstantDescs.CD_Object, List.of(
                cb -> cb.getstatic(C, "pf", I)));
    }

    static class Loader extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();

        Loader(ClassLoader parent) {
            super(parent);
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            byte[] b = bytes.get(name);
            if (b == null) {
                throw new ClassNotFoundException(name);
            }
            return defineClass(name, b, 0, b.length);
        }
    }

    static void row(String label, IntUnaryOperator op, int k) {
        try {
            System.out.println(label + ": " + op.applyAsInt(k));
        } catch (Throwable t) {
            System.out.println(label + ": " + t.getClass().getName());
        }
    }

    static IntUnaryOperator make(ClassLoader loader, String name) throws Exception {
        return (IntUnaryOperator) loader.loadClass(name).getConstructor().newInstance();
    }

    public static void main(String[] args) throws Exception {
        Loader one = new Loader(ClassLoader.getPlatformClassLoader());
        one.bytes.put("p.C", c());
        one.bytes.put("p.Pkg", pkg());
        one.bytes.put("q.S", s());
        one.bytes.put("q.Other", other());
        Loader two = new Loader(one);
        two.bytes.put("p.Split", split());

        IntUnaryOperator s = make(one, "q.S");
        row("protected via this", s, 0);
        row("protected via declaring name", s, 1);
        row("protected static via subclass name", s, 2);
        row("protected virtual via declaring name", s, 3);
        row("hidden-owner getstatic", s, 4);
        row("hidden-owner invokestatic", s, 5);
        row("package static from other package", s, 6);

        IntUnaryOperator other = make(one, "q.Other");
        row("protected static from non-subclass", other, 0);
        row("protected static field from non-subclass", other, 1);
        row("protected virtual from non-subclass", other, 2);

        row("package static from other loader", make(two, "p.Split"), 0);

        System.out.println("nest siblings, host not loaded: " + L5W26Nest.A.f());

        // Hot enough for the compiler: a compiled `applyAsInt` must not bind
        // the refused `invokestatic p/C.sm` by name.
        int refusals = 0;
        for (int i = 0; i < 30000; i++) {
            try {
                other.applyAsInt(0);
            } catch (IllegalAccessError e) {
                refusals++;
            }
        }
        System.out.println("compiled refusals: " + refusals);
    }
}

class L5W26Nest {
    static class A {
        static int f() {
            return B.secret();
        }
    }

    static class B {
        private static int secret() {
            return 42;
        }
    }
}
