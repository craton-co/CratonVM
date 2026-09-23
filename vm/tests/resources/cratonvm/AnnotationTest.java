package cratonvm;

import java.lang.annotation.*;
import java.lang.reflect.Method;

/**
 * Session 18: Annotation Processing tests.
 */
public class AnnotationTest {

    // --- Custom annotations ---

    @Retention(RetentionPolicy.RUNTIME)
    @Target(ElementType.TYPE)
    @Inherited
    @interface InheritedTag {
        String value() default "inherited-default";
    }

    @Retention(RetentionPolicy.RUNTIME)
    @Target(ElementType.TYPE)
    @interface NonInheritedTag {
        String value() default "non-inherited";
    }

    @Retention(RetentionPolicy.RUNTIME)
    @Target(ElementType.METHOD)
    @interface MethodInfo {
        String name();
        int priority() default 0;
        String description() default "none";
    }

    @Retention(RetentionPolicy.RUNTIME)
    @Target(ElementType.PARAMETER)
    @interface ParamTag {
        String value();
    }

    // --- Annotated classes ---

    @InheritedTag("base-value")
    @NonInheritedTag("base-only")
    static class Base {}

    static class Child extends Base {}

    @InheritedTag("overridden")
    static class OverridingChild extends Base {}

    // --- Annotated methods ---

    @MethodInfo(name = "doWork", priority = 5, description = "performs work")
    public static void annotatedMethod() {}

    @MethodInfo(name = "simple")
    public static void methodWithDefaults() {}

    public static void paramMethod(@ParamTag("first") int a, @ParamTag("second") String b) {}

    // --- Test methods (return int for harness) ---

    // Test 1: Custom annotation with values — Base has 2 annotations
    public static int testCustomAnnotationValues() {
        Annotation[] anns = Base.class.getAnnotations();
        return anns.length >= 2 ? 1 : 0;
    }

    // Test 2: @Inherited — Child inherits @InheritedTag from Base
    public static int testInheritedAnnotation() {
        boolean hasInherited = Child.class.isAnnotationPresent(InheritedTag.class);
        return hasInherited ? 1 : 0;
    }

    // Test 3: Non-@Inherited annotation is NOT inherited by Child
    public static int testNonInheritedNotPresent() {
        boolean hasNonInherited = Child.class.isAnnotationPresent(NonInheritedTag.class);
        return hasNonInherited ? 0 : 1; // should NOT be present
    }

    // Test 4: getAnnotation with @Inherited walks superclass chain
    public static int testGetInheritedAnnotation() {
        Annotation ann = Child.class.getAnnotation(InheritedTag.class);
        return ann != null ? 1 : 0;
    }

    // Test 5: getDeclaredAnnotations does NOT include inherited
    public static int testDeclaredAnnotationsNoInherited() {
        Annotation[] declared = Child.class.getDeclaredAnnotations();
        return declared.length == 0 ? 1 : 0;
    }

    // Test 6: OverridingChild has its own @InheritedTag (not Base's)
    public static int testOverridingInheritedAnnotation() {
        Annotation[] anns = OverridingChild.class.getDeclaredAnnotations();
        // Should have exactly 1: @InheritedTag("overridden")
        return anns.length == 1 ? 1 : 0;
    }

    // Test 7: Method annotation is present
    public static int testMethodAnnotationPresent() {
        try {
            Method m = AnnotationTest.class.getDeclaredMethod("annotatedMethod");
            Annotation[] anns = m.getDeclaredAnnotations();
            return anns.length == 1 ? 1 : 0;
        } catch (Exception e) {
            return 0;
        }
    }

    // Test 8: Method annotation proxies are identity-stable, while each
    // getDeclaredAnnotations call still returns a fresh defensive array.
    public static int testMethodAnnotationIdentity() {
        try {
            Method m = AnnotationTest.class.getDeclaredMethod("annotatedMethod");
            MethodInfo direct1 = m.getAnnotation(MethodInfo.class);
            MethodInfo direct2 = m.getAnnotation(MethodInfo.class);
            Annotation[] declared1 = m.getDeclaredAnnotations();
            Annotation[] declared2 = m.getDeclaredAnnotations();
            return direct1 != null
                && direct1 == direct2
                && declared1 != declared2
                && declared1.length == 1
                && declared2.length == 1
                && declared1.getClass() == Annotation[].class
                && declared1[0] == direct1
                && declared2[0] == direct1 ? 1 : 0;
        } catch (Exception e) {
            return 0;
        }
    }

    // Test 9: Method without annotation has empty array
    public static int testMethodNoAnnotation() {
        try {
            Method m = AnnotationTest.class.getDeclaredMethod("testCustomAnnotationValues");
            Annotation[] anns = m.getDeclaredAnnotations();
            return anns.length == 0 ? 1 : 0;
        } catch (Exception e) {
            return 0;
        }
    }

    // Test 10: getParameterAnnotations returns correct count
    public static int testParameterAnnotationCount() {
        try {
            Method m = AnnotationTest.class.getDeclaredMethod("paramMethod", int.class, String.class);
            Annotation[][] paramAnns = m.getParameterAnnotations();
            if (paramAnns.length != 2) return 0;
            if (paramAnns[0].length != 1) return 0;
            if (paramAnns[1].length != 1) return 0;
            return 1;
        } catch (Exception e) {
            return 0;
        }
    }

    // Test 11: getParameterAnnotations for unannotated method returns empty arrays
    public static int testParameterAnnotationEmpty() {
        try {
            Method m = AnnotationTest.class.getDeclaredMethod("annotatedMethod");
            Annotation[][] paramAnns = m.getParameterAnnotations();
            // annotatedMethod has 0 params
            return paramAnns.length == 0 ? 1 : 0;
        } catch (Exception e) {
            return 0;
        }
    }

    // --- Entry points for test harness ---

    public static void testCustom() { Util.tempPrint(testCustomAnnotationValues()); }
    public static void testInherited() { Util.tempPrint(testInheritedAnnotation()); }
    public static void testNonInherited() { Util.tempPrint(testNonInheritedNotPresent()); }
    public static void testGetInherited() { Util.tempPrint(testGetInheritedAnnotation()); }
    public static void testDeclaredOnly() { Util.tempPrint(testDeclaredAnnotationsNoInherited()); }
    public static void testOverrideInherited() { Util.tempPrint(testOverridingInheritedAnnotation()); }
    public static void testMethodAnn() { Util.tempPrint(testMethodAnnotationPresent()); }
    public static void testMethodNoAnn() { Util.tempPrint(testMethodNoAnnotation()); }
    public static void testParamCount() { Util.tempPrint(testParameterAnnotationCount()); }
    public static void testParamEmpty() { Util.tempPrint(testParameterAnnotationEmpty()); }
}
