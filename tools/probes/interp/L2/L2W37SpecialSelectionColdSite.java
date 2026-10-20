// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 37, lane L2: the last half of item 1 of
// docs/internal/fixed-bugs/interpreter-L4-invokespecial-and-invokeinterface-selection-diverge-from-hotspot-FIXED-20261001.md
// (filed as i29-L4-invokespecial-...-20260930.md). Wave 36 bail-lists a
// caller once the INTERPRETER raises its `invokespecial` selection error;
// every row here reaches its erring site for the first time only after the
// caller has been compiled, so that mechanism never fires and the compiled
// door itself must refuse to bind the site.
//
//   entry-cold-branch     `call(i)`: `super.m()` (invokespecial SaB.m, SaB
//                         re-declares m abstract over SaA.m) only for
//                         i >= 20000, after 20 000 calls that returned "C":
//                         the method-entry doors (single-pass, IR tier,
//                         background tiered compile).
//   osr-cold-branch       one call of `loop(30000)`, whose loop takes the same
//                         erring `super.m()` only on its last iteration: the
//                         OSR doors (single-pass `compile_osr_body`, the
//                         optimizing OSR door).
//   splice-cold-branch    `drive(i)` calls the private `leaf(i)`, whose
//                         `super.m()` runs only for i >= 20000: an IR splice
//                         of `leaf` into `drive` (`resolve_inline_site_from`).
//   osr-grandparent-named `loop(30000)` whose last iteration runs
//                         `invokespecial GpA.m` (the GRANDPARENT) while the
//                         parent GpB overrides m: JVMS §6.5 selects GpB.m.
//                         The single-pass OSR door bound the constant-pool
//                         class's GpA.m.
//
// Run: javac -d out L2W37SpecialSelectionColdSite.java
//      cratonvm --java-home <jdk25> [--nojit|--compatible] -cp out L2W37SpecialSelectionColdSite
//
// Positive control (--jdk-only, the default): CRATONVM_DBG_JITC=1 prints at
// least one `[cratonvm-jitc] invokespecial-selection REFUSED` line naming
// `HotColdEntry.call`, `HotColdOsr.loop` / `HotColdGp.loop`, or
// `HotColdSplice.leaf` (the splice refusal prints `inline-resolve REFUSED
// ... invokespecial-selection-...`).
//
// Expected HotSpot 25 output (default and -Xint):
//   entry-cold-branch: AbstractMethodError=10000 C=20000 A=0 other=0
//   osr-cold-branch: AbstractMethodError
//   splice-cold-branch: AbstractMethodError=10000 C=20000 A=0 other=0
//   osr-grandparent-named: B
//
// `--compatible` keeps the lenient selection by design (item 1 of the page is
// `--jdk-only`); its interpreter answers `A` / `A=...` on the abstract rows.
// `osr-grandparent-named` is `B` in every mode (`--compatible`'s interpreter
// selects from the superclass too).
//
// Wave 37 host run of the first fix: `osr-grandparent-named: A` in default and
// `--compatible`. The site's grandparent `GpA` is neither a direct supertype
// of `HotColdGp` nor initiated by its loader, so `jit_known_class` answered
// nothing and every door bound `GpA.m` by name; `special_site_cp_class` now
// finds it on the caller's superclass chain.
import java.lang.classfile.ClassBuilder;
import java.lang.classfile.ClassFile;
import java.lang.classfile.Label;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.util.function.Consumer;

public class L2W37SpecialSelectionColdSite {
    public static class SaA {
        public String m() {
            return "A";
        }
    }

    public abstract static class SaB extends SaA {
        @Override
        public abstract String m();
    }

    public static class GpA {
        public String m() {
            return "A";
        }
    }

    public static class GpB extends GpA {
        @Override
        public String m() {
            return "B";
        }
    }

    public interface Cold {
        String call(int i);
    }

    public interface Looper {
        String loop(int n);
    }

    static final MethodTypeDesc STR = MethodTypeDesc.of(ConstantDescs.CD_String);
    static final MethodTypeDesc STR_INT = MethodTypeDesc.of(ConstantDescs.CD_String, ConstantDescs.CD_int);

    static byte[] build(String name, ClassDesc sup, Class<?> iface, Consumer<ClassBuilder> body) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withSuperclass(sup);
            cb.withInterfaceSymbols(ClassDesc.of(iface.getName()));
            cb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0).invokespecial(sup, ConstantDescs.INIT_NAME, ConstantDescs.MTD_void).return_());
            body.accept(cb);
        });
    }

    static final class Loader extends ClassLoader {
        Loader() {
            super(L2W37SpecialSelectionColdSite.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static final Loader LOADER = new Loader();

    static String cold(String label, Class<?> c) throws Exception {
        Cold o = (Cold) c.getConstructor().newInstance();
        int ame = 0;
        int cc = 0;
        int a = 0;
        int other = 0;
        for (int i = 0; i < 30_000; i++) {
            try {
                String r = o.call(i);
                if ("C".equals(r)) {
                    cc++;
                } else if ("A".equals(r)) {
                    a++;
                } else {
                    other++;
                }
            } catch (AbstractMethodError e) {
                ame++;
            }
        }
        return label + ": AbstractMethodError=" + ame + " C=" + cc + " A=" + a + " other=" + other;
    }

    static String once(String label, Class<?> c) throws Exception {
        Looper o = (Looper) c.getConstructor().newInstance();
        try {
            return label + ": " + o.loop(30_000);
        } catch (AbstractMethodError e) {
            return label + ": AbstractMethodError";
        }
    }

    // for (int i = 0; i < n; i++) { if (i == 29999) return <owner>.super.m(); } return "done";
    static void loopBody(ClassBuilder cb, ClassDesc owner) {
        cb.withMethodBody("loop", STR_INT, ClassFile.ACC_PUBLIC, code -> {
            Label top = code.newLabel();
            Label next = code.newLabel();
            Label end = code.newLabel();
            code.iconst_0().istore(2);
            code.labelBinding(top);
            code.iload(2).iload(1).if_icmpge(end);
            code.iload(2).sipush(29_999).if_icmpne(next);
            code.aload(0).invokespecial(owner, "m", STR).areturn();
            code.labelBinding(next);
            code.iinc(2, 1).goto_(top);
            code.labelBinding(end);
            code.ldc("done").areturn();
        });
    }

    public static void main(String[] args) throws Exception {
        ClassDesc sab = ClassDesc.of(SaB.class.getName());

        // if (i < 20000) return "C"; return super.m();
        Class<?> entry = LOADER.define("HotColdEntry", build("HotColdEntry", sab, Cold.class, cb -> {
            cb.withMethodBody("m", STR, ClassFile.ACC_PUBLIC, code -> code.ldc("X").areturn());
            cb.withMethodBody("call", STR_INT, ClassFile.ACC_PUBLIC, code -> {
                Label erring = code.newLabel();
                code.iload(1).sipush(20_000).if_icmpge(erring);
                code.ldc("C").areturn();
                code.labelBinding(erring);
                code.aload(0).invokespecial(sab, "m", STR).areturn();
            });
        }));
        System.out.println(cold("entry-cold-branch", entry));

        Class<?> osr = LOADER.define("HotColdOsr", build("HotColdOsr", sab, Looper.class, cb -> {
            cb.withMethodBody("m", STR, ClassFile.ACC_PUBLIC, code -> code.ldc("X").areturn());
            loopBody(cb, sab);
        }));
        System.out.println(once("osr-cold-branch", osr));

        // leaf(i): String r = "C"; if (i >= 20000) r = super.m(); return r;
        // call(i): return this.leaf(i);   (invokevirtual of a private method)
        ClassDesc splice = ClassDesc.of("HotColdSplice");
        Class<?> spliced = LOADER.define("HotColdSplice", build("HotColdSplice", sab, Cold.class, cb -> {
            cb.withMethodBody("m", STR, ClassFile.ACC_PUBLIC, code -> code.ldc("X").areturn());
            cb.withMethodBody("leaf", STR_INT, ClassFile.ACC_PRIVATE, code -> {
                Label skip = code.newLabel();
                code.ldc("C").astore(2);
                code.iload(1).sipush(20_000).if_icmplt(skip);
                code.aload(0).invokespecial(sab, "m", STR).astore(2);
                code.labelBinding(skip);
                code.aload(2).areturn();
            });
            cb.withMethodBody("call", STR_INT, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0).iload(1).invokevirtual(splice, "leaf", STR_INT).areturn());
        }));
        System.out.println(cold("splice-cold-branch", spliced));

        ClassDesc gpa = ClassDesc.of(GpA.class.getName());
        ClassDesc gpb = ClassDesc.of(GpB.class.getName());
        Class<?> gp = LOADER.define("HotColdGp", build("HotColdGp", gpb, Looper.class, cb -> {
            cb.withMethodBody("m", STR, ClassFile.ACC_PUBLIC, code -> code.ldc("X").areturn());
            loopBody(cb, gpa);
        }));
        System.out.println(once("osr-grandparent-named", gp));
    }
}
