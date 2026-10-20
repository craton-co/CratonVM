// Interpreter round i1, wave 25, lane L5 — JVMS §5.4.3 for the CLASS of a
// member reference, the companion of `L5W24OwnerFailureRecord` (which covers
// `getstatic`): a failed owner resolution is recorded against the
// referencing class's `CONSTANT_Class` entry and rethrown by every later
// resolution through that entry, even after the class has become loadable.
//
// Each scenario gets a fresh `Flaky` loader, which refuses `l5gen.Opt` on its
// FIRST request and defines it on every later one; the using classes are
// defined by that loader, so `Opt` is resolved through it. The classes are
// generated with the java.lang.classfile API (final since JDK 24) so that
// `l5gen.Opt` exists nowhere on the application class path.
//
//   static   `invokestatic Opt.m()`: the second call rethrows; after another
//            class (`Other`, its own entry) has loaded `Opt`, the third still
//            rethrows.
//   shared   one `CONSTANT_Class` entry used by `ldc Opt.class` (fails first)
//            and then by `invokestatic Opt.m()`: the invoke rethrows the ldc's
//            record, because the record belongs to the class entry.
//   getfield `aconst_null; getfield Opt.f`: resolution precedes the null
//            check, so NoClassDefFoundError both times, never an NPE.
//   putstatic `putstatic Opt.X`.
//   virtual  `aconst_null; invokevirtual Opt.v()`: resolution precedes the
//            null check.
//
// Run (no setup):
//   javac -d out L5W25OwnerFailureRecordInvoke.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W25OwnerFailureRecordInvoke
//
// Expected HotSpot 25 output (compare verbatim; NCDFE = java.lang.NoClassDefFoundError:
// l5gen/Opt | cause=java.lang.ClassNotFoundException: l5gen.Opt, spelled out below):
//   static#0: java.lang.NoClassDefFoundError: l5gen/Opt | cause=java.lang.ClassNotFoundException: l5gen.Opt
//   static#1: java.lang.NoClassDefFoundError: l5gen/Opt | cause=java.lang.ClassNotFoundException: l5gen.Opt
//   other: 8
//   static#2: java.lang.NoClassDefFoundError: l5gen/Opt | cause=java.lang.ClassNotFoundException: l5gen.Opt
//   static requests: 2
//   shared#0: java.lang.NoClassDefFoundError: l5gen/Opt | cause=java.lang.ClassNotFoundException: l5gen.Opt
//   shared#1: java.lang.NoClassDefFoundError: l5gen/Opt | cause=java.lang.ClassNotFoundException: l5gen.Opt
//   shared requests: 1
//   getfield#0: java.lang.NoClassDefFoundError: l5gen/Opt | cause=java.lang.ClassNotFoundException: l5gen.Opt
//   getfield#1: java.lang.NoClassDefFoundError: l5gen/Opt | cause=java.lang.ClassNotFoundException: l5gen.Opt
//   getfield requests: 1
//   putstatic#0: java.lang.NoClassDefFoundError: l5gen/Opt | cause=java.lang.ClassNotFoundException: l5gen.Opt
//   putstatic#1: java.lang.NoClassDefFoundError: l5gen/Opt | cause=java.lang.ClassNotFoundException: l5gen.Opt
//   putstatic requests: 1
//   virtual#0: java.lang.NoClassDefFoundError: l5gen/Opt | cause=java.lang.ClassNotFoundException: l5gen.Opt
//   virtual#1: java.lang.NoClassDefFoundError: l5gen/Opt | cause=java.lang.ClassNotFoundException: l5gen.Opt
//   virtual requests: 1
//
// CratonVM before wave 25 (from the code, not run): no member resolver
// recorded an owner failure, so every `#1` row asked the loader again, which
// then defined `Opt`: `static#1: 8`, `shared#1: 8`, `putstatic#1: 0`,
// `getfield#1: java.lang.NullPointerException...`, request counts one higher.
// Wave 25 records and rethrows for the field opcodes and `invokestatic`.
// The merged wave-25 build (host, all four modes) still printed
//   shared#0: 0 / shared#1: 8 / shared requests: 2
// (the ldc, refused by its own loader, took the SIBLING loader's `Opt` that
// the `static` scenario had defined, from the flat fallback's lone-user-loader
// last resort) and
//   virtual#0/#1: java.lang.NullPointerException: Cannot invoke "l5gen.Opt.v()" because "null" is null | cause=null
//   virtual requests: 0
// Both fixed by the lane's follow-up (L5b); every row should now match. See
// docs/internal/fixed-bugs/interpreter-L5-field-and-method-owner-resolution-failures-are-not-recorded-FIXED-20260927.md
// and docs/internal/fixed-bugs/interpreter-L5-invokevirtual-does-not-resolve-an-unloaded-owner-before-the-null-check-FIXED-20260930.md.

import java.lang.classfile.ClassFile;
import java.lang.classfile.CodeBuilder;
import java.lang.classfile.Label;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.util.HashMap;
import java.util.Map;
import java.util.function.Consumer;
import java.util.function.IntUnaryOperator;

public class L5W25OwnerFailureRecordInvoke {
    static final ClassDesc OPT = ClassDesc.of("l5gen.Opt");
    static final MethodTypeDesc INT = MethodTypeDesc.of(ConstantDescs.CD_int);
    static final MethodTypeDesc INT_INT =
            MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_int);
    static final int PUBLIC = ClassFile.ACC_PUBLIC;

    static byte[] opt() {
        return ClassFile.of().build(OPT, clb -> clb
                .withFlags(PUBLIC)
                .withField("X", ConstantDescs.CD_int, PUBLIC | ClassFile.ACC_STATIC)
                .withField("f", ConstantDescs.CD_int, PUBLIC)
                .withMethodBody("m", INT, PUBLIC | ClassFile.ACC_STATIC,
                        cb -> cb.bipush(8).ireturn())
                .withMethodBody("v", INT, PUBLIC, cb -> cb.bipush(9).ireturn()));
    }

    /// A public `IntUnaryOperator` whose `applyAsInt(x)` is `body`.
    static byte[] user(String name, Consumer<CodeBuilder> body) {
        return ClassFile.of().build(ClassDesc.of(name), clb -> clb
                .withFlags(PUBLIC)
                .withInterfaceSymbols(ClassDesc.of("java.util.function.IntUnaryOperator"))
                .withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, PUBLIC,
                        cb -> cb.aload(0)
                                .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                        ConstantDescs.MTD_void)
                                .return_())
                .withMethodBody("applyAsInt", INT_INT, PUBLIC, body));
    }

    static final class Flaky extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();
        int optRequests;

        Flaky() {
            super(ClassLoader.getPlatformClassLoader());
            bytes.put("l5gen.Opt", opt());
            bytes.put("l5gen.Static", user("l5gen.Static",
                    cb -> cb.invokestatic(OPT, "m", INT).ireturn()));
            bytes.put("l5gen.Other", user("l5gen.Other",
                    cb -> cb.invokestatic(OPT, "m", INT).ireturn()));
            bytes.put("l5gen.Shared", user("l5gen.Shared", cb -> {
                Label call = cb.newLabel();
                cb.iload(1).ifne(call)
                        .ldc(OPT).pop().iconst_0().ireturn()
                        .labelBinding(call)
                        .invokestatic(OPT, "m", INT).ireturn();
            }));
            bytes.put("l5gen.GetField", user("l5gen.GetField",
                    cb -> cb.aconst_null().getfield(OPT, "f", ConstantDescs.CD_int).ireturn()));
            bytes.put("l5gen.PutStatic", user("l5gen.PutStatic",
                    cb -> cb.iload(1).putstatic(OPT, "X", ConstantDescs.CD_int)
                            .iconst_0().ireturn()));
            bytes.put("l5gen.Virtual", user("l5gen.Virtual",
                    cb -> cb.aconst_null().invokevirtual(OPT, "v", INT).ireturn()));
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                byte[] b = bytes.get(name);
                if (b == null) {
                    return super.loadClass(name, resolve);
                }
                if (name.equals("l5gen.Opt") && optRequests++ == 0) {
                    throw new ClassNotFoundException(name);
                }
                return defineClass(name, b, 0, b.length);
            }
        }

        IntUnaryOperator make(String name) throws Exception {
            return (IntUnaryOperator) loadClass(name).getConstructor().newInstance();
        }
    }

    static void row(String label, IntUnaryOperator op, int arg) {
        try {
            System.out.println(label + ": " + op.applyAsInt(arg));
        } catch (Throwable t) {
            Throwable c = t.getCause();
            System.out.println(label + ": " + t.getClass().getName() + ": " + t.getMessage()
                    + " | cause=" + (c == null ? "null" : c.getClass().getName() + ": " + c.getMessage()));
        }
    }

    public static void main(String[] args) throws Exception {
        Flaky a = new Flaky();
        IntUnaryOperator st = a.make("l5gen.Static");
        row("static#0", st, 0);
        row("static#1", st, 0);
        row("other", a.make("l5gen.Other"), 0);
        row("static#2", st, 0);
        System.out.println("static requests: " + a.optRequests);

        Flaky b = new Flaky();
        IntUnaryOperator shared = b.make("l5gen.Shared");
        row("shared#0", shared, 0);
        row("shared#1", shared, 1);
        System.out.println("shared requests: " + b.optRequests);

        Flaky c = new Flaky();
        IntUnaryOperator gf = c.make("l5gen.GetField");
        row("getfield#0", gf, 0);
        row("getfield#1", gf, 0);
        System.out.println("getfield requests: " + c.optRequests);

        Flaky d = new Flaky();
        IntUnaryOperator ps = d.make("l5gen.PutStatic");
        row("putstatic#0", ps, 5);
        row("putstatic#1", ps, 5);
        System.out.println("putstatic requests: " + d.optRequests);

        Flaky e = new Flaky();
        IntUnaryOperator vi = e.make("l5gen.Virtual");
        row("virtual#0", vi, 0);
        row("virtual#1", vi, 0);
        System.out.println("virtual requests: " + e.optRequests);
    }
}
