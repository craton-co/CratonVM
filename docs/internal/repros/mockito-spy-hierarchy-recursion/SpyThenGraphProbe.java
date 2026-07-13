import static org.mockito.Mockito.*;
import net.bytebuddy.description.method.MethodDescription;
import net.bytebuddy.description.method.MethodDescription.SignatureToken;
import net.bytebuddy.description.type.TypeDescription;
import net.bytebuddy.dynamic.scaffold.MethodGraph;
import org.springframework.beans.factory.support.DefaultListableBeanFactory;
import org.springframework.beans.factory.support.DefaultSingletonBeanRegistry;

import java.lang.reflect.Method;

public class SpyThenGraphProbe {
    public static void main(String[] args) throws Exception {
        System.out.println("STEP0 creating spy (triggers retransformClasses on the hierarchy)...");
        DefaultListableBeanFactory real = new DefaultListableBeanFactory();
        DefaultListableBeanFactory spy = spy(real);
        System.out.println("STEP0b spy.getClass()=" + spy.getClass().getName());

        Method m = DefaultSingletonBeanRegistry.class.getMethod("registerSingleton", String.class, Object.class);
        SignatureToken token = new MethodDescription.ForLoadedMethod(m).asSignatureToken();
        System.out.println("STEP1 computing MethodGraph for spy.getClass() (POST-RETRANSFORM) via ByteBuddy's own compiler...");
        TypeDescription td = TypeDescription.ForLoadedType.of(spy.getClass());
        MethodGraph.Linked graph = MethodGraph.Compiler.Default.forJavaHierarchy().compile(td);
        MethodGraph.Node node = graph.locate(token);
        System.out.println("STEP2 node sort=" + node.getSort() + " isResolved=" + node.getSort().isResolved());
        if (node.getSort().isResolved()) {
            MethodDescription rep = node.getRepresentative();
            TypeDescription declaringType = rep.asDefined().getDeclaringType().asErasure();
            System.out.println("STEP3 representative method=" + rep + " declaringType=" + declaringType.getName());
            boolean notOverridden = declaringType.represents(m.getDeclaringClass());
            boolean isOverridden = !notOverridden;
            System.out.println("STEP4 isOverridden(DefaultSingletonBeanRegistry.registerSingleton on POST-RETRANSFORM DLBF instance) = " + isOverridden + " (expected true)");
        } else {
            System.out.println("STEP3 NODE NOT RESOLVED");
        }

        // Also check the reverse: is DefaultListableBeanFactory.registerSingleton
        // itself considered "overridden" (it shouldn't be -- it's the most-derived).
        Method m2 = DefaultListableBeanFactory.class.getMethod("registerSingleton", String.class, Object.class);
        SignatureToken token2 = new MethodDescription.ForLoadedMethod(m2).asSignatureToken();
        MethodGraph.Node node2 = graph.locate(token2);
        if (node2.getSort().isResolved()) {
            TypeDescription declaringType2 = node2.getRepresentative().asDefined().getDeclaringType().asErasure();
            boolean notOverridden2 = declaringType2.represents(m2.getDeclaringClass());
            System.out.println("STEP5 isOverridden(DefaultListableBeanFactory.registerSingleton on POST-RETRANSFORM DLBF instance) = " + (!notOverridden2) + " (expected false)");
        }
        System.out.println("ALL DONE");
    }
}
