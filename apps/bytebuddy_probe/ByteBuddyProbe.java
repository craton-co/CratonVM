import net.bytebuddy.ByteBuddy;
import net.bytebuddy.dynamic.DynamicType;
import net.bytebuddy.dynamic.loading.ClassLoadingStrategy;
import net.bytebuddy.implementation.FixedValue;
import static net.bytebuddy.matcher.ElementMatchers.named;

public class ByteBuddyProbe {
    public static void main(String[] args) throws Exception {
        DynamicType.Unloaded<Object> unloaded = new ByteBuddy()
                .subclass(Object.class)
                .name("GeneratedHello")
                .method(named("toString"))
                .intercept(FixedValue.value("hello-bytebuddy"))
                .make();

        Class<?> dyn = unloaded.load(ByteBuddyProbe.class.getClassLoader(),
                ClassLoadingStrategy.Default.WRAPPER).getLoaded();

        Object inst = dyn.getDeclaredConstructor().newInstance();
        System.out.println("dyn.toString=" + inst);
        System.out.println("ByteBuddyProbe: PASS");
    }
}
