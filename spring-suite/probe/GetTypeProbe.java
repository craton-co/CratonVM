// Isolate spring-bug-10 residual: BeanFactory.getType(name) returns null for a
// plain bean with an explicit concrete class. No XML, no AOP namespace — pure
// DefaultListableBeanFactory + RootBeanDefinition, to localize getType/resolveBeanClass.
import org.springframework.beans.factory.support.DefaultListableBeanFactory;
import org.springframework.beans.factory.support.RootBeanDefinition;

public class GetTypeProbe {
  public static void main(String[] a) throws Exception {
    String cls = a.length > 0 ? a[0] : "org.springframework.aop.aspectj.AdviceBindingTestAspect";

    // Sanity: can we even load the class directly?
    Class<?> direct;
    try { direct = Class.forName(cls); }
    catch (Throwable t) { direct = null; System.out.println("Class.forName THREW: " + t); }
    System.out.println("Class.forName(" + cls + ") = " + direct);

    // (1) bean class given as a STRING (the XML path → lazy resolveBeanClass)
    DefaultListableBeanFactory bf1 = new DefaultListableBeanFactory();
    RootBeanDefinition bdS = new RootBeanDefinition();
    bdS.setBeanClassName(cls);
    bf1.registerBeanDefinition("testAspect", bdS);
    System.out.println("[className String] getBeanClassName = " + bf1.getBeanDefinition("testAspect").getBeanClassName());

    ClassLoader bcl = bf1.getBeanClassLoader();
    System.out.println("[drill] getBeanClassLoader = " + bcl);
    try { System.out.println("[drill] ClassUtils.forName(cls, bcl) = " + org.springframework.util.ClassUtils.forName(cls, bcl)); }
    catch (Throwable t) { System.out.println("[drill] ClassUtils.forName THREW: " + t); }

    // Direct call to the PUBLIC AbstractBeanDefinition.resolveBeanClass(ClassLoader)
    try {
      org.springframework.beans.factory.support.AbstractBeanDefinition abd =
          (org.springframework.beans.factory.support.AbstractBeanDefinition) bf1.getBeanDefinition("testAspect");
      System.out.println("[drill] bd.hasBeanClass(before) = " + abd.hasBeanClass());
      System.out.println("[drill] bd.resolveBeanClass(bcl) = " + abd.resolveBeanClass(bcl));
      System.out.println("[drill] bd.hasBeanClass(after)  = " + abd.hasBeanClass());
    } catch (Throwable t) { System.out.println("[drill] bd.resolveBeanClass THREW: " + t); }

    System.out.println("[className String] getType BEFORE getBean = " + safeType(bf1, "testAspect"));
    try { Object inst = bf1.getBean("testAspect"); System.out.println("[drill] getBean = " + inst.getClass().getName()); }
    catch (Throwable t) { System.out.println("[drill] getBean THREW: " + t); }
    System.out.println("[className String] getType AFTER getBean = " + safeType(bf1, "testAspect"));

    // (2) bean class given as a resolved Class (already-resolved path)
    if (direct != null) {
      DefaultListableBeanFactory bf2 = new DefaultListableBeanFactory();
      RootBeanDefinition bdC = new RootBeanDefinition(direct);
      bf2.registerBeanDefinition("testAspect", bdC);
      System.out.println("[Class object]    getType(testAspect) = " + safeType(bf2, "testAspect"));
    }
  }

  static Object safeType(DefaultListableBeanFactory bf, String name) {
    try { return bf.getType(name); }
    catch (Throwable t) { return "THREW: " + t; }
  }
}
