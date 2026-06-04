import java.lang.reflect.*;
import java.lang.annotation.*;
import org.junit.platform.commons.support.*;
import org.junit.jupiter.api.Test;

public class AnnProbe4 {
    public static void main(String[] args) throws Exception {
        String clsName = args.length > 0 ? args[0]
            : "org.apache.commons.math4.transform.TransformUtilsTest";
        Class<?> c = Class.forName(clsName);
        System.out.println("CLASS=" + c.getName() + " modifiers=0x" + Integer.toHexString(c.getModifiers()));
        System.out.println("Test.class loader=" + Test.class.getClassLoader());
        System.out.println("Test.class identityHash=" + System.identityHashCode(Test.class));

        // Which Test.class does the test method actually reference?
        for (Method m : c.getDeclaredMethods()) {
            Annotation[] da = m.getDeclaredAnnotations();
            StringBuilder sb = new StringBuilder();
            for (Annotation a : da) {
                Class<?> at = a.annotationType();
                sb.append(at.getName())
                  .append("(ldr=").append(at.getClassLoader())
                  .append(",ih=").append(System.identityHashCode(at))
                  .append(",==Test.class:").append(at == Test.class)
                  .append(") ");
            }
            boolean isAnn = AnnotationSupport.isAnnotated(m, Test.class);
            System.out.println("M " + m.getName()
                + " mod=0x" + Integer.toHexString(m.getModifiers())
                + " declAnns=[" + sb.toString().trim() + "]"
                + " isAnnotated(Test)=" + isAnn);
        }

        var found = AnnotationSupport.findAnnotatedMethods(c, Test.class, HierarchyTraversalMode.TOP_DOWN);
        System.out.println("findAnnotatedMethods(Test, TOP_DOWN)=" + found.size());
        for (Method m : found) System.out.println("  -> " + m.getName());

        // also check @Testable meta-annotation presence on Test itself
        System.out.println("Test annotations: ");
        for (Annotation a : Test.class.getDeclaredAnnotations())
            System.out.println("  @" + a.annotationType().getName());
        System.out.println("isAnnotated(Test.class, Testable-by-name check)");
    }
}
