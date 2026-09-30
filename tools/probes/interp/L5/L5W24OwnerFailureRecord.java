// Interpreter round i1, wave 24, lane L5 — JVMS §5.4.3 for the CLASS of a
// field or method reference: a failed owner resolution is recorded against
// the `Fieldref` / `Methodref` entry and rethrown, even after the class has
// become loadable.
//
// A loader (`Flaky`) refuses `l5gen.Opt` on its FIRST request and defines it
// on every later one. `l5gen.UseField` (`getstatic Opt.X`) and
// `l5gen.UseMethod` (`invokestatic Opt.m()`) are defined by that loader. The
// classes are generated with the java.lang.classfile API (final since JDK 24)
// so that `l5gen.Opt` exists nowhere on the application class path.
//
// HotSpot records the first failure per constant-pool entry
// (`SystemDictionary::add_resolution_error`), so every later execution of
// `UseField`'s `getstatic` rethrows it without asking the loader again, even
// after `UseMethod`'s own (different) entry resolved `Opt` successfully.
//
// Run (no setup):
//   javac -d out L5W24OwnerFailureRecord.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W24OwnerFailureRecord
//
// Expected HotSpot 25 output (compare verbatim):
//   field#0: java.lang.NoClassDefFoundError: l5gen/Opt | cause=java.lang.ClassNotFoundException: l5gen.Opt
//   field#1: java.lang.NoClassDefFoundError: l5gen/Opt | cause=java.lang.ClassNotFoundException: l5gen.Opt
//   method#0: 8
//   field#2: java.lang.NoClassDefFoundError: l5gen/Opt | cause=java.lang.ClassNotFoundException: l5gen.Opt
//   method#1: 8
//   opt requests: 2
//
// CratonVM before wave 25 (from the code, not run; the wave-24 host build
// differed as expected): the class-resolving opcodes (`new`, `ldc`, casts,
// `anewarray`, `multianewarray`, condy) recorded their failures, but no field
// or method resolver did, so `field#1` asked the loader again (request 2),
// which then defined `Opt`: `field#1: 0` and every later field row `0`,
// `opt requests: 2` reached one row early. Fixed in wave 25 (lane L5); see
// docs/internal/fixed-bugs/interpreter-L5-field-and-method-owner-resolution-failures-are-not-recorded-FIXED-20260927.md
// and its companion probe `L5W25OwnerFailureRecordInvoke`.

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.util.HashMap;
import java.util.Map;
import java.util.function.IntSupplier;

public class L5W24OwnerFailureRecord {
    static final ClassDesc OPT = ClassDesc.of("l5gen.Opt");
    static final MethodTypeDesc INT = MethodTypeDesc.of(ConstantDescs.CD_int);
    static final int PUBLIC = ClassFile.ACC_PUBLIC;

    static byte[] opt() {
        return ClassFile.of().build(OPT, clb -> clb
                .withFlags(PUBLIC)
                .withField("X", ConstantDescs.CD_int, PUBLIC | ClassFile.ACC_STATIC)
                .withMethodBody("m", INT, PUBLIC | ClassFile.ACC_STATIC,
                        cb -> cb.bipush(8).ireturn()));
    }

    /// A public `IntSupplier` whose `getAsInt` is `body`'s single access to `Opt`.
    static byte[] user(String name, boolean field) {
        return ClassFile.of().build(ClassDesc.of(name), clb -> clb
                .withFlags(PUBLIC)
                .withInterfaceSymbols(ClassDesc.of("java.util.function.IntSupplier"))
                .withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, PUBLIC,
                        cb -> cb.aload(0)
                                .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                        ConstantDescs.MTD_void)
                                .return_())
                .withMethodBody("getAsInt", INT, PUBLIC, cb -> {
                    if (field) {
                        cb.getstatic(OPT, "X", ConstantDescs.CD_int);
                    } else {
                        cb.invokestatic(OPT, "m", INT);
                    }
                    cb.ireturn();
                }));
    }

    static final class Flaky extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();
        int optRequests;

        Flaky() {
            super(ClassLoader.getPlatformClassLoader());
            bytes.put("l5gen.Opt", opt());
            bytes.put("l5gen.UseField", user("l5gen.UseField", true));
            bytes.put("l5gen.UseMethod", user("l5gen.UseMethod", false));
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
    }

    static void row(String label, IntSupplier s) {
        try {
            System.out.println(label + ": " + s.getAsInt());
        } catch (Throwable t) {
            Throwable c = t.getCause();
            System.out.println(label + ": " + t.getClass().getName() + ": " + t.getMessage()
                    + " | cause=" + (c == null ? "null" : c.getClass().getName() + ": " + c.getMessage()));
        }
    }

    public static void main(String[] args) throws Exception {
        Flaky loader = new Flaky();
        IntSupplier field = (IntSupplier) loader.loadClass("l5gen.UseField")
                .getConstructor().newInstance();
        IntSupplier method = (IntSupplier) loader.loadClass("l5gen.UseMethod")
                .getConstructor().newInstance();
        row("field#0", field);
        row("field#1", field);
        row("method#0", method);
        row("field#2", field);
        row("method#1", method);
        System.out.println("opt requests: " + loader.optRequests);
    }
}
