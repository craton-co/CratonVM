import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

// Residual shapes of r19-mh-interface-handle-runs-a-non-implementors-same-named-method:
// every way a non-implementing receiver can reach an interface `findVirtual` handle, cold
// (first call) and warm (after 2000 good calls), each row printed with its exception text.
// Must match HotSpot 25 byte for byte, both modes.
public class R19MhifaceResid {
    interface Fn {
        int apply(int x);

        default int twice(int x) { return 2 * apply(x); }
    }

    static final class Good implements Fn {
        public int apply(int x) { return x + 1; }
    }

    public static final class Impostor {
        public int apply(int x) { return 999; }

        public int twice(int x) { return 888; }
    }

    static final MethodHandle APPLY;
    static final MethodHandle TWICE;
    static final MethodHandle UNREFLECTED;

    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            MethodType ii = MethodType.methodType(int.class, int.class);
            APPLY = l.findVirtual(Fn.class, "apply", ii);
            TWICE = l.findVirtual(Fn.class, "twice", ii);
            UNREFLECTED = l.unreflect(Fn.class.getMethod("apply", int.class));
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static String describe(Throwable t) {
        return t.getClass().getName() + ": " + t.getMessage();
    }

    // static int call(Object o, int i) { return (int) <H>.invokeExact((Fn) <o unchecked>, i); }
    static MethodHandle generated(String name, String field) throws Throwable {
        ClassDesc self = ClassDesc.of("R19MhifaceResid");
        ClassDesc fn = ClassDesc.of("R19MhifaceResid$Fn");
        ClassDesc mh = ClassDesc.of("java.lang.invoke.MethodHandle");
        MethodTypeDesc site = MethodTypeDesc.of(ConstantDescs.CD_int, fn, ConstantDescs.CD_int);
        MethodTypeDesc call = MethodTypeDesc.of(ConstantDescs.CD_int, ConstantDescs.CD_Object,
                ConstantDescs.CD_int);
        byte[] bytes = ClassFile.of().build(ClassDesc.of(name), cb -> cb
                .withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_FINAL)
                .withMethodBody("call", call, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> code
                        .getstatic(self, field, mh)
                        .aload(0)
                        .iload(1)
                        .invokevirtual(mh, "invokeExact", site)
                        .ireturn()));
        Class<?> gen = MethodHandles.lookup().defineClass(bytes);
        return MethodHandles.lookup().findStatic(gen, "call",
                MethodType.methodType(int.class, Object.class, int.class));
    }

    interface Caller {
        int call(Object o, int i) throws Throwable;
    }

    static String row(Caller c, Object bad, int n) {
        int thrown = 0;
        String first = "none";
        long sum = 0;
        for (int i = 0; i < n; i++) {
            try {
                sum += c.call(bad, i);
            } catch (Throwable t) {
                if (thrown++ == 0) first = describe(t);
            }
        }
        return "throws=" + thrown + " sum=" + sum + " first=" + first;
    }

    static void both(String name, Caller c, Object bad) {
        System.out.println(name + " cold " + row(c, bad, 1));
        Object good = new Good();
        for (int k = 0; k < 2000; k++) row(c, good, 10);
        System.out.println(name + " warm " + row(c, bad, 500));
        System.out.println(name + " good " + row(c, good, 10));
    }

    public static void main(String[] a) throws Throwable {
        MethodHandle gApply = generated("R19MhifaceResidGenA", "APPLY");
        MethodHandle gTwice = generated("R19MhifaceResidGenB", "TWICE");
        MethodHandle gUnref = generated("R19MhifaceResidGenC", "UNREFLECTED");
        both("impostor", (o, i) -> (int) gApply.invokeExact(o, i), new Impostor());
        both("plain", (o, i) -> (int) gApply.invokeExact(o, i), "a string");
        both("array", (o, i) -> (int) gApply.invokeExact(o, i), new int[3]);
        both("default", (o, i) -> (int) gTwice.invokeExact(o, i), new Impostor());
        both("unreflect", (o, i) -> (int) gUnref.invokeExact(o, i), new Impostor());
        MethodHandle widened = APPLY.asType(MethodType.methodType(int.class, Object.class, int.class));
        both("astype", (o, i) -> (int) widened.invokeExact(o, i), new Impostor());
        both("invoke", (o, i) -> (int) APPLY.invoke(o, i), new Impostor());
        both("withargs", (o, i) -> (int) APPLY.invokeWithArguments(o, i), new Impostor());
        both("bindto", (o, i) -> (int) APPLY.bindTo(o).invokeExact(i), new Impostor());
        MethodHandle spread = APPLY.asSpreader(Object[].class, 2);
        both("spread", (o, i) -> (int) spread.invoke(new Object[] { o, i }), new Impostor());
    }
}
