package cratonvm;

import java.lang.annotation.*;

@Retention(RetentionPolicy.RUNTIME)
@Target(ElementType.TYPE)
@interface CratonTestAnnotation {
    String value() default "hello";
    int count() default 0;
}

/**
 * Smoke test for the CRATONVM_REAL_ANNOTATIONS real-path gate.
 *
 * Run under CRATONVM_REAL_ANNOTATIONS=1 to verify that annotation dispatch
 * routes to real JDK bytecode instead of the synthetic overlay. The test
 * exercises getAnnotation, annotationType, and attribute reads on a
 * @Retention(RUNTIME) annotation, then prints the OK marker.
 */
@CratonTestAnnotation(value = "world", count = 7)
public class RealAnnotations {
    public static void main(String[] args) {
        CratonTestAnnotation a = RealAnnotations.class.getAnnotation(CratonTestAnnotation.class);
        System.out.println("r:present=" + (a != null));
        System.out.println("r:value=" + (a != null ? a.value() : "null"));
        System.out.println("r:count=" + (a != null ? a.count() : -1));
        System.out.println("r:type=" + (a != null ? a.annotationType().getSimpleName() : "null"));

        // Verify @Retention is itself accessible at runtime.
        Retention ret = CratonTestAnnotation.class.getAnnotation(Retention.class);
        System.out.println("r:retention=" + (ret != null ? ret.value().name() : "null"));

        System.out.println("REAL_ANNOTATIONS_OK 5");
    }
}
