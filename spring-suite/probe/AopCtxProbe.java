// Reproduce the AfterThrowing 'testBean' loss outside JUnit: load the AOP XML
// via ClassPathXmlApplicationContext and inspect whether testBean's definition
// survives context refresh.
import org.springframework.context.support.ClassPathXmlApplicationContext;

public class AopCtxProbe {
  public static void main(String[] a) {
    String xml = a.length > 0 ? a[0]
        : "org/springframework/aop/aspectj/AfterThrowingAdviceBindingTests.xml";
    System.out.println("=== loading " + xml + " ===");
    try {
      ClassPathXmlApplicationContext ctx = new ClassPathXmlApplicationContext(xml);
      System.out.println("refresh OK");
      System.out.println("containsBeanDefinition(testBean) = " + ctx.containsBeanDefinition("testBean"));
      System.out.println("containsBeanDefinition(testAspect) = " + ctx.containsBeanDefinition("testAspect"));
      System.out.println("beanDefinitionCount = " + ctx.getBeanDefinitionCount());
      try { System.out.println("getBean(testBean) = " + ctx.getBean("testBean").getClass().getName()); }
      catch (Throwable t) { System.out.println("getBean(testBean) THREW: " + t); }
    } catch (Throwable t) {
      int d = 0;
      for (Throwable c = t; c != null && d < 12; c = c.getCause(), d++) {
        System.out.println("CAUSE[" + d + "]: " + c.getClass().getName() + ": " + c.getMessage());
      }
    }
  }
}
