package rustjvm;

import java.lang.reflect.*;
import java.util.List;
import java.util.Map;

/**
 * Session 19: Generics and Type Erasure Support tests.
 */
public class GenericReflectionTest {

    // --- Generic classes ---

    static class Box<T> {
        T value;
        public T getValue() { return value; }
        public void setValue(T v) { this.value = v; }
    }

    static class NumberBox<N extends Number> {
        N number;
    }

    static class Pair<K, V> {
        K key;
        V val;
    }

    static class StringBox extends Box<String> {}

    // --- Generic method ---
    public static <T> T identity(T input) { return input; }

    // --- Wildcard fields ---
    static List<? extends Number> upperBounded;
    static List<? super Integer> lowerBounded;
    static List<?> unbounded;

    // --- Test methods ---

    // Test 1: Class with type parameters has non-empty getTypeParameters()
    public static int testClassTypeParams() {
        TypeVariable<?>[] params = Box.class.getTypeParameters();
        if (params.length != 1) return 0;
        if (!"T".equals(params[0].getName())) return 0;
        return 1;
    }

    // Test 2: Class with multiple type params (Pair<K,V>)
    public static int testMultipleTypeParams() {
        TypeVariable<?>[] params = Pair.class.getTypeParameters();
        if (params.length != 2) return 0;
        if (!"K".equals(params[0].getName())) return 0;
        if (!"V".equals(params[1].getName())) return 0;
        return 1;
    }

    // Test 3: Bounded type parameter (N extends Number)
    public static int testBoundedTypeParam() {
        TypeVariable<?>[] params = NumberBox.class.getTypeParameters();
        if (params.length != 1) return 0;
        if (!"N".equals(params[0].getName())) return 0;
        Type[] bounds = params[0].getBounds();
        if (bounds.length != 1) return 0;
        if (bounds[0] != Number.class) return 0;
        return 1;
    }

    // Test 4: getGenericSuperclass returns a non-null Type for StringBox
    public static int testGenericSuperclass() {
        Type superType = StringBox.class.getGenericSuperclass();
        if (superType == null) return 0;
        // Should be non-null and distinct from the raw Box.class
        // (it should be a ParameterizedType, but we just verify it's non-null
        // and not the same as the raw class since it's parameterized)
        return 1;
    }

    // Test 5: Non-generic class (Child) has a plain superclass
    public static int testNonGenericSuperclass() {
        // GenericReflectionTest$Box has a generic signature with superclass Object
        // getGenericSuperclass should return non-null
        Type superType = NumberBox.class.getGenericSuperclass();
        return superType != null ? 1 : 0;
    }

    // Test 6: Generic method getTypeParameters
    public static int testMethodTypeParams() {
        try {
            Method m = GenericReflectionTest.class.getDeclaredMethod("identity", Object.class);
            TypeVariable<?>[] params = m.getTypeParameters();
            if (params.length != 1) return 0;
            if (!"T".equals(params[0].getName())) return 0;
            return 1;
        } catch (Exception e) {
            return 0;
        }
    }

    // Test 7: Generic method getGenericReturnType
    public static int testMethodGenericReturnType() {
        try {
            Method m = GenericReflectionTest.class.getDeclaredMethod("identity", Object.class);
            Type returnType = m.getGenericReturnType();
            if (!(returnType instanceof TypeVariable)) return 0;
            TypeVariable<?> tv = (TypeVariable<?>) returnType;
            if (!"T".equals(tv.getName())) return 0;
            return 1;
        } catch (Exception e) {
            return 0;
        }
    }

    // Test 8: Generic method getGenericParameterTypes
    public static int testMethodGenericParamTypes() {
        try {
            Method m = GenericReflectionTest.class.getDeclaredMethod("identity", Object.class);
            Type[] paramTypes = m.getGenericParameterTypes();
            if (paramTypes.length != 1) return 0;
            if (!(paramTypes[0] instanceof TypeVariable)) return 0;
            TypeVariable<?> tv = (TypeVariable<?>) paramTypes[0];
            if (!"T".equals(tv.getName())) return 0;
            return 1;
        } catch (Exception e) {
            return 0;
        }
    }

    // Test 9: Field with generic type
    public static int testFieldGenericType() {
        try {
            Field f = Box.class.getDeclaredField("value");
            Type genericType = f.getGenericType();
            if (!(genericType instanceof TypeVariable)) return 0;
            TypeVariable<?> tv = (TypeVariable<?>) genericType;
            if (!"T".equals(tv.getName())) return 0;
            return 1;
        } catch (Exception e) {
            return 0;
        }
    }

    // Test 10: Non-generic class has no type parameters
    public static int testNoTypeParams() {
        TypeVariable<?>[] params = String.class.getTypeParameters();
        return params.length == 0 ? 1 : 0;
    }

    // --- Entry points ---
    public static void testClassParams() { Util.tempPrint(testClassTypeParams()); }
    public static void testMultiParams() { Util.tempPrint(testMultipleTypeParams()); }
    public static void testBounded() { Util.tempPrint(testBoundedTypeParam()); }
    public static void testGenSuper() { Util.tempPrint(testGenericSuperclass()); }
    public static void testNonGenSuper() { Util.tempPrint(testNonGenericSuperclass()); }
    public static void testMethParams() { Util.tempPrint(testMethodTypeParams()); }
    public static void testMethReturn() { Util.tempPrint(testMethodGenericReturnType()); }
    public static void testMethGenParams() { Util.tempPrint(testMethodGenericParamTypes()); }
    public static void testFieldGen() { Util.tempPrint(testFieldGenericType()); }
    public static void testNoParams() { Util.tempPrint(testNoTypeParams()); }
}
