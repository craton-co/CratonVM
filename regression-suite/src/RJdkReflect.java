import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.Externalizable;
import java.io.IOException;
import java.io.NotSerializableException;
import java.io.ObjectInput;
import java.io.ObjectInputStream;
import java.io.ObjectOutput;
import java.io.ObjectOutputStream;
import java.io.ObjectStreamClass;
import java.io.ObjectStreamField;
import java.io.Serializable;
import java.lang.annotation.Annotation;
import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;
import java.lang.reflect.ParameterizedType;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;

/**
 * JDK-only corpus: reflection and serialization -- accessors, constructors,
 * annotations, generated classes.
 *
 * Specifically exercises the reflection INFLATION path: after
 * {@code sun.reflect.inflationThreshold} (15 by default) invocations the JDK
 * generates a bytecode accessor class, which under {@code --jdk-only} must
 * carry {@code ClassOrigin::ReflectionAccessor} -- not a compatibility stub,
 * and not a refusal.
 *
 * Determinism: {@code getDeclaredMethods()} order is unspecified, so every
 * reflective listing is sorted; {@code serialVersionUID} is declared
 * explicitly so it never depends on the compiler's default computation.
 */
public class RJdkReflect {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    @Retention(RetentionPolicy.RUNTIME)
    @Target({ ElementType.TYPE, ElementType.METHOD, ElementType.FIELD, ElementType.PARAMETER })
    public @interface Tag {
        String value();

        int order() default 7;

        String[] extras() default { "a", "b" };

        Class<?> type() default String.class;
    }

    @Retention(RetentionPolicy.CLASS)
    @Target(ElementType.TYPE)
    public @interface ClassRetained {
    }

    @Tag(value = "on-type", order = 1)
    @ClassRetained
    public static class Subject {
        @Tag("on-field")
        private int hidden = 3;
        public String pub = "p";
        private static final long CONST = 99L;
        private final List<String> generic = new ArrayList<>();

        public Subject() {
        }

        private Subject(int hidden) {
            this.hidden = hidden;
        }

        @Tag(value = "on-method", order = 2)
        public int compute(@Tag("on-param") int x) {
            return hidden * x;
        }

        private String secret() {
            return "s" + hidden;
        }

        public static String statics(String a, int b) {
            return a + b;
        }

        protected void thrower() throws IOException {
            throw new IOException("declared-io");
        }
    }

    static void members() throws Exception {
        Class<?> k = Subject.class;
        check(k.getName().equals("RJdkReflect$Subject"), "binary name: " + k.getName());
        check(k.getSimpleName().equals("Subject"), "simple name");
        check(k.getEnclosingClass() == RJdkReflect.class, "enclosing class");
        check(k.getClassLoader() == RJdkReflect.class.getClassLoader(), "loader identity");
        check(k.getPackageName().isEmpty(), "default package");

        List<String> methods = new ArrayList<>();
        for (Method m : k.getDeclaredMethods()) {
            if (!m.isSynthetic()) {
                methods.add(m.getName() + "/" + m.getParameterCount());
            }
        }
        Collections.sort(methods);
        check(methods.equals(Arrays.asList("compute/1", "secret/0", "statics/2", "thrower/0")),
                "declared methods: " + methods);

        List<String> fields = new ArrayList<>();
        for (Field f : k.getDeclaredFields()) {
            if (!f.isSynthetic()) {
                fields.add(f.getName());
            }
        }
        Collections.sort(fields);
        check(fields.equals(Arrays.asList("CONST", "generic", "hidden", "pub")),
                "declared fields: " + fields);

        check(k.getDeclaredConstructors().length == 2, "constructor count");
        Method compute = k.getDeclaredMethod("compute", int.class);
        check(Modifier.isPublic(compute.getModifiers()), "compute is public");
        check(compute.getReturnType() == int.class, "return type");
        Method thrower = k.getDeclaredMethod("thrower");
        check(thrower.getExceptionTypes().length == 1
                && thrower.getExceptionTypes()[0] == IOException.class, "exception types");

        // Generic signature survives on a field.
        Field generic = k.getDeclaredField("generic");
        check(generic.getGenericType() instanceof ParameterizedType, "generic field type");
        check(((ParameterizedType) generic.getGenericType()).getActualTypeArguments()[0]
                == String.class, "type argument");
        System.out.println("CK RJdkReflect methods=" + methods + " fields=" + fields);
    }

    static void accessAndInvoke() throws Exception {
        Class<?> k = Subject.class;
        Subject s = new Subject();

        Method compute = k.getDeclaredMethod("compute", int.class);
        check((Integer) compute.invoke(s, 5) == 15, "public invoke");

        // Subject is a NESTMATE of RJdkReflect, so its private members are
        // reflectively accessible from here without setAccessible.
        Method secret = k.getDeclaredMethod("secret");
        check("s3".equals(secret.invoke(s)), "nestmate private invoke without setAccessible");
        secret.setAccessible(true);
        check("s3".equals(secret.invoke(s)), "private invoke after setAccessible");

        // Strong encapsulation: a java.base internal that is neither exported
        // nor opened must be refused. This is the access check that matters for
        // JDK-only mode -- a fabricated class would have no module to enforce.
        boolean threw = false;
        try {
            Class<?> internal = Class.forName("jdk.internal.misc.Unsafe");
            Method m = internal.getDeclaredMethod("getUnsafe");
            m.setAccessible(true);
            check(m.invoke(null) == null, "unreachable");
        } catch (RuntimeException expected) {
            // InaccessibleObjectException extends RuntimeException.
            threw = expected.getClass().getSimpleName().equals("InaccessibleObjectException");
        } catch (ClassNotFoundException | IllegalAccessException expected) {
            threw = true;
        }
        check(threw, "setAccessible on a non-opened java.base internal must be refused");

        Field hidden = k.getDeclaredField("hidden");

        // L15: the nestmate FIELD arm, asked with NO setAccessible -- the
        // mirror of the nestmate METHOD arm above. Until this block existed,
        // every field assertion in this vector called setAccessible(true)
        // first, so the same-class-only predicate that check_field_access used
        // to apply read green here: the narrowing that replaced it (nestmate,
        // same-package and subclass callers admitted) landed UNEXERCISED.
        // Do not add setAccessible above these four checks.
        //
        // HotSpot 25 oracle, measured 2026-08-12: Subject is a nestmate of
        // RJdkReflect, so a private instance field get AND set both succeed
        // from here without setAccessible, as does a private static final
        // field READ; the same read against a non-nestmate throws
        // IllegalAccessException. The fourth check is the falsifier for the
        // other three -- under a gate that admits too much, or no gate at all,
        // the positive checks still pass and only that one goes red.
        check(hidden.getInt(s) == 3, "nestmate private field get without setAccessible");
        hidden.setInt(s, 4);
        check(hidden.getInt(s) == 4, "nestmate private field set without setAccessible");
        hidden.setInt(s, 3);
        Field nestmateConst = k.getDeclaredField("CONST");
        check(nestmateConst.getLong(null) == 99L,
                "nestmate private static field get without setAccessible");
        boolean nonNestmateRefused = false;
        try {
            Field locked = RJdkReflectOutsider.class.getDeclaredField("locked");
            locked.getInt(new RJdkReflectOutsider());
        } catch (IllegalAccessException expected) {
            nonNestmateRefused = true;
        }
        check(nonNestmateRefused,
                "non-nestmate private field get must be refused without setAccessible");

        hidden.setAccessible(true);
        check(hidden.getInt(s) == 3, "private field get");
        hidden.setInt(s, 11);
        check(s.hidden == 11, "private field set");
        check((Integer) compute.invoke(s, 2) == 22, "invoke after field write");

        Field constant = k.getDeclaredField("CONST");
        constant.setAccessible(true);
        check(constant.getLong(null) == 99L, "static final field read");

        // L15: the CONSTRUCTOR arm of the same rule, asked with NO
        // setAccessible. `Constructor.newInstance` had no member-modifier gate
        // at ALL -- only the two JPMS checks -- so a private constructor was
        // reachable from anywhere, which is more permissive than HotSpot. These
        // three checks are the vector for the narrowing that closes it, and
        // they must stay ABOVE the `priv.setAccessible(true)` block below: the
        // whole reason L15's field narrowing sat unexercised for weeks is that
        // every assertion around it had already set the accessible flag, which
        // short-circuits the gate before it is reached. Do not add setAccessible
        // above these three.
        //
        // HotSpot 25 oracle: `Subject` is a NESTMATE of `RJdkReflect`, so its
        // private constructor is reflectively reachable from here;
        // `RJdkReflectOutsider` is a separate top-level class in the SAME
        // (unnamed) package, so its package-private constructor is reachable
        // and its private one is not. The third check is the falsifier for the
        // first two -- with no gate at all, or with a gate that has collapsed
        // to "same package wins", the two positives still pass and only that
        // one goes red.
        Constructor<?> nestCtor = k.getDeclaredConstructor(int.class);
        check(((Subject) nestCtor.newInstance(7)).hidden == 7,
                "nestmate private constructor newInstance without setAccessible");
        check(RJdkReflectOutsider.class.getDeclaredConstructor().newInstance()
                        instanceof RJdkReflectOutsider,
                "same-package package-private constructor without setAccessible");
        boolean nonNestmateCtorRefused = false;
        try {
            RJdkReflectOutsider.class.getDeclaredConstructor(int.class).newInstance(7);
        } catch (IllegalAccessException expected) {
            nonNestmateCtorRefused = true;
        }
        check(nonNestmateCtorRefused,
                "non-nestmate private constructor must be refused without setAccessible");

        Constructor<?> priv = k.getDeclaredConstructor(int.class);
        priv.setAccessible(true);
        Subject made = (Subject) priv.newInstance(21);
        check(made.hidden == 21, "private constructor newInstance");

        Method statics = k.getDeclaredMethod("statics", String.class, int.class);
        check("x9".equals(statics.invoke(null, "x", 9)), "static invoke");

        // Target exceptions are wrapped.
        Method thrower = k.getDeclaredMethod("thrower");
        thrower.setAccessible(true);
        threw = false;
        try {
            thrower.invoke(s);
        } catch (InvocationTargetException e) {
            threw = e.getCause() instanceof IOException
                    && "declared-io".equals(e.getCause().getMessage());
        }
        check(threw, "target exception must be wrapped in InvocationTargetException");

        // Argument-shape errors.
        threw = false;
        try {
            compute.invoke(s, "not-an-int");
        } catch (IllegalArgumentException expected) {
            threw = true;
        }
        check(threw, "wrong argument type must raise IllegalArgumentException");
        threw = false;
        try {
            compute.invoke(null, 1);
        } catch (NullPointerException expected) {
            threw = true;
        }
        check(threw, "null receiver for an instance method must NPE");

        // Array reflection.
        Object arr = java.lang.reflect.Array.newInstance(int.class, 4);
        java.lang.reflect.Array.setInt(arr, 2, 7);
        check(java.lang.reflect.Array.getLength(arr) == 4, "Array.getLength");
        check(java.lang.reflect.Array.getInt(arr, 2) == 7, "Array.getInt");
        check(arr.getClass() == int[].class, "array class identity");
        check(arr.getClass().getComponentType() == int.class, "componentType");
        System.out.println("CK RJdkReflect invoke ok");
    }

    /**
     * Force the JDK past its reflection inflation threshold so a bytecode
     * accessor class is generated, then keep asserting the same answers.
     */
    static void inflation() throws Exception {
        Method compute = Subject.class.getDeclaredMethod("compute", int.class);
        Method secret = Subject.class.getDeclaredMethod("secret");
        secret.setAccessible(true);
        Field hidden = Subject.class.getDeclaredField("hidden");
        hidden.setAccessible(true);
        Constructor<?> priv = Subject.class.getDeclaredConstructor(int.class);
        priv.setAccessible(true);

        long acc = 0;
        for (int i = 0; i < 200; i++) {
            Subject s = (Subject) priv.newInstance(i);
            acc += (Integer) compute.invoke(s, 2);
            acc += secret.invoke(s).toString().length();
            hidden.setInt(s, i + 1);
            acc += hidden.getInt(s);
        }
        // 39800 (sum 2i) + 690 (sum of "s<i>".length()) + 20100 (sum i+1).
        check(acc == 60590L, "inflation loop accumulator: " + acc);
        System.out.println("CK RJdkReflect inflation acc=" + acc);
    }

    static void annotations() throws Exception {
        Class<?> k = Subject.class;
        Tag t = k.getAnnotation(Tag.class);
        check(t != null, "runtime annotation on the type");
        check(t.value().equals("on-type"), "annotation value");
        check(t.order() == 1, "explicit annotation member");
        check(Arrays.equals(t.extras(), new String[] { "a", "b" }), "default array member");
        check(t.type() == String.class, "default Class member");
        check(t.annotationType() == Tag.class, "annotationType");
        // Annotation instances are proxies; toString/equals/hashCode must work.
        check(t.equals(k.getAnnotation(Tag.class)), "annotation equals");
        check(t.hashCode() == k.getAnnotation(Tag.class).hashCode(), "annotation hashCode");
        check(k.isAnnotationPresent(Tag.class), "isAnnotationPresent");
        // CLASS retention must NOT be visible at runtime.
        check(k.getAnnotation(ClassRetained.class) == null,
                "a CLASS-retention annotation must not be reflectively visible");

        Method compute = k.getDeclaredMethod("compute", int.class);
        check(compute.getAnnotation(Tag.class).order() == 2, "method annotation");
        Annotation[][] params = compute.getParameterAnnotations();
        check(params.length == 1 && params[0].length == 1, "parameter annotation shape");
        check(((Tag) params[0][0]).value().equals("on-param"), "parameter annotation value");
        Field f = k.getDeclaredField("hidden");
        check(f.getAnnotation(Tag.class).value().equals("on-field"), "field annotation");

        List<String> present = new ArrayList<>();
        for (Annotation a : k.getAnnotations()) {
            present.add(a.annotationType().getSimpleName());
        }
        Collections.sort(present);
        check(present.equals(Collections.singletonList("Tag")), "visible annotations: " + present);
        System.out.println("CK RJdkReflect annotations=" + present + " order=" + t.order());
    }

    // ---- serialization ----------------------------------------------------

    static class Node implements Serializable {
        private static final long serialVersionUID = 20260731L;
        int id;
        String name;
        transient String skipped = "not-written";
        Node next;
        int[] data;
        Map<String, Integer> map;

        Node(int id, String name) {
            this.id = id;
            this.name = name;
        }

        private void writeObject(ObjectOutputStream out) throws IOException {
            out.defaultWriteObject();
            out.writeUTF("extra:" + id);
        }

        private void readObject(ObjectInputStream in) throws IOException, ClassNotFoundException {
            in.defaultReadObject();
            skipped = in.readUTF();
        }
    }

    static class Ext implements Externalizable {
        private static final long serialVersionUID = 20260732L;
        String a;
        int b;

        public Ext() {
        }

        Ext(String a, int b) {
            this.a = a;
            this.b = b;
        }

        @Override
        public void writeExternal(ObjectOutput out) throws IOException {
            out.writeUTF(a);
            out.writeInt(b);
        }

        @Override
        public void readExternal(ObjectInput in) throws IOException {
            a = in.readUTF();
            b = in.readInt();
        }
    }

    static class NotSer {
        int x = 1;
    }

    static byte[] write(Object o) throws IOException {
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        try (ObjectOutputStream oos = new ObjectOutputStream(bos)) {
            oos.writeObject(o);
        }
        return bos.toByteArray();
    }

    static Object read(byte[] b) throws Exception {
        try (ObjectInputStream ois = new ObjectInputStream(new ByteArrayInputStream(b))) {
            return ois.readObject();
        }
    }

    static void serialization() throws Exception {
        Node a = new Node(1, "a");
        Node b = new Node(2, "b");
        a.next = b;
        b.next = a; // cycle
        a.data = new int[] { 1, 2, 3 };
        a.map = new TreeMap<>();
        a.map.put("k", 5);

        byte[] bytes = write(a);
        Node back = (Node) read(bytes);
        check(back.id == 1 && back.name.equals("a"), "round-trip scalars");
        check(back.next.id == 2, "round-trip reference");
        check(back.next.next == back, "cyclic back-reference must be restored by identity");
        check(Arrays.equals(back.data, new int[] { 1, 2, 3 }), "round-trip array");
        check(back.map.get("k") == 5, "round-trip map");
        check(back.skipped.equals("extra:1"),
                "transient field must be repopulated by the custom readObject");
        check(back != a, "deserialization produces a NEW graph");

        // Externalizable takes the public no-arg constructor path.
        Ext e = (Ext) read(write(new Ext("x", 9)));
        check(e.a.equals("x") && e.b == 9, "Externalizable round-trip");

        // Serializing a non-serializable object is a NotSerializableException.
        boolean threw = false;
        try {
            write(new NotSer());
        } catch (NotSerializableException expected) {
            threw = expected.getMessage().contains("NotSer");
        }
        check(threw, "non-serializable class must raise NotSerializableException");

        // The serialization descriptor is reflectively describable.
        ObjectStreamClass osc = ObjectStreamClass.lookup(Node.class);
        check(osc != null, "ObjectStreamClass.lookup");
        check(osc.getSerialVersionUID() == 20260731L, "declared serialVersionUID must be used");
        List<String> serialFields = new ArrayList<>();
        for (ObjectStreamField f : osc.getFields()) {
            serialFields.add(f.getName() + ":" + f.getTypeCode());
        }
        Collections.sort(serialFields);
        check(serialFields.equals(Arrays.asList("data:[", "id:I", "map:L", "name:L", "next:L")),
                "serial fields (transient excluded): " + serialFields);
        check(ObjectStreamClass.lookup(NotSer.class) == null,
                "lookup of a non-serializable class must be null");

        // Byte-for-byte stability of the encoding itself.
        check(Arrays.equals(write(new Ext("x", 9)), write(new Ext("x", 9))),
                "serialization must be byte-stable");
        System.out.println("CK RJdkReflect serial fields=" + serialFields
                + " svuid=" + osc.getSerialVersionUID() + " len=" + write(new Ext("x", 9)).length);
    }

    public static void main(String[] args) throws Exception {
        members();
        accessAndInvoke();
        inflation();
        annotations();
        serialization();
        System.out.println("CK RJdkReflect checks=" + checks);
        System.out.println("PASS RJdkReflect (" + checks + " checks)");
    }
}

/**
 * The NON-nestmate half of {@code accessAndInvoke}'s L15 block. A top-level
 * class in the same compilation unit is NOT a nestmate of {@code RJdkReflect}
 * -- its nest host is itself -- so its {@code private} field must stay refused
 * to a reflective read from {@code RJdkReflect} that has not called
 * {@code setAccessible(true)}. It is in the same (unnamed) package on purpose:
 * that isolates the {@code private} rule from the package rule, so a gate that
 * has collapsed to "same package wins" is caught here rather than read as
 * green.
 *
 * <p>Deliberately not a separate {@code src/*.java} file: {@code run.sh}'s list
 * hygiene globs {@code src/*.java} and requires every one to be a listed vector
 * or named in {@code UNREGISTERED_CLASSES}. It has no {@code main} and is never
 * run on its own.
 */
class RJdkReflectOutsider {
    private int locked = 5;

    /**
     * Package-private, and DECLARED rather than left implicit so the pairing
     * with the private one below is visible. {@code RJdkReflect} is in the same
     * runtime package, so a reflective {@code newInstance()} on this one must
     * succeed with no {@code setAccessible(true)}.
     */
    RJdkReflectOutsider() {
    }

    /**
     * The falsifier's subject. Private on a NON-nestmate, so a reflective
     * {@code newInstance(int)} from {@code RJdkReflect} must be refused with no
     * {@code setAccessible(true)} -- {@code private} is nest-scoped (JEP 181)
     * and being in the same package buys nothing. Never called from Java; it
     * exists only to be reached reflectively.
     */
    private RJdkReflectOutsider(int seed) {
        this.locked = seed;
    }

    int visible() {
        return locked;
    }
}
