import org.springframework.context.annotation.AnnotationConfigApplicationContext;
import org.springframework.context.annotation.Bean;
import org.springframework.context.annotation.Configuration;
import org.springframework.beans.factory.support.SimpleInstantiationStrategy;
import java.lang.reflect.Method;

public class CglibDiag {

    static class Engine { final int id; Engine(int id) { this.id = id; } }
    static class Car { final Engine engine; Car(Engine e) { this.engine = e; } }

    static int engineCtor = 0;

    static String cur() {
        Method m = SimpleInstantiationStrategy.getCurrentlyInvokedFactoryMethod();
        return m == null ? "null" : m.getName();
    }

    @Configuration
    static class Cfg {
        @Bean Engine engine() {
            System.out.println("[DIAG] ENTER engine(), currentlyInvoked=" + cur());
            Engine e = new Engine(engineCtor++);
            System.out.println("[DIAG] EXIT  engine(), engineCtor now=" + engineCtor);
            return e;
        }
        @Bean Car car1() {
            System.out.println("[DIAG] ENTER car1(), currentlyInvoked=" + cur());
            Car c = new Car(engine());
            System.out.println("[DIAG] EXIT  car1()");
            return c;
        }
        @Bean Car car2() {
            System.out.println("[DIAG] ENTER car2(), currentlyInvoked=" + cur());
            Car c = new Car(engine());
            System.out.println("[DIAG] EXIT  car2()");
            return c;
        }
    }

    public static void main(String[] args) {
        AnnotationConfigApplicationContext ctx = new AnnotationConfigApplicationContext(Cfg.class);
        Engine e = ctx.getBean(Engine.class);
        Car c1 = ctx.getBean("car1", Car.class);
        Car c2 = ctx.getBean("car2", Car.class);
        System.out.println("[DIAG] final engineCtor=" + engineCtor);
        System.out.println("[DIAG] c1.engine==e: " + (c1.engine == e));
        System.out.println("[DIAG] c2.engine==e: " + (c2.engine == e));
        ctx.close();
        System.exit(0);
    }
}
