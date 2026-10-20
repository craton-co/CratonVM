// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 84, lane L7: JVMS 5.4.3.3 / 5.4.3.4 / 5.4.6 and
// 6.5 invoke selection on class shapes javac will not compile, built at run
// time with the java.lang.classfile API (final in JDK 24) and defined with
// Lookup.defineClass in this probe's own (unnamed) package. Every call is
// made FOUR times from the same site so the first (linking) call and the
// cached paths (invoke cache, inline caches, the fast doors) are compared:
// a row prints `xN` only when all four calls agreed.
//
//   privateIface   an interface's default method calls its own PRIVATE
//                  method (javac: invokeinterface) on a receiver whose class
//                  declares a public method of the same name and descriptor:
//                  the private method is the selected one (6.5).
//   superGrand     `invokespecial SA.m` naming a NON-direct superclass while
//                  the direct superclass SB overrides m: ACC_SUPER semantics
//                  select from the direct superclass, so SB.m runs (6.5).
//   superSkipStatic `invokespecial SA.m` where the intermediate class declares
//                  a STATIC m: selection skips it, SA.m runs (HotSpot's
//                  lookup_instance_method_in_klasses).
//   virtSkipStatic `invokevirtual SA.m` on the same receiver: a static method
//                  never overrides (5.4.5), SA.m runs.
//   conflict       a class implementing two interfaces with the same default:
//                  IncompatibleClassChangeError ("Conflicting default
//                  methods: ...") at the call, not at load.
//   masked         an abstract re-declaration in a subinterface masks the
//                  superinterface default (5.4.3.3 maximally-specific):
//                  AbstractMethodError, through either interface.
//   notPublic      invokeinterface selecting a package-private implementation:
//                  IllegalAccessError (6.5 invokeinterface).
//   privateHider   a PRIVATE method in the receiver class does not override
//                  an interface default (5.4.5): the default runs.
//
// Messages are not printed (HotSpot's differ between the linking call and
// the itable stub for notPublic); for the conflict row only its prefix.
//
// Run (both modes; with and without --nojit):
//   cratonvm --java-home <jdk25> --nojit -cp <dir> L7W84InvokeSelection
//   cratonvm --java-home <jdk25> --compatible --nojit -cp <dir> L7W84InvokeSelection
// HOTSPOT_EXPECTED_BEGIN (JDK 25, derived from linkResolver.cpp /
// klassVtable.cpp / defaultMethods.cpp, not yet run)
// privateIface PI.p x4
// superGrand SB.m x4
// superSkipStatic SA.m x4
// virtSkipStatic SA.m x4
// conflict java.lang.IncompatibleClassChangeError prefix=true x4
// masked/IA java.lang.AbstractMethodError x4
// masked/IB java.lang.AbstractMethodError x4
// notPublic java.lang.IllegalAccessError x4
// privateHider ID.m x4
// HOTSPOT_EXPECTED_END

import java.lang.classfile.ClassBuilder;
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandles;
import java.util.List;
import java.util.function.Consumer;
import java.util.function.Supplier;

public class L7W84InvokeSelection {
    // ── privateIface ─────────────────────────────────────────────────────
    public interface PI {
        private String p() {
            return "PI.p";
        }

        default String q() {
            return p();
        }
    }

    public static class PC implements PI {
        public String p() {
            return "PC.p";
        }
    }

    // ── superGrand / superSkipStatic / virtSkipStatic ───────────────────
    public interface Caller {
        String callA();
    }

    public static class SA {
        public String m() {
            return "SA.m";
        }
    }

    public static class SB extends SA {
        @Override
        public String m() {
            return "SB.m";
        }
    }

    // ── conflict ────────────────────────────────────────────────────────
    public interface I1 {
        default String m() {
            return "I1.m";
        }
    }

    public interface I2 {
        default String m() {
            return "I2.m";
        }
    }

    // ── masked ──────────────────────────────────────────────────────────
    public interface IA {
        default String m() {
            return "IA.m";
        }
    }

    public interface IB extends IA {
        @Override
        String m();
    }

    // ── notPublic ───────────────────────────────────────────────────────
    public interface IP {
        String m();
    }

    // ── privateHider ────────────────────────────────────────────────────
    public interface ID {
        default String m() {
            return "ID.m";
        }
    }

    static final MethodTypeDesc MTD_STRING = MethodTypeDesc.of(ConstantDescs.CD_String);

    static ClassDesc nested(String simple) {
        return ClassDesc.of("L7W84InvokeSelection$" + simple);
    }

    /** A public class `name` extending `sup`, implementing `ifaces`, with a
     *  public no-arg constructor and whatever `body` adds. */
    static Class<?> define(String name, ClassDesc sup, List<ClassDesc> ifaces, Consumer<ClassBuilder> body)
            throws Exception {
        byte[] bytes = ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withSuperclass(sup);
            if (!ifaces.isEmpty()) {
                cb.withInterfaceSymbols(ifaces);
            }
            cb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(sup, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void)
                            .return_());
            body.accept(cb);
        });
        return MethodHandles.lookup().defineClass(bytes);
    }

    /** A method `name()Ljava/lang/String;` with `flags` returning `value`. */
    static Consumer<ClassBuilder> returning(String name, int flags, String value) {
        return cb -> cb.withMethodBody(name, MTD_STRING, flags, code -> code.ldc(value).areturn());
    }

    /** `public String callA() { return invokespecial SA.m(); }` */
    static final Consumer<ClassBuilder> CALL_A_SPECIAL = cb -> cb.withMethodBody("callA", MTD_STRING,
            ClassFile.ACC_PUBLIC,
            code -> code.aload(0).invokespecial(nested("SA"), "m", MTD_STRING).areturn());

    static Object instance(Class<?> c) throws Exception {
        return c.getConstructor().newInstance();
    }

    static void row(String label, Supplier<String> call) {
        String first = null;
        boolean same = true;
        for (int k = 0; k < 4; k++) {
            String got;
            try {
                got = call.get();
            } catch (Throwable t) {
                got = t.getClass().getName();
                if (t instanceof IncompatibleClassChangeError
                        && t.getClass() == IncompatibleClassChangeError.class) {
                    String msg = t.getMessage();
                    got += " prefix=" + (msg != null && msg.startsWith("Conflicting default methods:"));
                }
            }
            if (first == null) {
                first = got;
            } else if (!first.equals(got)) {
                same = false;
                System.out.println(label + " call " + k + " differs: " + got);
            }
        }
        System.out.println(label + " " + first + (same ? " x4" : " MIXED"));
    }

    public static void main(String[] args) throws Exception {
        PI pc = new PC();
        row("privateIface", pc::q);

        Caller grand = (Caller) instance(define("L7W84SelGrand", nested("SB"),
                List.of(nested("Caller")), CALL_A_SPECIAL));
        row("superGrand", grand::callA);

        define("L7W84SelStaticMid", nested("SA"), List.of(),
                returning("m", ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, "static"));
        Object leaf = instance(define("L7W84SelStaticLeaf", ClassDesc.of("L7W84SelStaticMid"),
                List.of(nested("Caller")), CALL_A_SPECIAL));
        row("superSkipStatic", ((Caller) leaf)::callA);
        SA leafAsA = (SA) leaf;
        row("virtSkipStatic", () -> leafAsA.m());

        Object both = instance(define("L7W84SelBoth", ConstantDescs.CD_Object,
                List.of(nested("I1"), nested("I2")), cb -> { }));
        I1 bothAs1 = (I1) both;
        row("conflict", () -> bothAs1.m());

        Object masked = instance(define("L7W84SelMasked", ConstantDescs.CD_Object,
                List.of(nested("IB")), cb -> { }));
        IA maskedA = (IA) masked;
        IB maskedB = (IB) masked;
        row("masked/IA", () -> maskedA.m());
        row("masked/IB", () -> maskedB.m());

        IP pkg = (IP) instance(define("L7W84SelPkg", ConstantDescs.CD_Object,
                List.of(nested("IP")), returning("m", 0, "pkg")));
        row("notPublic", () -> pkg.m());

        ID hider = (ID) instance(define("L7W84SelPrivHider", ConstantDescs.CD_Object,
                List.of(nested("ID")), returning("m", ClassFile.ACC_PRIVATE, "private")));
        row("privateHider", () -> hider.m());
    }
}
