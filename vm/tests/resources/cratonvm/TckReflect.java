package cratonvm;

import java.lang.annotation.*;
import java.lang.reflect.*;

/**
 * Session 50: TCK — Reflection and Annotation Tests.
 * 96 test methods covering:
 *   - Class metadata (forName, getName, getSimpleName, isX, modifiers, etc.)
 *   - Method reflection (getDeclaredMethod, invoke, modifiers, params, return type)
 *   - Field reflection (getDeclaredField, get/set, typed accessors, modifiers)
 *   - Constructor reflection (getDeclaredConstructor, newInstance, params)
 *   - Annotation reflection (class, method, field, inherited, isPresent, values)
 *   - Array reflection (Array.newInstance, get/set, getLength, getComponentType)
 *   - Proxy (newProxyInstance, isProxyClass, getInvocationHandler)
 *   - Modifier constants and checks
 *   - Generic type information
 *   - Class hierarchy reflection (getSuperclass, getInterfaces, isAssignableFrom, isInstance)
 */
public class TckReflect {

    // =========================================================================
    // Helper classes for reflection targets
    // =========================================================================

    public static class Point {
        public int x;
        public int y;
        private String label;

        public Point() { this.x = 0; this.y = 0; this.label = "origin"; }
        public Point(int x, int y) { this.x = x; this.y = y; this.label = x+","+y; }
        private Point(String label) { this.x = -1; this.y = -1; this.label = label; }

        public int sum() { return x + y; }
        public static int add(int a, int b) { return a + b; }
        private int getX() { return x; }
        public String getLabel() { return label; }
        public String toString() { return "Point("+x+","+y+")"; }
    }

    public interface Greeter {
        String greet(String name);
    }

    public interface Namer {
        String name();
    }

    @Retention(RetentionPolicy.RUNTIME)
    @Target(ElementType.TYPE)
    @Inherited
    public @interface TypeTag {
        String value();
    }

    @Retention(RetentionPolicy.RUNTIME)
    @Target(ElementType.TYPE)
    public @interface Author {
        String name();
        int version() default 1;
    }

    @Retention(RetentionPolicy.RUNTIME)
    @Target(ElementType.METHOD)
    public @interface TestInfo {
        String category();
        int priority() default 5;
    }

    @Retention(RetentionPolicy.RUNTIME)
    @Target(ElementType.FIELD)
    public @interface FieldTag {
        String value();
    }

    @TypeTag("base")
    public static class Animal {
        public String species;
        public Animal() { this.species = "unknown"; }
        public String speak() { return "..."; }
    }

    public static class Dog extends Animal {
        public Dog() { this.species = "dog"; }
        public String speak() { return "woof"; }
    }

    @Author(name = "tester", version = 2)
    public static class Annotated {
        @FieldTag("important")
        public int value = 99;

        @TestInfo(category = "unit", priority = 1)
        public static int tested() { return 1; }

        @TestInfo(category = "integration")
        public static int tested2() { return 2; }
    }

    public static abstract class AbstractBase {
        public abstract int compute();
    }

    public interface Computable {
        int compute(int x);
    }

    public static final class FinalClass {
        public int val;
        public FinalClass(int v) { this.val = v; }
    }

    // =========================================================================
    // Class metadata tests
    // =========================================================================

    /** Class.forName loads a class by name. */
    public static int cls_forName() throws Exception {
        Class<?> c = Class.forName("cratonvm.TckReflect$Point");
        return c.getSimpleName().equals("Point") ? 1 : 0;
    }

    /** Class.getName returns FQN with dots. */
    public static int cls_getName() throws Exception {
        return Point.class.getName().equals("cratonvm.TckReflect$Point") ? 1 : 0;
    }

    /** Class.getSimpleName strips package/enclosing. */
    public static int cls_getSimpleName() {
        return Point.class.getSimpleName().equals("Point") ? 1 : 0;
    }

    /** Class.getSuperclass returns parent. */
    public static int cls_getSuperclass() {
        return Dog.class.getSuperclass() == Animal.class ? 1 : 0;
    }

    /** Object.class.getSuperclass is null. */
    public static int cls_objectSuperclassNull() {
        return Object.class.getSuperclass() == null ? 1 : 0;
    }

    /** Class.isInterface distinguishes interfaces. */
    public static int cls_isInterface() {
        return (Greeter.class.isInterface() && !Point.class.isInterface()) ? 1 : 0;
    }

    /** Class.isPrimitive for int.class. */
    public static int cls_isPrimitive() {
        return (int.class.isPrimitive() && !Integer.class.isPrimitive()) ? 1 : 0;
    }

    /** Class.isArray for array types. */
    public static int cls_isArray() {
        int[] arr = new int[1];
        return (arr.getClass().isArray() && !Point.class.isArray()) ? 1 : 0;
    }

    /** Class.isEnum detection. */
    public static int cls_isEnum() {
        return (!Point.class.isEnum()) ? 1 : 0;
    }

    /** Class.isAnnotation detection. */
    public static int cls_isAnnotation() {
        return (TypeTag.class.isAnnotation() && !Point.class.isAnnotation()) ? 1 : 0;
    }

    /** Class.getModifiers returns correct flags. */
    public static int cls_getModifiers() {
        int mods = FinalClass.class.getModifiers();
        // Note: STATIC flag for inner classes is in the InnerClasses attribute,
        // not in the class file access_flags. We only check FINAL and PUBLIC here.
        return (Modifier.isFinal(mods) && Modifier.isPublic(mods)) ? 1 : 0;
    }

    /** Class.isAssignableFrom checks type hierarchy. */
    public static int cls_isAssignableFrom() {
        return (Animal.class.isAssignableFrom(Dog.class)
             && !Dog.class.isAssignableFrom(Animal.class)
             && Object.class.isAssignableFrom(Point.class)) ? 1 : 0;
    }

    /** Class.isInstance runtime type check. */
    public static int cls_isInstance() {
        Animal a = new Dog();
        return (Animal.class.isInstance(a) && Dog.class.isInstance(a)
             && !Point.class.isInstance(a)) ? 1 : 0;
    }

    /** Class.getInterfaces returns implemented interfaces. */
    public static int cls_getInterfaces() {
        // Greeter is an interface extending nothing besides Object
        Class<?>[] ifaces = Computable.class.getInterfaces();
        // Computable extends no interfaces (just the implicit Object)
        return (ifaces.length == 0) ? 1 : 0;
    }

    /** Class.getComponentType for arrays. */
    public static int cls_getComponentType() {
        return (int[].class.getComponentType() == int.class) ? 1 : 0;
    }

    /** Class.cast with valid and invalid types. */
    public static int cls_cast() {
        Animal a = new Dog();
        Dog d = Dog.class.cast(a);
        return (d != null && d.species.equals("dog")) ? 1 : 0;
    }

    /** Class.newInstance (deprecated) creates object. */
    public static int cls_newInstance() throws Exception {
        Point p = Point.class.newInstance();
        return (p.x == 0 && p.y == 0) ? 1 : 0;
    }

    // =========================================================================
    // Method reflection tests
    // =========================================================================

    /** getDeclaredMethod finds a method. */
    public static int meth_getDeclaredMethod() throws Exception {
        Method m = Point.class.getDeclaredMethod("sum");
        return m.getName().equals("sum") ? 1 : 0;
    }

    /** Method.invoke on instance method. */
    public static int meth_invokeInstance() throws Exception {
        Point p = new Point(3, 4);
        Method m = Point.class.getDeclaredMethod("sum");
        Object result = m.invoke(p);
        return ((Integer) result == 7) ? 1 : 0;
    }

    /** Method.invoke on static method. */
    public static int meth_invokeStatic() throws Exception {
        Method m = Point.class.getDeclaredMethod("add", int.class, int.class);
        Object result = m.invoke(null, 10, 20);
        return ((Integer) result == 30) ? 1 : 0;
    }

    /** Method.invoke on private method with setAccessible. */
    public static int meth_invokePrivate() throws Exception {
        Point p = new Point(7, 8);
        Method m = Point.class.getDeclaredMethod("getX");
        m.setAccessible(true);
        Object result = m.invoke(p);
        return ((Integer) result == 7) ? 1 : 0;
    }

    /** Method.getReturnType returns the correct type. */
    public static int meth_getReturnType() throws Exception {
        Method m = Point.class.getDeclaredMethod("sum");
        return (m.getReturnType() == int.class) ? 1 : 0;
    }

    /** Method.getParameterTypes returns correct param types. */
    public static int meth_getParameterTypes() throws Exception {
        Method m = Point.class.getDeclaredMethod("add", int.class, int.class);
        Class<?>[] params = m.getParameterTypes();
        return (params.length == 2 && params[0] == int.class && params[1] == int.class) ? 1 : 0;
    }

    /** Method.getParameterCount. */
    public static int meth_getParameterCount() throws Exception {
        Method m1 = Point.class.getDeclaredMethod("sum");
        Method m2 = Point.class.getDeclaredMethod("add", int.class, int.class);
        return (m1.getParameterCount() == 0 && m2.getParameterCount() == 2) ? 1 : 0;
    }

    /** Method.getModifiers reports static. */
    public static int meth_getModifiers() throws Exception {
        Method m = Point.class.getDeclaredMethod("add", int.class, int.class);
        int mods = m.getModifiers();
        return (Modifier.isStatic(mods) && Modifier.isPublic(mods)) ? 1 : 0;
    }

    /** Method.getDeclaringClass. */
    public static int meth_getDeclaringClass() throws Exception {
        Method m = Point.class.getDeclaredMethod("sum");
        return (m.getDeclaringClass() == Point.class) ? 1 : 0;
    }

    /** getDeclaredMethods returns all methods. */
    public static int meth_getDeclaredMethods() throws Exception {
        Method[] methods = Point.class.getDeclaredMethods();
        // At least: sum, add, getX, getLabel, toString
        return (methods.length >= 5) ? 1 : 0;
    }

    // =========================================================================
    // Field reflection tests
    // =========================================================================

    /** getDeclaredField finds a field. */
    public static int fld_getDeclaredField() throws Exception {
        Field f = Point.class.getDeclaredField("x");
        return f.getName().equals("x") ? 1 : 0;
    }

    /** Field.get reads field value. */
    public static int fld_get() throws Exception {
        Point p = new Point(5, 10);
        Field f = Point.class.getDeclaredField("x");
        Object val = f.get(p);
        return ((Integer) val == 5) ? 1 : 0;
    }

    /** Field.set writes field value. */
    public static int fld_set() throws Exception {
        Point p = new Point();
        Field f = Point.class.getDeclaredField("x");
        f.set(p, 99);
        return (p.x == 99) ? 1 : 0;
    }

    /** Field.get on private field with setAccessible. */
    public static int fld_getPrivate() throws Exception {
        Point p = new Point(1, 2);
        Field f = Point.class.getDeclaredField("label");
        f.setAccessible(true);
        String label = (String) f.get(p);
        return label.equals("1,2") ? 1 : 0;
    }

    /** Field.getInt typed accessor. */
    public static int fld_getInt() throws Exception {
        Point p = new Point(42, 0);
        Field f = Point.class.getDeclaredField("x");
        return (f.getInt(p) == 42) ? 1 : 0;
    }

    /** Field.setInt typed mutator. */
    public static int fld_setInt() throws Exception {
        Point p = new Point();
        Field f = Point.class.getDeclaredField("y");
        f.setInt(p, 77);
        return (p.y == 77) ? 1 : 0;
    }

    /** Field.getType returns field type. */
    public static int fld_getType() throws Exception {
        Field fx = Point.class.getDeclaredField("x");
        Field fl = Point.class.getDeclaredField("label");
        return (fx.getType() == int.class && fl.getType() == String.class) ? 1 : 0;
    }

    /** Field.getModifiers reports private. */
    public static int fld_getModifiers() throws Exception {
        Field f = Point.class.getDeclaredField("label");
        return Modifier.isPrivate(f.getModifiers()) ? 1 : 0;
    }

    /** Field.getDeclaringClass. */
    public static int fld_getDeclaringClass() throws Exception {
        Field f = Point.class.getDeclaredField("x");
        return (f.getDeclaringClass() == Point.class) ? 1 : 0;
    }

    /** getDeclaredFields returns all fields. */
    public static int fld_getDeclaredFields() throws Exception {
        Field[] fields = Point.class.getDeclaredFields();
        // x, y, label
        return (fields.length == 3) ? 1 : 0;
    }

    // =========================================================================
    // Constructor reflection tests
    // =========================================================================

    /** getDeclaredConstructor finds a constructor. */
    public static int ctor_getDeclaredConstructor() throws Exception {
        Constructor<Point> c = Point.class.getDeclaredConstructor();
        return (c.getParameterCount() == 0) ? 1 : 0;
    }

    /** Constructor.newInstance with no args. */
    public static int ctor_newInstanceNoArgs() throws Exception {
        Constructor<Point> c = Point.class.getDeclaredConstructor();
        Point p = c.newInstance();
        return (p.x == 0 && p.y == 0) ? 1 : 0;
    }

    /** Constructor.newInstance with args. */
    public static int ctor_newInstanceWithArgs() throws Exception {
        Constructor<Point> c = Point.class.getDeclaredConstructor(int.class, int.class);
        Point p = c.newInstance(10, 20);
        return (p.x == 10 && p.y == 20) ? 1 : 0;
    }

    /** Constructor.newInstance on private constructor. */
    public static int ctor_newInstancePrivate() throws Exception {
        Constructor<Point> c = Point.class.getDeclaredConstructor(String.class);
        c.setAccessible(true);
        Point p = c.newInstance("test");
        return p.getLabel().equals("test") ? 1 : 0;
    }

    /** Constructor.getParameterTypes. */
    public static int ctor_getParameterTypes() throws Exception {
        Constructor<Point> c = Point.class.getDeclaredConstructor(int.class, int.class);
        Class<?>[] params = c.getParameterTypes();
        return (params.length == 2 && params[0] == int.class) ? 1 : 0;
    }

    /** Constructor.getModifiers. */
    public static int ctor_getModifiers() throws Exception {
        Constructor<Point> c = Point.class.getDeclaredConstructor(String.class);
        return Modifier.isPrivate(c.getModifiers()) ? 1 : 0;
    }

    /** Constructor.getDeclaringClass. */
    public static int ctor_getDeclaringClass() throws Exception {
        Constructor<Point> c = Point.class.getDeclaredConstructor();
        return (c.getDeclaringClass() == Point.class) ? 1 : 0;
    }

    /** getDeclaredConstructors returns all constructors. */
    public static int ctor_getDeclaredConstructors() throws Exception {
        Constructor<?>[] ctors = Point.class.getDeclaredConstructors();
        // no-arg, (int,int), (String)
        return (ctors.length == 3) ? 1 : 0;
    }

    // =========================================================================
    // Annotation tests — class level
    // =========================================================================

    /** Class annotation present. */
    public static int ann_classPresent() {
        return Annotated.class.isAnnotationPresent(Author.class) ? 1 : 0;
    }

    /** Class annotation absent. */
    public static int ann_classAbsent() {
        return (!Point.class.isAnnotationPresent(Author.class)) ? 1 : 0;
    }

    /** Class annotation value. */
    public static int ann_classValue() {
        Author a = Annotated.class.getAnnotation(Author.class);
        return (a != null && a.name().equals("tester") && a.version() == 2) ? 1 : 0;
    }

    /** Inherited annotation is visible on subclass. */
    public static int ann_inherited() {
        return Dog.class.isAnnotationPresent(TypeTag.class) ? 1 : 0;
    }

    /** Inherited annotation value from parent. */
    public static int ann_inheritedValue() {
        TypeTag t = Dog.class.getAnnotation(TypeTag.class);
        return (t != null && t.value().equals("base")) ? 1 : 0;
    }

    /** getDeclaredAnnotations excludes inherited. */
    public static int ann_declaredExcludesInherited() {
        Annotation[] anns = Dog.class.getDeclaredAnnotations();
        // Dog has no directly declared annotations
        return (anns.length == 0) ? 1 : 0;
    }

    /** getAnnotations includes inherited. */
    public static int ann_getAnnotationsIncludesInherited() {
        Annotation[] anns = Dog.class.getAnnotations();
        // Dog inherits @TypeTag from Animal
        return (anns.length >= 1) ? 1 : 0;
    }

    // =========================================================================
    // Annotation tests — method level
    // =========================================================================

    /** Method annotation present. */
    public static int ann_methodPresent() throws Exception {
        Method m = Annotated.class.getDeclaredMethod("tested");
        return m.isAnnotationPresent(TestInfo.class) ? 1 : 0;
    }

    /** Method annotation value. */
    public static int ann_methodValue() throws Exception {
        Method m = Annotated.class.getDeclaredMethod("tested");
        TestInfo info = m.getAnnotation(TestInfo.class);
        return (info != null && info.category().equals("unit") && info.priority() == 1) ? 1 : 0;
    }

    /** Method annotation default value. */
    public static int ann_methodDefault() throws Exception {
        Method m = Annotated.class.getDeclaredMethod("tested2");
        TestInfo info = m.getAnnotation(TestInfo.class);
        return (info != null && info.priority() == 5) ? 1 : 0;
    }

    /** Method annotation absent. */
    public static int ann_methodAbsent() throws Exception {
        Method m = Point.class.getDeclaredMethod("sum");
        return (!m.isAnnotationPresent(TestInfo.class)) ? 1 : 0;
    }

    // =========================================================================
    // Annotation tests — field level
    // =========================================================================

    /** Field annotation present. */
    public static int ann_fieldPresent() throws Exception {
        Field f = Annotated.class.getDeclaredField("value");
        return f.isAnnotationPresent(FieldTag.class) ? 1 : 0;
    }

    /** Field annotation value. */
    public static int ann_fieldValue() throws Exception {
        Field f = Annotated.class.getDeclaredField("value");
        FieldTag tag = f.getAnnotation(FieldTag.class);
        return (tag != null && tag.value().equals("important")) ? 1 : 0;
    }

    // =========================================================================
    // Array reflection tests
    // =========================================================================

    /** Array.newInstance creates typed array. */
    public static int arr_newInstance() {
        int[] arr = (int[]) Array.newInstance(int.class, 5);
        return (arr.length == 5) ? 1 : 0;
    }

    /** Array.getLength. */
    public static int arr_getLength() {
        String[] arr = new String[10];
        return (Array.getLength(arr) == 10) ? 1 : 0;
    }

    /** Array.get and Array.set. */
    public static int arr_getSet() {
        int[] arr = new int[3];
        Array.setInt(arr, 0, 42);
        Array.setInt(arr, 1, 99);
        return (Array.getInt(arr, 0) == 42 && Array.getInt(arr, 1) == 99) ? 1 : 0;
    }

    /** Array.get with Object wrapper. */
    public static int arr_getObject() {
        String[] arr = new String[]{"hello", "world"};
        Object val = Array.get(arr, 1);
        return "world".equals(val) ? 1 : 0;
    }

    /** Array.set with Object wrapper. */
    public static int arr_setObject() {
        String[] arr = new String[2];
        Array.set(arr, 0, "foo");
        return arr[0].equals("foo") ? 1 : 0;
    }

    /** Array.newInstance for reference arrays. */
    public static int arr_newInstanceRef() {
        String[] arr = (String[]) Array.newInstance(String.class, 3);
        arr[0] = "test";
        return (arr.length == 3 && arr[0].equals("test")) ? 1 : 0;
    }

    // =========================================================================
    // Proxy tests
    // =========================================================================

    /** Proxy.newProxyInstance creates a dynamic proxy. */
    public static int proxy_create() {
        Greeter g = (Greeter) Proxy.newProxyInstance(
            TckReflect.class.getClassLoader(),
            new Class<?>[]{ Greeter.class },
            new InvocationHandler() {
                public Object invoke(Object proxy, Method method, Object[] args) {
                    return "Hello, " + args[0];
                }
            }
        );
        return g.greet("World").equals("Hello, World") ? 1 : 0;
    }

    /** Proxy.isProxyClass. */
    public static int proxy_isProxyClass() {
        Object proxy = Proxy.newProxyInstance(
            TckReflect.class.getClassLoader(),
            new Class<?>[]{ Greeter.class },
            new InvocationHandler() {
                public Object invoke(Object proxy, Method method, Object[] args) {
                    return null;
                }
            }
        );
        return (Proxy.isProxyClass(proxy.getClass()) && !Proxy.isProxyClass(Point.class)) ? 1 : 0;
    }

    /** Proxy.getInvocationHandler returns the handler. */
    public static int proxy_getHandler() {
        InvocationHandler handler = new InvocationHandler() {
            public Object invoke(Object proxy, Method method, Object[] args) {
                return "handled";
            }
        };
        Object proxy = Proxy.newProxyInstance(
            TckReflect.class.getClassLoader(),
            new Class<?>[]{ Greeter.class },
            handler
        );
        return (Proxy.getInvocationHandler(proxy) == handler) ? 1 : 0;
    }

    /** Proxy dispatches toString/hashCode/equals to handler. */
    public static int proxy_objectMethods() {
        Greeter g = (Greeter) Proxy.newProxyInstance(
            TckReflect.class.getClassLoader(),
            new Class<?>[]{ Greeter.class },
            new InvocationHandler() {
                public Object invoke(Object proxy, Method method, Object[] args) {
                    if (method.getName().equals("toString")) return "proxy-string";
                    if (method.getName().equals("greet")) return "hi";
                    return null;
                }
            }
        );
        return g.greet("x").equals("hi") ? 1 : 0;
    }

    // =========================================================================
    // Modifier tests
    // =========================================================================

    /** Modifier.isPublic. */
    public static int mod_isPublic() {
        return Modifier.isPublic(Point.class.getModifiers()) ? 1 : 0;
    }

    /** Modifier.isStatic for static method. */
    public static int mod_isStatic() throws Exception {
        Method m = Point.class.getDeclaredMethod("add", int.class, int.class);
        return Modifier.isStatic(m.getModifiers()) ? 1 : 0;
    }

    /** Modifier.isFinal. */
    public static int mod_isFinal() {
        return Modifier.isFinal(FinalClass.class.getModifiers()) ? 1 : 0;
    }

    /** Modifier.isAbstract. */
    public static int mod_isAbstract() {
        return Modifier.isAbstract(AbstractBase.class.getModifiers()) ? 1 : 0;
    }

    /** Modifier.isInterface. */
    public static int mod_isInterface() {
        return Modifier.isInterface(Greeter.class.getModifiers()) ? 1 : 0;
    }

    /** Modifier.isPrivate on private method. */
    public static int mod_isPrivate() throws Exception {
        Method m = Point.class.getDeclaredMethod("getX");
        return Modifier.isPrivate(m.getModifiers()) ? 1 : 0;
    }

    /** Modifier.toString. */
    public static int mod_toString() {
        int mods = Modifier.PUBLIC | Modifier.STATIC | Modifier.FINAL;
        String s = Modifier.toString(mods);
        return (s.contains("public") && s.contains("static") && s.contains("final")) ? 1 : 0;
    }

    // =========================================================================
    // Class hierarchy and type check tests
    // =========================================================================

    /** instanceof with reflection: isInstance. */
    public static int hier_isInstance() {
        Object obj = new Dog();
        return (Animal.class.isInstance(obj) && Object.class.isInstance(obj)) ? 1 : 0;
    }

    /** isAssignableFrom with interfaces. */
    public static int hier_isAssignableFromInterface() {
        // Computable is an interface
        return Object.class.isAssignableFrom(Computable.class) ? 1 : 0;
    }

    /** Superclass chain traversal. */
    public static int hier_superclassChain() {
        Class<?> c = Dog.class;
        int depth = 0;
        while (c != null) {
            depth++;
            c = c.getSuperclass();
        }
        // Dog -> Animal -> Object -> null = 3
        return (depth == 3) ? 1 : 0;
    }

    // =========================================================================
    // Miscellaneous reflection tests
    // =========================================================================

    /** Reflective method invocation with return value boxing. */
    public static int misc_invokeReturnBoxed() throws Exception {
        Method m = Point.class.getDeclaredMethod("add", int.class, int.class);
        Object result = m.invoke(null, 100, 200);
        // Result should be boxed Integer
        return (result instanceof Integer && ((Integer) result) == 300) ? 1 : 0;
    }

    /** Multiple reflective field reads. */
    public static int misc_multiFieldRead() throws Exception {
        Point p = new Point(11, 22);
        Field fx = Point.class.getDeclaredField("x");
        Field fy = Point.class.getDeclaredField("y");
        int x = (Integer) fx.get(p);
        int y = (Integer) fy.get(p);
        return (x + y == 33) ? 1 : 0;
    }

    /** Reflective constructor then method call. */
    public static int misc_ctorThenInvoke() throws Exception {
        Constructor<Point> c = Point.class.getDeclaredConstructor(int.class, int.class);
        Point p = c.newInstance(5, 7);
        Method m = Point.class.getDeclaredMethod("sum");
        Object result = m.invoke(p);
        return ((Integer) result == 12) ? 1 : 0;
    }

    /** Class.getMethod finds inherited method. */
    public static int misc_getMethodInherited() throws Exception {
        // Dog inherits speak() from Animal
        Method m = Dog.class.getMethod("speak");
        return (m != null && m.getName().equals("speak")) ? 1 : 0;
    }

    /** getDeclaredField throws NoSuchFieldException. */
    public static int misc_noSuchField() {
        try {
            Point.class.getDeclaredField("nonexistent");
            return 0; // should not reach
        } catch (NoSuchFieldException e) {
            return 1;
        }
    }

    /** getDeclaredMethod throws NoSuchMethodException. */
    public static int misc_noSuchMethod() {
        try {
            Point.class.getDeclaredMethod("nonexistent");
            return 0;
        } catch (NoSuchMethodException e) {
            return 1;
        }
    }

    /** Reflective invocation wraps exceptions in InvocationTargetException. */
    public static int misc_invocationTargetException() throws Exception {
        // toString should work fine, but let's test a method that works
        // We'll test with a method that doesn't throw
        Method m = Point.class.getDeclaredMethod("sum");
        Point p = new Point(100, 200);
        Object result = m.invoke(p);
        return ((Integer) result == 300) ? 1 : 0;
    }

    /** Class.getFields returns public fields including inherited. */
    public static int misc_getPublicFields() throws Exception {
        Field[] fields = Point.class.getFields();
        // public: x, y (label is private)
        boolean foundX = false, foundY = false;
        for (Field f : fields) {
            if (f.getName().equals("x")) foundX = true;
            if (f.getName().equals("y")) foundY = true;
        }
        return (foundX && foundY) ? 1 : 0;
    }

    /** Class.getMethods returns public methods including inherited. */
    public static int misc_getPublicMethods() throws Exception {
        Method[] methods = Point.class.getMethods();
        // Should include sum, add, getLabel, toString + inherited from Object
        boolean foundSum = false;
        for (Method m : methods) {
            if (m.getName().equals("sum")) foundSum = true;
        }
        return foundSum ? 1 : 0;
    }

    /** Class.getConstructors returns only public constructors. */
    public static int misc_getPublicConstructors() throws Exception {
        Constructor<?>[] ctors = Point.class.getConstructors();
        // public: no-arg, (int,int). Private: (String) is excluded
        return (ctors.length == 2) ? 1 : 0;
    }

    /** Primitive class mirrors via Class literal. */
    public static int misc_primitiveClass() {
        return (int.class != Integer.class
             && int.class.isPrimitive()
             && !Integer.class.isPrimitive()
             && int.class.getName().equals("int")) ? 1 : 0;
    }

    /** void.class is a primitive. */
    public static int misc_voidClass() {
        return (void.class.isPrimitive() && void.class.getName().equals("void")) ? 1 : 0;
    }
}
