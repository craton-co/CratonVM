// Clean-room repro of the spring-bug-10 residual: AbstractBeanDefinition.resolveBeanClass
// returns null though getBeanClassName() (called directly) returns the right string.
// Mirrors the exact code shape: volatile Object field + pattern-instanceof ternary getter,
// and a caller that branches on the getter's null-ness.
public class Mini {
  volatile Object beanClass;   // like AbstractBeanDefinition.beanClass

  String getBeanClassName() {
    Object o = this.beanClass;                       // defensive volatile read
    return (o instanceof Class<?> c ? c.getName() : (String) o);
  }

  Class<?> resolveBeanClass(ClassLoader cl) throws ClassNotFoundException {
    String name = getBeanClassName();
    System.out.println("    [inside resolveBeanClass] name=" + name);
    if (name == null) {
      return null;
    }
    Class<?> r = Class.forName(name, false, cl);
    this.beanClass = r;
    return r;
  }

  public static void main(String[] a) throws Exception {
    Mini m = new Mini();
    m.beanClass = "java.util.ArrayList";
    System.out.println("getBeanClassName (direct) = " + m.getBeanClassName());
    System.out.println("resolveBeanClass          = " + m.resolveBeanClass(Mini.class.getClassLoader()));
  }
}
