import java.lang.reflect.*;
import java.util.*;
import org.junit.runner.*;
import org.junit.runners.model.*;

public class VintageProbe {
    public static void main(String[] args) throws Exception {
        String clsName = args.length > 0 ? args[0]
            : "org.apache.commons.math4.transform.TransformUtilsTest";
        Class<?> c = Class.forName(clsName);
        System.out.println("CLASS=" + c.getName());
        System.out.println("org.junit.Test ih=" + System.identityHashCode(org.junit.Test.class));

        // 1) raw getAnnotations() per method, checking org.junit.Test
        for (Method m : c.getDeclaredMethods()) {
            boolean has = m.isAnnotationPresent(org.junit.Test.class);
            java.lang.annotation.Annotation[] ga = m.getAnnotations();
            System.out.println("M " + m.getName() + " getAnnotations.len=" + ga.length
                + " isAnnotationPresent(org.junit.Test)=" + has);
        }

        // 2) JUnit4 TestClass model
        TestClass tc = new TestClass(c);
        List<FrameworkMethod> tms = tc.getAnnotatedMethods(org.junit.Test.class);
        System.out.println("TestClass.getAnnotatedMethods(org.junit.Test)=" + tms.size());
        for (FrameworkMethod fm : tms) System.out.println("  fm-> " + fm.getName());

        // 3) Full JUnit4 Request -> Runner -> Description
        try {
            Request req = Request.aClass(c);
            Runner runner = req.getRunner();
            System.out.println("Runner class=" + runner.getClass().getName());
            Description d = runner.getDescription();
            System.out.println("Runner desc=" + d.getDisplayName()
                + " childCount=" + d.getChildren().size()
                + " testCount=" + runner.testCount());
            for (Description child : d.getChildren())
                System.out.println("  desc-> " + child.getDisplayName() + " isTest=" + child.isTest());
        } catch (Throwable t) {
            System.out.println("RUNNER-EXC: " + t);
            t.printStackTrace();
        }
    }
}
