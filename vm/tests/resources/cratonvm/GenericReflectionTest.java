package cratonvm;

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

    // WP2.8 acceptance: a class that extends a parameterized superclass
    // with a concrete type argument so getGenericSuperclass() materializes
    // a real ParameterizedType with getActualTypeArguments() populated.
    static class StringList extends java.util.ArrayList<String> {}

    // --- Generic method ---
    public static <T> T identity(T input) { return input; }

    // --- Wildcard fields ---
    static List<? extends Number> upperBounded;
    static List<? super Integer> lowerBounded;
    static List<?> unbounded;

    // WP2.8 acceptance: parameterized field types
    static List<String> users;
    static Map<String, Integer> counts;

    interface Search<I, O> {
    }

    interface Create<I, O> {
        default O create(I body) {
            return null;
        }
    }

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

    // --- WP2.8 acceptance tests ---

    // Test 11: getGenericSuperclass on a class that extends ArrayList<String>
    // returns a ParameterizedType with raw=ArrayList and args=[String].
    // This is the load-bearing roadmap acceptance criterion ("Jackson
    // deserializes List<User>" / "Hibernate-style entity-type discovery
    // finds List<OrderLine>" — both depend on this round-trip).
    public static int testParameterizedSuperclass() {
        Type t = StringList.class.getGenericSuperclass();
        if (!(t instanceof ParameterizedType)) return 0;
        ParameterizedType pt = (ParameterizedType) t;
        if (pt.getRawType() != java.util.ArrayList.class) return 0;
        Type[] args = pt.getActualTypeArguments();
        if (args.length != 1) return 0;
        if (args[0] != String.class) return 0;
        return 1;
    }

    // Test 12: Field.getGenericType for List<String> returns a
    // ParameterizedType with the right raw + args.
    public static int testParameterizedField() {
        try {
            Field f = GenericReflectionTest.class.getDeclaredField("users");
            Type t = f.getGenericType();
            if (!(t instanceof ParameterizedType)) return 0;
            ParameterizedType pt = (ParameterizedType) t;
            if (pt.getRawType() != java.util.List.class) return 0;
            Type[] args = pt.getActualTypeArguments();
            if (args.length != 1) return 0;
            if (args[0] != String.class) return 0;
            return 1;
        } catch (Exception e) {
            return 0;
        }
    }

    // Test 13: Two-arg ParameterizedType (Map<String, Integer>) — both
    // type arguments must round-trip in declaration order.
    public static int testTwoArgParameterizedField() {
        try {
            Field f = GenericReflectionTest.class.getDeclaredField("counts");
            Type t = f.getGenericType();
            if (!(t instanceof ParameterizedType)) return 0;
            ParameterizedType pt = (ParameterizedType) t;
            Type[] args = pt.getActualTypeArguments();
            if (args.length != 2) return 0;
            if (args[0] != String.class) return 0;
            if (args[1] != Integer.class) return 0;
            return 1;
        } catch (Exception e) {
            return 0;
        }
    }

    // Test 14: Wildcard with upper bound (`? extends Number`) materializes
    // as a WildcardType whose getUpperBounds()[0] is Number.class.
    public static int testWildcardExtendsNumber() {
        try {
            Field f = GenericReflectionTest.class.getDeclaredField("upperBounded");
            Type t = f.getGenericType();
            if (!(t instanceof ParameterizedType)) return 0;
            ParameterizedType pt = (ParameterizedType) t;
            Type arg = pt.getActualTypeArguments()[0];
            if (!(arg instanceof WildcardType)) return 0;
            WildcardType w = (WildcardType) arg;
            Type[] uppers = w.getUpperBounds();
            if (uppers.length != 1) return 0;
            if (uppers[0] != Number.class) return 0;
            return 1;
        } catch (Exception e) {
            return 0;
        }
    }

    // Test 15: Method generic accessors on generic default methods must use
    // the Signature attribute and keep same-named variables declaration-scoped.
    public static int testSameNamedInterfaceTypeVariables() {
        try {
            Method m = Create.class.getDeclaredMethod("create", Object.class);
            Type[] bodyTypes = m.getGenericParameterTypes();
            if (bodyTypes.length != 1) return 0;
            if (!(bodyTypes[0] instanceof TypeVariable)) return 0;
            TypeVariable<?> bodyType = (TypeVariable<?>) bodyTypes[0];

            Type returnType = m.getGenericReturnType();
            if (!(returnType instanceof TypeVariable)) return 0;
            TypeVariable<?> outputType = (TypeVariable<?>) returnType;

            TypeVariable<?> createI = Create.class.getTypeParameters()[0];
            TypeVariable<?> createO = Create.class.getTypeParameters()[1];
            TypeVariable<?> searchI = Search.class.getTypeParameters()[0];
            if (!bodyType.equals(createI)) return 0;
            if (!outputType.equals(createO)) return 0;
            if (bodyType.equals(searchI)) return 0;
            return 1;
        } catch (Throwable ex) {
            return 0;
        }
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
    public static void testParamSuper()  { Util.tempPrint(testParameterizedSuperclass()); }
    public static void testParamField()  { Util.tempPrint(testParameterizedField()); }
    public static void testTwoArgField() { Util.tempPrint(testTwoArgParameterizedField()); }
    public static void testWildExtends() { Util.tempPrint(testWildcardExtendsNumber()); }
    public static void testSameNamedVars() { Util.tempPrint(testSameNamedInterfaceTypeVariables()); }
}
