import java.lang.reflect.Method;
import java.util.List;
import kotlin.reflect.KFunction;
import kotlin.reflect.KParameter;
import kotlin.reflect.KType;
import kotlin.reflect.jvm.ReflectJvmMapping;

/**
 * Isolation probe: call kotlin-reflect directly on MethodParameterKotlinTests
 * methods and print the raw KFunction/KParameter/KType facts that Spring's
 * MethodParameter.KotlinDelegate relies on. Compare CratonVM vs HotSpot.
 */
public class KReflectProbe {
    public static void main(String[] args) throws Exception {
        Class<?> c = Class.forName("org.springframework.core.MethodParameterKotlinTests");
        for (String name : new String[]{"nullable", "nonNullable", "withDefaultValue", "suspendFun", "suspendFun2", "suspendFun5"}) {
            Method m = pick(c, name);
            System.out.println("==== " + name + " :: jvm=" + m.toGenericString());
            KFunction<?> f = ReflectJvmMapping.getKotlinFunction(m);
            if (f == null) { System.out.println("   getKotlinFunction = NULL"); continue; }
            System.out.println("   isSuspend=" + f.isSuspend());
            KType rt = f.getReturnType();
            System.out.println("   returnType=" + rt + " markedNullable=" + rt.isMarkedNullable()
                    + " classifier=" + rt.getClassifier() + " args=" + rt.getArguments()
                    + " javaType=" + safe(rt));
            List<KParameter> ps = f.getParameters();
            for (KParameter p : ps) {
                System.out.println("   param[" + p.getIndex() + "] kind=" + p.getKind()
                        + " name=" + p.getName() + " optional(hasDefault)=" + p.isOptional()
                        + " type=" + p.getType() + " typeMarkedNullable=" + p.getType().isMarkedNullable());
            }
        }
    }
    static Method pick(Class<?> c, String name) {
        for (Method m : c.getDeclaredMethods()) if (m.getName().equals(name)) return m;
        throw new RuntimeException("no method " + name);
    }
    static String safe(KType t) {
        try { return String.valueOf(ReflectJvmMapping.getJavaType(t)); }
        catch (Throwable e) { return "<" + e.getClass().getSimpleName() + ">"; }
    }
}
