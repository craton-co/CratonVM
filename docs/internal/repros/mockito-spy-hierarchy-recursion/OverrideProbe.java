import java.lang.reflect.Method;
import org.springframework.beans.factory.support.DefaultListableBeanFactory;
import org.springframework.beans.factory.support.DefaultSingletonBeanRegistry;

public class OverrideProbe {
    public static void main(String[] args) throws Exception {
        // What class does Class.getMethod() say ACTUALLY declares
        // registerSingleton(String,Object) for a DefaultListableBeanFactory
        // receiver? Should be DefaultListableBeanFactory (it overrides the
        // inherited DefaultSingletonBeanRegistry implementation).
        Method m1 = DefaultListableBeanFactory.class.getMethod("registerSingleton", String.class, Object.class);
        System.out.println("DefaultListableBeanFactory.class.getMethod(...).getDeclaringClass() = " + m1.getDeclaringClass().getName());

        Method m2 = DefaultSingletonBeanRegistry.class.getMethod("registerSingleton", String.class, Object.class);
        System.out.println("DefaultSingletonBeanRegistry.class.getMethod(...).getDeclaringClass() = " + m2.getDeclaringClass().getName());

        boolean sameMethodObjectDeclaringClass = m1.getDeclaringClass().equals(m2.getDeclaringClass());
        System.out.println("Are the two declaring classes EQUAL? " + sameMethodObjectDeclaringClass + " (expected: false -- DLBF overrides it)");

        // Direct check: does DefaultListableBeanFactory itself declare
        // registerSingleton (i.e. getDeclaredMethods contains it)?
        boolean declaredDirectly = false;
        for (Method dm : DefaultListableBeanFactory.class.getDeclaredMethods()) {
            if (dm.getName().equals("registerSingleton")
                    && dm.getParameterCount() == 2) {
                declaredDirectly = true;
                System.out.println("Found in DefaultListableBeanFactory.getDeclaredMethods(): " + dm);
            }
        }
        System.out.println("declaredDirectly=" + declaredDirectly + " (expected true)");

        System.out.println("ALL DONE");
    }
}
