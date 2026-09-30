// Interpreter round i1, wave 12, lane L2 — JVMS §5.4.3 at a COMPILED type check.
//
// A generated class `l2gen.Holder`, defined by a custom loader, has
// `static boolean test(Object o) { return o instanceof l2gen.Gone; }`.
// `l2gen.Gone` exists nowhere: the loader counts every loadClass("l2gen.Gone")
// and throws ClassNotFoundException. `test` runs 50 000 times on a non-null
// receiver, long enough for the JIT to compile it; the compile door cannot
// bind the site (the class is not loaded), so compiled code resolves it at run
// time through the holder's loader.
//
// JVMS §5.4.3: the first failure (NoClassDefFoundError) is recorded against
// the constant-pool entry, and every later execution — interpreted or
// compiled — rethrows it WITHOUT asking the loader again. Before wave 12 the
// compiled site recorded nothing and drove `loadClass` on every execution
// (docs/internal/fixed-bugs/interpreter-L2-compiled-typecheck-does-not-record-resolution-failures-FIXED-20260925.md).
//
// No setup: the bytecode is generated with java.lang.classfile (final since
// JDK 24), so no class file for `l2gen.Gone` exists for any loader to find.
//
// Expected HotSpot 25 stdout:
//   errors=50000 answers=0
//   error class: java.lang.NoClassDefFoundError
//   one message: true
//   loadClass(l2gen.Gone) calls=1

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L2CompiledCastFailureRecorded {
    static int goneCalls;

    static final class CountingLoader extends ClassLoader {
        private final byte[] holderBytes;

        CountingLoader(ClassLoader parent, byte[] holderBytes) {
            super(parent);
            this.holderBytes = holderBytes;
        }

        @Override
        protected synchronized Class<?> loadClass(String name, boolean resolve)
                throws ClassNotFoundException {
            Class<?> c = findLoadedClass(name);
            if (c != null) return c;
            if (name.equals("l2gen.Holder")) {
                return defineClass(name, holderBytes, 0, holderBytes.length);
            }
            if (name.equals("l2gen.Gone")) {
                goneCalls++;
                throw new ClassNotFoundException(name);
            }
            return super.loadClass(name, resolve);
        }
    }

    public static void main(String[] args) throws Throwable {
        byte[] bytes = ClassFile.of().build(ClassDesc.of("l2gen.Holder"), clb -> clb
                .withFlags(ClassFile.ACC_PUBLIC)
                .withMethodBody("test",
                        MethodTypeDesc.of(ConstantDescs.CD_boolean, ConstantDescs.CD_Object),
                        ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                        cb -> cb.aload(0).instanceOf(ClassDesc.of("l2gen.Gone")).ireturn()));
        CountingLoader loader =
                new CountingLoader(L2CompiledCastFailureRecorded.class.getClassLoader(), bytes);
        Class<?> holder = Class.forName("l2gen.Holder", true, loader);
        MethodHandle test = MethodHandles.lookup().findStatic(holder, "test",
                MethodType.methodType(boolean.class, Object.class));

        Object receiver = "x";
        int errors = 0;
        int answers = 0;
        String errorClass = null;
        String firstMessage = null;
        boolean oneMessage = true;
        for (int i = 0; i < 50_000; i++) {
            try {
                boolean r = (boolean) test.invokeExact(receiver);
                answers++;
            } catch (NoClassDefFoundError e) {
                errors++;
                if (errorClass == null) {
                    errorClass = e.getClass().getName();
                    firstMessage = String.valueOf(e.getMessage());
                } else if (!firstMessage.equals(String.valueOf(e.getMessage()))) {
                    oneMessage = false;
                }
            }
        }
        System.out.println("errors=" + errors + " answers=" + answers);
        System.out.println("error class: " + errorClass);
        System.out.println("one message: " + oneMessage);
        System.out.println("loadClass(l2gen.Gone) calls=" + goneCalls);
    }
}
