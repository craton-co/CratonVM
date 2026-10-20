import java.io.InputStream;
import java.lang.reflect.Field;
import java.lang.reflect.Method;

// lane classshape residual (CLASSSHAPE19-2): a descriptor-backed stand-in mirror for an
// array whose component a child loader defined must answer that loader's component,
// and minting it must not make the APPLICATION loader load its own copy of the
// component (HotSpot never does). Run with --add-opens java.base/java.lang=ALL-UNNAMED.
// Expected (HotSpot 25): every line ends `true`, and `app-loaded-before`/`app-loaded-after`
// are both `false`.
public class R19StandinComponent {
    static final class OwnLoader extends ClassLoader {
        OwnLoader(ClassLoader parent) { super(parent); }
        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (!name.startsWith("R19StandinPayload")) return super.loadClass(name, resolve);
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c == null) {
                    try (InputStream in = R19StandinComponent.class.getClassLoader().getResourceAsStream(name + ".class")) {
                        byte[] b = in.readAllBytes();
                        c = defineClass(name, b, 0, b.length);
                    } catch (Exception e) {
                        throw new ClassNotFoundException(name, e);
                    }
                }
                return c;
            }
        }
    }

    public static void main(String[] a) throws Throwable {
        Method flc = ClassLoader.class.getDeclaredMethod("findLoadedClass", String.class);
        flc.setAccessible(true);
        ClassLoader app = R19StandinComponent.class.getClassLoader();
        String n = "R19StandinPayload";
        Class<?> own = new OwnLoader(app).loadClass(n);
        System.out.println("own-distinct-loader " + (own.getClassLoader() != app));
        System.out.println("app-loaded-before " + (flc.invoke(app, n) != null));
        Class<?> r1 = own.getMethod("one").getReturnType();
        Class<?> r2 = own.getMethod("two").getReturnType();
        Class<?> r3 = own.getMethod("three").getReturnType();
        Field f = own.getDeclaredField("items");
        Class<?> pt = own.getMethod("take", own.arrayType()).getParameterTypes()[0];
        System.out.println("one-isArray " + r1.isArray() + " " + (r1.getComponentType() == own) + " " + (r1.componentType() == own) + " " + (r1 == own.arrayType()));
        System.out.println("two-isArray " + r2.isArray() + " " + (r2.getComponentType() == own.arrayType()) + " " + (r2.getComponentType().getComponentType() == own));
        System.out.println("three-isArray " + r3.isArray() + " " + (r3.getComponentType().getComponentType().getComponentType() == own));
        System.out.println("field-isArray " + f.getType().isArray() + " " + (f.getType().getComponentType() == own));
        System.out.println("param-isArray " + pt.isArray() + " " + (pt.getComponentType() == own));
        System.out.println("names " + r1.getName().equals("[L" + n + ";") + " " + r2.getSimpleName().equals(n + "[][]") + " " + r1.descriptorString().equals("[L" + n + ";"));
        System.out.println("shape " + (!r1.isInterface()) + " " + (!r1.isPrimitive()) + " " + ((r1.getModifiers() & 0x410) == 0x410));
        System.out.println("app-loaded-after " + (flc.invoke(app, n) != null));
        Class<?> appCopy = Class.forName(n, false, app);
        System.out.println("app-copy-distinct " + (appCopy != own) + " " + (appCopy.getMethod("one").getReturnType().getComponentType() == appCopy));
        // The application copy's array types are the APPLICATION loader's `X[]`, never the
        // child loader's (the only `[LR19StandinPayload;` until now).
        System.out.println("app-copy-array " + (appCopy.arrayType().getComponentType() == appCopy)
            + " " + (appCopy.getMethod("one").getReturnType() == appCopy.arrayType())
            + " " + (appCopy.getDeclaredField("items").getType() == appCopy.arrayType())
            + " " + (appCopy.arrayType() != own.arrayType())
            + " " + (java.lang.reflect.Array.newInstance(appCopy, 1).getClass() == appCopy.arrayType())
            + " " + (appCopy.getMethod("two").getReturnType().getComponentType() == appCopy.arrayType()));
    }
}

class R19StandinPayload {
    R19StandinPayload[] items;
    public R19StandinPayload[] one() { return null; }
    public R19StandinPayload[][] two() { return null; }
    public R19StandinPayload[][][] three() { return null; }
    public void take(R19StandinPayload[] p) { }
}
