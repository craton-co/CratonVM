// Interpreter round i1, wave 25, lane L5 — JVMS §5.4.4 access control at
// FIELD and METHOD resolution: a `getstatic` / `putstatic` / `invokestatic` /
// `getfield` naming a member the referencing class may not access is an
// `IllegalAccessError`, and so is a member reference whose OWNER class is not
// accessible (a package-private class of another package).
//
// javac never emits such bytecode, so the classes are generated with the
// java.lang.classfile API (final since JDK 24) and defined by one loader:
// `l5acc.Secret` (public; private / package-private statics and a private
// static method), `l5acc.Hidden` (package-private class, public static field)
// and `l5other.Peek` (another package), whose `applyAsInt(k)` performs access
// number k. Only the exception CLASS is printed: HotSpot's message names the
// loader with an identity hash.
//
// Run (no setup):
//   javac -d out L5W25MemberAccessProbe.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W25MemberAccessProbe
//
// Expected HotSpot 25 output (compare verbatim):
//   private getstatic: java.lang.IllegalAccessError
//   private putstatic: java.lang.IllegalAccessError
//   private invokestatic: java.lang.IllegalAccessError
//   package getstatic: java.lang.IllegalAccessError
//   private getfield: java.lang.IllegalAccessError
//   hidden-owner getstatic: java.lang.IllegalAccessError
//   public getstatic: 11
//
// CratonVM (from the code, not run): member access is not checked on the
// bytecode resolution path (`classloading/src/access_control.rs`, "STATUS":
// `check_field_access` / `check_method_access` have no caller; field and
// method resolution apply `AccessPolicy::ModuleOnly`), and the owner class of
// a member reference gets no §5.4.4 class check, so every row but the last
// prints the member's value (7, 0, 8, 9, then a NullPointerException for the
// null receiver of the getfield row, 10). See
// docs/internal/fixed-bugs/interpreter-L5-member-access-is-not-checked-at-field-and-method-resolution-FIXED-20260930.md.
//
// Wave 26 (lane L5): the default mode (`--jdk-only`) enforces both checks on
// every resolution miss and should print HotSpot's seven rows; `--compatible`
// still admits (and counts: `CRATONVM_DBG=access` traces five
// `[ACCESS-DBG] MEMBER ADMIT` lines — the putstatic row shares the getstatic
// row's `Fieldref`, resolved and cached by then) and prints the values
// above. HotSpot's
// full messages, for reference (`<L>` = `L5W25MemberAccessProbe$Loader
// @<hash>`, which CratonVM leaves out for a user-defined loader):
//   class l5other.Peek tried to access private field l5acc.Secret.P (l5other.Peek and l5acc.Secret are in unnamed module of loader <L>)
//   class l5other.Peek tried to access private method 'int l5acc.Secret.p()' (...)
//   class l5other.Peek tried to access field l5acc.Secret.Q (...)
//   failed to access class l5acc.Hidden from class l5other.Peek (l5acc.Hidden and l5other.Peek are in unnamed module of loader <L>)

import java.lang.classfile.ClassFile;
import java.lang.classfile.CodeBuilder;
import java.lang.classfile.Label;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.util.HashMap;
import java.util.Map;
import java.util.function.IntUnaryOperator;

public class L5W25MemberAccessProbe {
    static final ClassDesc SECRET = ClassDesc.of("l5acc.Secret");
    static final ClassDesc HIDDEN = ClassDesc.of("l5acc.Hidden");
    static final MethodTypeDesc INT = MethodTypeDesc.of(ConstantDescs.CD_int);
    static final MethodTypeDesc INT_INT =
            MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_int);
    static final int PUBLIC = ClassFile.ACC_PUBLIC;
    static final int STATIC = ClassFile.ACC_STATIC;
    static final int PRIVATE = ClassFile.ACC_PRIVATE;

    static byte[] secret() {
        return ClassFile.of().build(SECRET, clb -> clb
                .withFlags(PUBLIC)
                .withField("P", ConstantDescs.CD_int, PRIVATE | STATIC)
                .withField("Q", ConstantDescs.CD_int, STATIC)
                .withField("F", ConstantDescs.CD_int, PUBLIC | STATIC)
                .withField("f", ConstantDescs.CD_int, PRIVATE)
                .withMethodBody("p", INT, PRIVATE | STATIC, cb -> cb.bipush(8).ireturn())
                .withMethodBody(ConstantDescs.CLASS_INIT_NAME, ConstantDescs.MTD_void, STATIC,
                        cb -> cb.bipush(7).putstatic(SECRET, "P", ConstantDescs.CD_int)
                                .bipush(9).putstatic(SECRET, "Q", ConstantDescs.CD_int)
                                .bipush(11).putstatic(SECRET, "F", ConstantDescs.CD_int)
                                .return_()));
    }

    static byte[] hidden() {
        return ClassFile.of().build(HIDDEN, clb -> clb
                .withFlags(0)
                .withField("H", ConstantDescs.CD_int, PUBLIC | STATIC)
                .withMethodBody(ConstantDescs.CLASS_INIT_NAME, ConstantDescs.MTD_void, STATIC,
                        cb -> cb.bipush(10).putstatic(HIDDEN, "H", ConstantDescs.CD_int)
                                .return_()));
    }

    /// `applyAsInt(k)`: a `tableswitch` over the seven accesses.
    static byte[] peek() {
        return ClassFile.of().build(ClassDesc.of("l5other.Peek"), clb -> clb
                .withFlags(PUBLIC)
                .withInterfaceSymbols(ClassDesc.of("java.util.function.IntUnaryOperator"))
                .withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, PUBLIC,
                        cb -> cb.aload(0)
                                .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                        ConstantDescs.MTD_void)
                                .return_())
                .withMethodBody("applyAsInt", INT_INT, PUBLIC, cb -> {
                    Label[] arms = new Label[7];
                    for (int i = 0; i < arms.length; i++) {
                        arms[i] = cb.newLabel();
                    }
                    Label dflt = cb.newLabel();
                    java.util.List<java.lang.classfile.instruction.SwitchCase> cases =
                            new java.util.ArrayList<>();
                    for (int i = 0; i < arms.length; i++) {
                        cases.add(java.lang.classfile.instruction.SwitchCase.of(i, arms[i]));
                    }
                    cb.iload(1).tableswitch(0, arms.length - 1, dflt, cases);
                    arm(cb, arms[0], c -> c.getstatic(SECRET, "P", ConstantDescs.CD_int));
                    arm(cb, arms[1], c -> c.iconst_1().putstatic(SECRET, "P", ConstantDescs.CD_int)
                            .iconst_0());
                    arm(cb, arms[2], c -> c.invokestatic(SECRET, "p", INT));
                    arm(cb, arms[3], c -> c.getstatic(SECRET, "Q", ConstantDescs.CD_int));
                    arm(cb, arms[4], c -> c.aconst_null().getfield(SECRET, "f", ConstantDescs.CD_int));
                    arm(cb, arms[5], c -> c.getstatic(HIDDEN, "H", ConstantDescs.CD_int));
                    arm(cb, arms[6], c -> c.getstatic(SECRET, "F", ConstantDescs.CD_int));
                    cb.labelBinding(dflt).iconst_m1().ireturn();
                }));
    }

    static void arm(CodeBuilder cb, Label at, java.util.function.Consumer<CodeBuilder> body) {
        cb.labelBinding(at);
        body.accept(cb);
        cb.ireturn();
    }

    static final class Loader extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();

        Loader() {
            super(ClassLoader.getPlatformClassLoader());
            bytes.put("l5acc.Secret", secret());
            bytes.put("l5acc.Hidden", hidden());
            bytes.put("l5other.Peek", peek());
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

    public static void main(String[] args) throws Exception {
        IntUnaryOperator peek = (IntUnaryOperator) new Loader().loadClass("l5other.Peek")
                .getConstructor().newInstance();
        String[] labels = {
            "private getstatic", "private putstatic", "private invokestatic",
            "package getstatic", "private getfield", "hidden-owner getstatic",
            "public getstatic",
        };
        for (int k = 0; k < labels.length; k++) {
            try {
                System.out.println(labels[k] + ": " + peek.applyAsInt(k));
            } catch (Throwable t) {
                System.out.println(labels[k] + ": " + t.getClass().getName());
            }
        }
    }
}
