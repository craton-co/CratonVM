// JAVA21+
package cratonvm;

import java.lang.annotation.*;
import java.lang.reflect.Method;

/**
 * Phase 88.4: Annotation runtime retention.
 */
public class ReflectAnnotation {

    @Retention(RetentionPolicy.RUNTIME)
    @Target(ElementType.TYPE)
    @interface MyTag {
        String value() default "default";
    }

    @Retention(RetentionPolicy.RUNTIME)
    @Target(ElementType.METHOD)
    @interface Info {
        String name();
        int priority() default 0;
    }

    @MyTag("test-class")
    static class Tagged {}

    static class Untagged {}

    @Info(name = "doWork", priority = 5)
    public static void annotatedMethod() {}

    public static void unannotatedMethod() {}

    // 88.4: Class has runtime annotation
    public static int testClassAnnotation() {
        Annotation[] anns = Tagged.class.getAnnotations();
        return anns.length > 0 ? 1 : 0;  // 1
    }

    // 88.4: Method has runtime annotation
    public static int testMethodAnnotation() throws Exception {
        Method m = ReflectAnnotation.class.getDeclaredMethod("annotatedMethod");
        Annotation[] anns = m.getDeclaredAnnotations();
        return anns.length > 0 ? 1 : 0;  // 1
    }

    // 88.4: Class without annotation returns empty array
    public static int testNoAnnotation() {
        Annotation[] anns = Untagged.class.getAnnotations();
        return anns.length == 0 ? 1 : 0;  // 1
    }

    // 88.4: isAnnotationPresent
    public static int testIsAnnotationPresent() {
        boolean present = Tagged.class.isAnnotationPresent(MyTag.class);
        boolean absent = Untagged.class.isAnnotationPresent(MyTag.class);
        return (present && !absent) ? 1 : 0;  // 1
    }
}
