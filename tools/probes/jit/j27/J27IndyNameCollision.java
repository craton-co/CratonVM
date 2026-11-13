import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.function.Supplier;

/**
 * TR16-2 / TR16-3 collision fixture: a REAL class named {@code <jit-indy>} (the spelling of the VM's
 * synthetic indy bridge frame) must stay visible in every stack view while a compiled capturing-lambda
 * site is live. Expected HotSpot 25 lines (three views x cold/hot; the class frame is the first one):
 * <pre>
 * fact throwable-cold &lt;jit-indy&gt;.t,J27IndyNameCollision.lambda$run$0,...
 * </pre>
 * Take the HotSpot run as the reference: {@code java -cp out J27IndyNameCollision > hs.txt}, then
 * {@code cratonvm [--compatible] [--nojit] --cp out J27IndyNameCollision | diff - hs.txt}.
 */
public class J27IndyNameCollision {
    static String top(StackTraceElement[] st, int skip) {
        StringBuilder sb = new StringBuilder();
        for (int i = skip; i < Math.min(st.length, skip + 3); i++) {
            sb.append(st[i].getClassName()).append('.').append(st[i].getMethodName()).append(',');
        }
        return sb.toString();
    }

    public static String dumpThrowable() { return top(new Throwable().getStackTrace(), 0); }
    public static String dumpThread() { return top(Thread.currentThread().getStackTrace(), 1); }
    public static String dumpWalker() {
        return StackWalker.getInstance().walk(s -> s.limit(3).map(f -> f.getClassName() + "." + f.getMethodName() + ",")
                .reduce("", String::concat));
    }

    static MethodHandle[] handles() throws Throwable {
        ClassDesc me = ClassDesc.of("J27IndyNameCollision");
        MethodTypeDesc str = MethodTypeDesc.of(ConstantDescs.CD_String);
        String[][] rows = {{"t", "dumpThrowable"}, {"th", "dumpThread"}, {"w", "dumpWalker"}};
        byte[] bytes = ClassFile.of().build(ClassDesc.of("<jit-indy>"), cb -> {
            for (String[] r : rows) {
                cb.withMethodBody(r[0], str, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                        c -> c.invokestatic(me, r[1], str).areturn());
            }
        });
        Class<?> k = MethodHandles.lookup().defineClass(bytes);
        MethodHandle[] out = new MethodHandle[rows.length];
        for (int i = 0; i < rows.length; i++) {
            out[i] = MethodHandles.lookup().findStatic(k, rows[i][0], MethodType.methodType(String.class));
        }
        return out;
    }

    static String run(MethodHandle mh) {
        // Capturing lambda: a fresh indy site that compiles to the bridge call once hot.
        Supplier<String> s = () -> {
            try {
                return (String) mh.invoke();
            } catch (Throwable e) {
                throw new RuntimeException(e);
            }
        };
        return s.get();
    }

    public static void main(String[] a) throws Throwable {
        MethodHandle[] hs = handles();
        String[] names = {"throwable", "thread", "walker"};
        for (int i = 0; i < hs.length; i++) {
            System.out.println("fact " + names[i] + "-cold " + run(hs[i]));
            String last = null;
            for (int n = 0; n < 40000; n++) last = run(hs[i]);
            System.out.println("fact " + names[i] + "-hot " + last);
        }
    }
}
