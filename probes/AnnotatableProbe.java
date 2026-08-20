import org.junit.runners.model.*;
import java.lang.annotation.Annotation;
import java.lang.reflect.Method;
import java.lang.reflect.Field;

public class AnnotatableProbe {
    // force the call to go through the interface, exactly as JUnit's
    // generic AnnotatableValidator<T extends Annotatable> does after erasure
    static void viaInterface(String label, Annotatable a) {
        try {
            Annotation[] anns = a.getAnnotations();
            System.out.println("OK   " + label + " -> " + anns.length + " annotation(s), impl="
                + a.getClass().getName());
        } catch (Throwable t) {
            System.out.println("FAIL " + label + " -> " + t.getClass().getName() + ": " + t.getMessage());
        }
    }
    public static class Sample { @Deprecated public void m() {} public int fld; }
    public static void main(String[] args) throws Exception {
        TestClass tc = new TestClass(Sample.class);
        viaInterface("TestClass", tc);
        Method m = Sample.class.getMethod("m");
        viaInterface("FrameworkMethod", new FrameworkMethod(m));
        Field f = Sample.class.getField("fld");
        viaInterface("FrameworkField", new FrameworkField(f));
    }
}
