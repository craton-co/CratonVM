import static org.mockito.Mockito.*;
import java.lang.reflect.Method;
import org.springframework.beans.factory.support.DefaultListableBeanFactory;
import org.springframework.beans.factory.support.DefaultSingletonBeanRegistry;

public class SpyDLBFProbe {
    public static void main(String[] args) throws Exception {
        System.out.println("STEP1 creating real DefaultListableBeanFactory");
        DefaultListableBeanFactory real = new DefaultListableBeanFactory();
        System.out.println("STEP2 spying it (triggers retransformClasses on the whole hierarchy)");
        DefaultListableBeanFactory spy;
        try {
            spy = spy(real);
        } catch (Throwable t) {
            System.out.println("SPY CREATION FAILED: " + t);
            t.printStackTrace(System.out);
            return;
        }
        System.out.println("STEP3 spy created: " + spy);

        System.out.println("STEP4 POST-RETRANSFORM reflection check:");
        Method m1 = DefaultListableBeanFactory.class.getMethod("registerSingleton", String.class, Object.class);
        System.out.println("  DefaultListableBeanFactory.class.getMethod(...).getDeclaringClass() = " + m1.getDeclaringClass().getName());

        Method m2 = DefaultSingletonBeanRegistry.class.getMethod("registerSingleton", String.class, Object.class);
        System.out.println("  DefaultSingletonBeanRegistry.class.getMethod(...).getDeclaringClass() = " + m2.getDeclaringClass().getName());

        System.out.println("  spy.getClass() = " + spy.getClass().getName());
        Method m3 = spy.getClass().getMethod("registerSingleton", String.class, Object.class);
        System.out.println("  spy.getClass().getMethod(...).getDeclaringClass() = " + m3.getDeclaringClass().getName());

        boolean declaredDirectly = false;
        for (Method dm : spy.getClass().getDeclaredMethods()) {
            if (dm.getName().equals("registerSingleton") && dm.getParameterCount() == 2) {
                declaredDirectly = true;
                System.out.println("  Found in spy.getClass().getDeclaredMethods(): " + dm);
            }
        }
        System.out.println("  declaredDirectly=" + declaredDirectly + " (expected true)");
        System.out.println("STEP5 now calling spy.registerSingleton(...) -- this is expected to either work or StackOverflowError");
        try {
            spy.registerSingleton("testBean", "hello");
            System.out.println("STEP6 registerSingleton returned normally -- NO BUG in this isolated case");
            System.out.println("STEP7 containsSingleton=" + spy.containsSingleton("testBean"));
        } catch (StackOverflowError soe) {
            System.out.println("STEP6 StackOverflowError caught -- BUG REPRODUCED in isolation");
        }
        System.out.println("ALL DONE");
    }
}
