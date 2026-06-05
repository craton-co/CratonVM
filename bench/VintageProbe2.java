import java.lang.reflect.*;
import java.util.*;
import org.junit.runner.*;
import org.junit.runners.model.*;

public class VintageProbe2 {
    public static void main(String[] args) throws Exception {
        String clsName = args.length > 0 ? args[0]
            : "org.apache.commons.math4.transform.TransformUtilsTest";
        Class<?> c = Class.forName(clsName);
        Request req = Request.aClass(c);
        Runner runner = req.getRunner();
        System.out.println("Runner=" + runner.getClass().getName());

        for (int i = 0; i < 3; i++) {
            Description d = runner.getDescription();
            System.out.println("call#" + i + " getDescription children=" + d.getChildren().size()
                + " testCount=" + d.testCount() + " isTest=" + d.isTest()
                + " isSuite=" + d.isSuite());
        }

        // Inspect computeTestMethods / getChildren via reflection on the runner
        try {
            Method gc = findMethod(runner.getClass(), "getChildren");
            gc.setAccessible(true);
            Object kids = gc.invoke(runner);
            System.out.println("getChildren() -> " + kids.getClass().getName()
                + " size=" + ((Collection<?>) kids).size());
            for (Object k : (Collection<?>) kids) System.out.println("  child=" + k);
        } catch (Throwable t) {
            System.out.println("getChildren reflect EXC: " + t);
        }

        // Inspect describeChild for one child
        try {
            Method cm = findMethod(runner.getClass(), "computeTestMethods");
            cm.setAccessible(true);
            Object kids = cm.invoke(runner);
            System.out.println("computeTestMethods() size=" + ((Collection<?>) kids).size());
        } catch (Throwable t) {
            System.out.println("computeTestMethods reflect EXC: " + t);
        }
    }

    static Method findMethod(Class<?> c, String name) throws Exception {
        for (Class<?> k = c; k != null; k = k.getSuperclass()) {
            for (Method m : k.getDeclaredMethods())
                if (m.getName().equals(name) && m.getParameterCount() == 0) return m;
        }
        throw new NoSuchMethodException(name);
    }
}
