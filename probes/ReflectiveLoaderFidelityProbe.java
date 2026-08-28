
import org.springframework.beans.factory.ObjectProvider;
import org.springframework.beans.factory.config.DependencyDescriptor;
import org.springframework.core.MethodParameter;
import org.springframework.core.ResolvableType;
import java.lang.reflect.Method;

/** Interrogates the exact decision DefaultListableBeanFactory.resolveDependency
 *  makes for the parameter that fails, and separates "the reflective descriptor
 *  path is unfaithful" from "the application namespace itself was overwritten". */
public class ReflectiveLoaderFidelityProbe {

    static String id(Class<?> c) {
        return c == null ? "null"
            : c.getName() + "@" + Integer.toHexString(System.identityHashCode(c))
              + " loader=" + (c.getClassLoader() == null ? "boot"
                  : c.getClassLoader().getClass().getSimpleName() + "@"
                    + Integer.toHexString(System.identityHashCode(c.getClassLoader())));
    }


    public static void main(String[] a) throws Exception {
        Class<?> cfg = Class.forName(
            "org.springframework.boot.actuate.autoconfigure.endpoint.web."
            + "WebEndpointAutoConfiguration$WebEndpointServletConfiguration");
        Method target = null;
        for (Method m : cfg.getDeclaredMethods()) {
            if (m.getName().equals("servletEndpointDiscoverer")) { target = m; break; }
        }
        if (target == null) { System.out.println("  method NOT FOUND"); return; }

        System.out.println("  ObjectProvider    = " + id(ObjectProvider.class));
        System.out.println("  declaringClass    = " + id(target.getDeclaringClass()));
        Class<?> pt = target.getParameterTypes()[1];
        System.out.println("  paramTypes[1]     = " + id(pt));
        System.out.println("  == ObjectProvider : " + (pt == ObjectProvider.class));

        // Is the APPLICATION namespace itself poisoned, or only this path?
        ClassLoader app = ReflectiveLoaderFidelityProbe.class.getClassLoader();
        Class<?> viaApp = Class.forName(ObjectProvider.class.getName(), false, app);
        System.out.println("  forName(app)      = " + id(viaApp));
        System.out.println("  forName==literal  : " + (viaApp == ObjectProvider.class));
        ClassLoader dl = target.getDeclaringClass().getClassLoader();
        Class<?> viaDecl = Class.forName(ObjectProvider.class.getName(), false, dl);
        System.out.println("  forName(declLdr)  = " + id(viaDecl));
        System.out.println("  declLdr==literal  : " + (viaDecl == ObjectProvider.class));

        // A FRESH mirror, in case the first Method object was cached from the
        // poisoned phase rather than built now.
        Method fresh = null;
        for (Method m2 : Class.forName(cfg.getName(), false, app).getDeclaredMethods()) {
            if (m2.getName().equals("servletEndpointDiscoverer")) { fresh = m2; break; }
        }
        System.out.println("  fresh paramTypes[1] = " + id(fresh.getParameterTypes()[1]));
        System.out.println("  fresh == literal  : " + (fresh.getParameterTypes()[1] == ObjectProvider.class));

        MethodParameter mp = new MethodParameter(target, 1);
        ResolvableType rt = ResolvableType.forMethodParameter(mp);
        System.out.println("  ResolvableType resolve() = " + id(rt.resolve()));
        DependencyDescriptor d = new DependencyDescriptor(mp, true);
        System.out.println("  >>> BRANCH TAKEN (== ObjectProvider.class) : "
            + (d.getDependencyType() == ObjectProvider.class));
    }
}
