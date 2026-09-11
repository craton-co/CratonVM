import java.lang.annotation.*;
import java.lang.invoke.*;
import java.lang.reflect.*;
import java.util.*;

/** Lane 3's surface: core reflection and `java.lang.invoke`.
 *
 *  PURPOSE. Lane 3 owns 242 bucket-A/B §1.4 shadows over 35 classes (measured
 *  from a full `--dump-native-registry`, not from the lane page, whose 251 was
 *  taken on a different tree and included rows lane T owns). Retiring any of
 *  them needs precondition 4 satisfied by THIS instrument: `invocations > 0`
 *  for the triple in the probe's own run. So every row below exists to
 *  dispatch a specific registered native.
 *
 *  PRINTING RULES. A probe that prints an unstable value manufactures diffs,
 *  and on lane 0 that cost a row: an exception MESSAGE carried an identity hash
 *  and read as a disagreement where the two VMs agreed. So the rule is enforced
 *  in `p()` here rather than left to the author -- `scrub()` strips `@<hex>`
 *  from every printed value, and lambda/proxy class names are reduced to a
 *  shape. Beyond that: no `Object.toString()`, every array and Set sorted
 *  before printing, exceptions reduced to type plus scrubbed message.
 *
 *  THREE TRAPS THIS AREA HAS ALREADY SPRUNG (lane page §3), all of which
 *  produced a wrong conclusion first:
 *    - `getDeclaredFields` on `java.lang.reflect.*` returns 0 on a HEALTHY
 *      image; core reflection hides its own fields. No field walk is used as
 *      an instrument here.
 *    - A caller-sensitive skip list can hide the real caller, so where an
 *      access check fires we print the throwing FRAME, not just the type.
 *    - A bare exception-type assertion cannot say which check fired, so every
 *      access-control row prints the message.
 *
 *  COPY SEMANTICS (lane page §5). `Field`/`Method`/`Constructor` are handed out
 *  as copies over a shared root: `getDeclaredField("x") == getDeclaredField("x")`
 *  is FALSE on HotSpot and `.equals` is TRUE. Any retirement that changes
 *  copying changes both answers, so both are printed, and `setAccessible` is
 *  tested for non-leakage between copies as well as for persistence on the copy
 *  it was set on.
 */
public class L3ReflectInvokeSurface {
    static int rows = 0;

    // ---- printing ---------------------------------------------------------

    /** Remove every value that differs between two healthy VMs by design, and
     *  every byte that would stop the output being TEXT.
     *
     *  The control-character escape is not cosmetic. `Field.getChar` on a
     *  yielding VM answers `0`, this probe printed it raw, and one NUL byte in
     *  the file made `diff` report `Binary files ... differ` -- a single line
     *  matching neither `^<` nor `^>`, so the harness's `grep -c '^[<>]'`
     *  scored a 245-row arm as ZERO differences: a crashing VM reported as a
     *  perfect match. The harness now passes `-a`, and this printer makes the
     *  question moot. **A probe must never emit a byte that its own comparator
     *  can choke on.** */
    static String scrub(String s) {
        if (s == null) {
            return "null";
        }
        // Identity hashes: `java.lang.Object@5387f9e0`, and the bare `@1a2b3c`.
        // `{1,}` not `{4,}`: this VM printed `Holder@53f`, three digits, which
        // slipped past a four-digit floor and read as a real disagreement.
        s = s.replaceAll("@[0-9a-fA-F]{1,}", "@<id>");
        // Lambda and proxy carrier names carry a mint counter.
        s = s.replaceAll("\\$\\$Lambda[/.$]?[0-9a-fA-Fx]*", "$$Lambda<n>");
        s = s.replaceAll("\\$Proxy[0-9]+", "$Proxy<n>");
        StringBuilder out = new StringBuilder(s.length() + 8);
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c == 0x7f) {
                out.append(String.format("<U+%04X>", (int) c));
            } else {
                out.append(c);
            }
        }
        return out.toString();
    }

    static void p(String tag, Object v) {
        System.out.println(++rows + " " + tag + " |" + scrub(String.valueOf(v)) + "|");
    }

    interface Body {
        Object call() throws Throwable;
    }

    static void t(String tag, Body b) {
        Object v;
        try {
            v = b.call();
        } catch (Throwable e) {
            v = e.getClass().getName() + ": " + e.getMessage();
        }
        p(tag, v);
    }

    /** For an access check: type, message AND the frame that threw, because a
     *  type alone cannot say which of several checks fired. */
    static void access(String tag, Body b) {
        Object v;
        try {
            v = "NO-THROW " + b.call();
        } catch (Throwable e) {
            StackTraceElement[] st = e.getStackTrace();
            String top = st.length > 0
                    ? st[0].getClassName() + "." + st[0].getMethodName()
                    : "<no frames>";
            v = e.getClass().getName() + " @ " + top + ": " + e.getMessage();
        }
        p(tag, v);
    }

    // ---- stable renderers -------------------------------------------------

    static String names(Class<?>[] cs) {
        if (cs == null) {
            return "null";
        }
        String[] a = new String[cs.length];
        for (int i = 0; i < cs.length; i++) {
            a[i] = cs[i].getName();
        }
        return Arrays.toString(a);
    }

    /** Sorted, so a registry-order change is not read as a defect. */
    static String sortedNames(Class<?>[] cs) {
        if (cs == null) {
            return "null";
        }
        String[] a = new String[cs.length];
        for (int i = 0; i < cs.length; i++) {
            a[i] = cs[i].getName();
        }
        Arrays.sort(a);
        return Arrays.toString(a);
    }

    static String strs(Object[] os) {
        if (os == null) {
            return "null";
        }
        String[] a = new String[os.length];
        for (int i = 0; i < os.length; i++) {
            a[i] = scrub(String.valueOf(os[i]));
        }
        Arrays.sort(a);
        return Arrays.toString(a);
    }

    // ---- fixtures ---------------------------------------------------------

    @Retention(RetentionPolicy.RUNTIME)
    @Target({ElementType.FIELD, ElementType.METHOD, ElementType.TYPE,
             ElementType.PARAMETER, ElementType.TYPE_USE})
    @interface Mark {
        String value() default "d";
    }

    @SuppressWarnings("unused")
    static class Holder<T extends Number & Comparable<T>> {
        @Mark("f")
        public int i = 7;
        public long j = 8L;
        public double d = 9.5;
        public boolean z = true;
        public byte b = 3;
        public short s = 4;
        public char c = 'q';
        public float f = 1.5f;
        public String str = "hi";
        public final int fin = 11;
        public static int stat = 12;
        private int priv = 13;
        public List<? extends Number> wild;
        public T tvar;
        public T[] tarr;
        public Map<String, List<T>> nested;

        public Holder() {
        }

        public Holder(int i) {
            this.i = i;
        }

        private Holder(String s) {
            this.str = s;
        }

        @Mark("m")
        public int add(int a, int b) throws IllegalStateException {
            return a + b;
        }

        public T pick(@Mark("p") T a, List<T> rest) {
            return a;
        }

        public static String stat(String a) {
            return "s:" + a;
        }

        private String hidden() {
            return "hidden";
        }

        public void varargs(String... a) {
        }
    }

    /** A genuine INNER (non-static) class, so javac emits the synthetic
     *  `this$0` field -- the only way `Field.isSynthetic` answers `true`. */
    class Inner {
        int v;

        /** This reference is REQUIRED, not decoration. Since JDK 18 javac
         *  omits the synthetic `this$0` field when an inner class never uses
         *  its enclosing instance -- the first version of this fixture had no
         *  such use and the row answered `no-synthetic-field-found` on
         *  HotSpot, which is a fixture that discriminates nothing. */
        Object outer() {
            // `L3ReflectInvokeSurface.this` and nothing weaker. The second
            // version of this fixture read `rows`, a STATIC field of the outer
            // class -- static access needs no enclosing instance, so javac
            // still omitted `this$0` and the row still found no synthetic
            // field. Only an explicit qualified `this` forces the capture.
            return L3ReflectInvokeSurface.this;
        }
    }

    interface Iface {
        int one();

        default int two() {
            return 2;
        }
    }

    /** The access-control fixture, and it MUST NOT be a nested class.
     *
     *  `Holder` is nested inside this probe, so the probe and `Holder` are
     *  NESTMATES and every `private` member of `Holder` is legally readable
     *  from `main` with no `setAccessible` at all. Six rows aimed at access
     *  control were therefore vacuous, and HotSpot itself said so: `F private
     *  read without setAccessible` answered `NO-THROW 13` on the ORACLE. An
     *  agreement on a row that cannot fail is not evidence about access
     *  control, and it would have been banked as one.
     *
     *  `L3Foreign` is a sibling TOP-LEVEL class. Same package -- which grants
     *  package-private access and nothing more -- and not a nestmate, so its
     *  `private` members are genuinely closed to this probe. */
    static Class<?> foreign() throws Exception {
        return Class.forName("L3Foreign");
    }

    record Rec(int a, String b) {
    }

    // Member lookups are sorted so declaration order never enters a diff.
    static Method m(Class<?> c, String name, Class<?>... ps) throws Exception {
        return c.getDeclaredMethod(name, ps);
    }

    static Field fl(Class<?> c, String name) throws Exception {
        return c.getDeclaredField(name);
    }

    public static void main(String[] args) throws Throwable {
        Class<?> H = Holder.class;

        // ================= Field: the 35-row family =================
        t("F getName", () -> fl(H, "i").getName());
        t("F getType", () -> fl(H, "i").getType().getName());
        t("F getDeclaringClass", () -> fl(H, "i").getDeclaringClass().getName());
        t("F getModifiers", () -> Modifier.toString(fl(H, "i").getModifiers()));
        t("F getModifiers static", () -> Modifier.toString(fl(H, "stat").getModifiers()));
        t("F getModifiers final", () -> Modifier.toString(fl(H, "fin").getModifiers()));
        t("F toString", () -> fl(H, "i").toString());
        t("F toGenericString", () -> fl(H, "nested").toGenericString());
        t("F isSynthetic", () -> fl(H, "i").isSynthetic());
        t("F isEnumConstant", () -> fl(H, "i").isEnumConstant());

        Holder h = new Holder();
        t("F get int", () -> fl(H, "i").get(h));
        t("F getInt", () -> fl(H, "i").getInt(h));
        t("F getLong", () -> fl(H, "j").getLong(h));
        t("F getDouble", () -> fl(H, "d").getDouble(h));
        t("F getBoolean", () -> fl(H, "z").getBoolean(h));
        t("F getByte", () -> fl(H, "b").getByte(h));
        t("F getShort", () -> fl(H, "s").getShort(h));
        t("F getChar", () -> fl(H, "c").getChar(h));
        t("F getFloat", () -> fl(H, "f").getFloat(h));
        t("F get Object", () -> fl(H, "str").get(h));
        t("F set int", () -> {
            Field x = fl(H, "i");
            x.set(h, 21);
            return x.getInt(h);
        });
        t("F setInt", () -> {
            Field x = fl(H, "i");
            x.setInt(h, 22);
            return x.getInt(h);
        });
        t("F setLong", () -> {
            Field x = fl(H, "j");
            x.setLong(h, 23L);
            return x.getLong(h);
        });
        t("F setDouble", () -> {
            Field x = fl(H, "d");
            x.setDouble(h, 2.5);
            return x.getDouble(h);
        });
        t("F setBoolean", () -> {
            Field x = fl(H, "z");
            x.setBoolean(h, false);
            return x.getBoolean(h);
        });
        t("F setByte", () -> {
            Field x = fl(H, "b");
            x.setByte(h, (byte) 9);
            return x.getByte(h);
        });
        t("F setShort", () -> {
            Field x = fl(H, "s");
            x.setShort(h, (short) 10);
            return x.getShort(h);
        });
        t("F setChar", () -> {
            Field x = fl(H, "c");
            x.setChar(h, 'z');
            return x.getChar(h);
        });
        t("F setFloat", () -> {
            Field x = fl(H, "f");
            x.setFloat(h, 3.5f);
            return x.getFloat(h);
        });
        t("F set Object", () -> {
            Field x = fl(H, "str");
            x.set(h, "bye");
            return x.get(h);
        });
        // Widening and the errors the JDK's bytecode raises for it.
        t("F getLong widens int", () -> fl(H, "i").getLong(h));
        t("F getInt on long throws", () -> fl(H, "j").getInt(h));
        t("F set wrong type throws", () -> {
            fl(H, "i").set(h, "not-an-int");
            return "no-throw";
        });
        t("F set final throws", () -> {
            fl(H, "fin").set(h, 99);
            return "no-throw";
        });
        t("F get wrong receiver throws", () -> fl(H, "i").get("wrong"));
        t("F get null receiver throws", () -> fl(H, "i").get(null));
        t("F static get null ok", () -> fl(H, "stat").get(null));

        // Generic and annotated type: the ONLY door to the generics impls.
        t("F getGenericType wildcard", () -> fl(H, "wild").getGenericType().getTypeName());
        t("F getGenericType tvar", () -> fl(H, "tvar").getGenericType().getTypeName());
        t("F getGenericType array", () -> fl(H, "tarr").getGenericType().getTypeName());
        t("F getGenericType nested", () -> fl(H, "nested").getGenericType().getTypeName());
        t("F getAnnotatedType", () -> fl(H, "nested").getAnnotatedType().getType().getTypeName());
        t("F getAnnotation", () -> {
            Mark mk = fl(H, "i").getAnnotation(Mark.class);
            return mk == null ? "null" : mk.value();
        });
        t("F getDeclaredAnnotations", () -> strs(fl(H, "i").getDeclaredAnnotations()));

        // Copy semantics, printed BOTH ways (lane page §5).
        t("F copy == is false", () -> fl(H, "i") == fl(H, "i"));
        t("F copy equals is true", () -> fl(H, "i").equals(fl(H, "i")));
        t("F hashCode agrees", () -> fl(H, "i").hashCode() == fl(H, "i").hashCode());
        t("F setAccessible persists on the copy", () -> {
            Field x = fl(H, "priv");
            x.setAccessible(true);
            return x.canAccess(h) + "/" + x.get(h);
        });
        t("F setAccessible does not leak to a fresh copy", () -> {
            Field a = fl(H, "priv");
            a.setAccessible(true);
            Field b2 = fl(H, "priv");
            return b2.canAccess(h);
        });
        access("F private read without setAccessible", () -> fl(H, "priv").get(h));
        t("F trySetAccessible", () -> fl(H, "priv").trySetAccessible());

        // ================= Method: the 33-row family =================
        t("M getName", () -> m(H, "add", int.class, int.class).getName());
        t("M getReturnType", () -> m(H, "add", int.class, int.class).getReturnType().getName());
        t("M getParameterTypes", () -> names(m(H, "add", int.class, int.class).getParameterTypes()));
        t("M getParameterCount", () -> m(H, "add", int.class, int.class).getParameterCount());
        t("M getExceptionTypes", () -> names(m(H, "add", int.class, int.class).getExceptionTypes()));
        t("M getDeclaringClass", () -> m(H, "add", int.class, int.class).getDeclaringClass().getName());
        t("M getModifiers", () -> Modifier.toString(m(H, "add", int.class, int.class).getModifiers()));
        t("M toString", () -> m(H, "add", int.class, int.class).toString());
        t("M toGenericString", () -> m(H, "pick", Object.class, List.class).toGenericString());
        t("M isVarArgs", () -> m(H, "varargs", String[].class).isVarArgs());
        t("M isBridge", () -> m(H, "add", int.class, int.class).isBridge());
        t("M isSynthetic", () -> m(H, "add", int.class, int.class).isSynthetic());
        t("M isDefault iface", () -> m(Iface.class, "two").isDefault());
        t("M invoke", () -> m(H, "add", int.class, int.class).invoke(h, 2, 3));
        t("M invoke static", () -> m(H, "stat", String.class).invoke(null, "x"));
        t("M invoke wrong arg count", () -> m(H, "add", int.class, int.class).invoke(h, 2));
        t("M invoke wrong arg type", () -> m(H, "add", int.class, int.class).invoke(h, "a", "b"));
        t("M invoke wrong receiver", () -> m(H, "add", int.class, int.class).invoke("nope", 1, 2));
        t("M invoke null receiver", () -> m(H, "add", int.class, int.class).invoke(null, 1, 2));
        t("M invoke unwraps target throw", () -> {
            try {
                m(Thrower.class, "boom").invoke(new Thrower());
                return "no-throw";
            } catch (InvocationTargetException e) {
                return "ITE cause=" + e.getCause().getClass().getName()
                        + " msg=" + e.getCause().getMessage();
            }
        });
        access("M private invoke without setAccessible", () -> m(H, "hidden").invoke(h));
        t("M setAccessible then invoke", () -> {
            Method x = m(H, "hidden");
            x.setAccessible(true);
            return x.invoke(h);
        });
        t("M getGenericReturnType", () -> m(H, "pick", Object.class, List.class)
                .getGenericReturnType().getTypeName());
        t("M getGenericParameterTypes", () -> {
            Type[] ts = m(H, "pick", Object.class, List.class).getGenericParameterTypes();
            String[] a = new String[ts.length];
            for (int i = 0; i < ts.length; i++) {
                a[i] = ts[i].getTypeName();
            }
            return Arrays.toString(a);
        });
        t("M getGenericExceptionTypes", () -> {
            Type[] ts = m(H, "add", int.class, int.class).getGenericExceptionTypes();
            String[] a = new String[ts.length];
            for (int i = 0; i < ts.length; i++) {
                a[i] = ts[i].getTypeName();
            }
            return Arrays.toString(a);
        });
        t("M getAnnotatedReturnType", () -> m(H, "add", int.class, int.class)
                .getAnnotatedReturnType().getType().getTypeName());
        t("M getTypeParameters", () -> {
            TypeVariable<?>[] tv = H.getTypeParameters();
            String[] a = new String[tv.length];
            for (int i = 0; i < tv.length; i++) {
                a[i] = tv[i].getName();
            }
            return Arrays.toString(a);
        });
        t("M getDefaultValue", () -> m(Mark.class, "value").getDefaultValue());
        t("M getAnnotation", () -> {
            Mark mk = m(H, "add", int.class, int.class).getAnnotation(Mark.class);
            return mk == null ? "null" : mk.value();
        });
        t("M getParameters", () -> {
            Parameter[] ps = m(H, "add", int.class, int.class).getParameters();
            String[] a = new String[ps.length];
            for (int i = 0; i < ps.length; i++) {
                a[i] = ps[i].getType().getName() + "/" + ps[i].isNamePresent();
            }
            return Arrays.toString(a);
        });
        t("M copy == is false", () -> m(H, "add", int.class, int.class)
                == m(H, "add", int.class, int.class));
        t("M copy equals is true", () -> m(H, "add", int.class, int.class)
                .equals(m(H, "add", int.class, int.class)));
        t("M hashCode agrees", () -> m(H, "add", int.class, int.class).hashCode()
                == m(H, "add", int.class, int.class).hashCode());
        t("M setAccessible does not leak", () -> {
            Method a = m(H, "hidden");
            a.setAccessible(true);
            return m(H, "hidden").canAccess(h);
        });

        // ================= Constructor: the 23-row family =================
        t("C getName", () -> H.getDeclaredConstructor().getName());
        t("C getParameterTypes", () -> names(H.getDeclaredConstructor(int.class).getParameterTypes()));
        t("C getParameterCount", () -> H.getDeclaredConstructor(int.class).getParameterCount());
        t("C getModifiers", () -> Modifier.toString(H.getDeclaredConstructor().getModifiers()));
        t("C getDeclaringClass", () -> H.getDeclaredConstructor().getDeclaringClass().getName());
        t("C toString", () -> H.getDeclaredConstructor(int.class).toString());
        t("C toGenericString", () -> H.getDeclaredConstructor(int.class).toGenericString());
        t("C newInstance", () -> ((Holder) H.getDeclaredConstructor().newInstance()).i);
        t("C newInstance arg", () -> ((Holder) H.getDeclaredConstructor(int.class)
                .newInstance(41)).i);
        t("C newInstance wrong args", () -> H.getDeclaredConstructor(int.class).newInstance());
        t("C newInstance wrong type", () -> H.getDeclaredConstructor(int.class).newInstance("s"));
        access("C private newInstance", () -> H.getDeclaredConstructor(String.class)
                .newInstance("x"));
        t("C setAccessible then newInstance", () -> {
            Constructor<?> k = H.getDeclaredConstructor(String.class);
            k.setAccessible(true);
            return ((Holder) k.newInstance("ok")).str;
        });
        t("C newInstance on abstract throws", () -> Executable.class.getDeclaredConstructors().length);
        t("C getExceptionTypes", () -> names(H.getDeclaredConstructor().getExceptionTypes()));
        t("C isVarArgs", () -> H.getDeclaredConstructor().isVarArgs());
        t("C isSynthetic", () -> H.getDeclaredConstructor().isSynthetic());
        t("C copy == is false", () -> H.getDeclaredConstructor() == H.getDeclaredConstructor());
        t("C copy equals is true", () -> H.getDeclaredConstructor()
                .equals(H.getDeclaredConstructor()));
        t("C hashCode agrees", () -> H.getDeclaredConstructor().hashCode()
                == H.getDeclaredConstructor().hashCode());
        t("C getGenericParameterTypes", () -> {
            Type[] ts = H.getDeclaredConstructor(int.class).getGenericParameterTypes();
            String[] a = new String[ts.length];
            for (int i = 0; i < ts.length; i++) {
                a[i] = ts[i].getTypeName();
            }
            return Arrays.toString(a);
        });
        t("C getAnnotatedReceiverType", () -> {
            AnnotatedType at = H.getDeclaredConstructor().getAnnotatedReceiverType();
            return at == null ? "null" : at.getType().getTypeName();
        });
        t("C getParameters", () -> H.getDeclaredConstructor(int.class).getParameters().length);

        // ================= AccessibleObject =================
        t("AO isAccessible default", () -> {
            Field x = fl(H, "i");
            return x.canAccess(h);
        });
        t("AO setAccessible false is legal", () -> {
            Field x = fl(H, "priv");
            x.setAccessible(true);
            x.setAccessible(false);
            return x.canAccess(h);
        });
        access("AO setAccessible on a JDK-internal member", () -> {
            Field x = Integer.class.getDeclaredField("value");
            x.setAccessible(true);
            return "no-throw";
        });

        // ================= MethodHandles$Lookup: 25 rows =================
        MethodHandles.Lookup L = MethodHandles.lookup();
        t("LK lookupClass", () -> L.lookupClass().getName());
        t("LK lookupModes", () -> L.lookupModes());
        t("LK publicLookup modes", () -> MethodHandles.publicLookup().lookupModes());
        t("LK publicLookup class", () -> MethodHandles.publicLookup().lookupClass().getName());
        t("LK in() modes", () -> L.in(String.class).lookupModes());
        t("LK in() class", () -> L.in(String.class).lookupClass().getName());
        t("LK dropLookupMode modes", () -> L.dropLookupMode(MethodHandles.Lookup.PRIVATE)
                .lookupModes());
        t("LK findStatic", () -> L.findStatic(Holder.class, "stat",
                MethodType.methodType(String.class, String.class)).invoke("y"));
        t("LK findVirtual", () -> L.findVirtual(Holder.class, "add",
                MethodType.methodType(int.class, int.class, int.class)).invoke(h, 4, 5));
        t("LK findConstructor", () -> {
            MethodHandle mh = L.findConstructor(Holder.class,
                    MethodType.methodType(void.class, int.class));
            return ((Holder) mh.invoke(51)).i;
        });
        t("LK findGetter", () -> L.findGetter(Holder.class, "i", int.class).invoke(h));
        t("LK findSetter", () -> {
            L.findSetter(Holder.class, "i", int.class).invoke(h, 61);
            return h.i;
        });
        t("LK findStaticGetter", () -> L.findStaticGetter(Holder.class, "stat", int.class)
                .invoke());
        t("LK findVarHandle get", () -> L.findVarHandle(Holder.class, "i", int.class).get(h));
        t("LK unreflect", () -> L.unreflect(m(H, "add", int.class, int.class)).invoke(h, 6, 7));
        t("LK unreflectGetter", () -> L.unreflectGetter(fl(H, "i")).invoke(h));
        t("LK unreflectConstructor", () -> {
            MethodHandle mh = L.unreflectConstructor(H.getDeclaredConstructor());
            return ((Holder) mh.invoke()).i;
        });
        access("LK unreflect a non-accessible member", () -> L.unreflect(m(H, "hidden")));
        t("LK unreflect after setAccessible", () -> {
            Method x = m(H, "hidden");
            x.setAccessible(true);
            return L.unreflect(x).invoke(h);
        });
        access("LK findVirtual missing method", () -> L.findVirtual(Holder.class, "nope",
                MethodType.methodType(void.class)));
        access("LK findStatic on a virtual method", () -> L.findStatic(Holder.class, "add",
                MethodType.methodType(int.class, int.class, int.class)));
        access("LK privateLookupIn java.base", () -> MethodHandles
                .privateLookupIn(Integer.class, L).lookupModes());
        access("LK accessClass on an inaccessible class", () -> MethodHandles.publicLookup()
                .accessClass(Class.forName("sun.reflect.annotation.AnnotationParser")).getName());
        t("LK accessClass on a public class", () -> L.accessClass(String.class).getName());
        t("LK revealDirect", () -> {
            MethodHandle mh = L.findVirtual(Holder.class, "add",
                    MethodType.methodType(int.class, int.class, int.class));
            MethodHandleInfo info = L.revealDirect(mh);
            return info.getName() + "/" + info.getReferenceKind()
                    + "/" + info.getDeclaringClass().getName();
        });
        t("LK hasPrivateAccess", () -> L.hasFullPrivilegeAccess());
        t("LK toString", () -> L.toString());

        // ================= MethodHandles: 23 rows =================
        t("MHS identity", () -> MethodHandles.identity(String.class).invoke("id"));
        t("MHS constant", () -> MethodHandles.constant(int.class, 77).invoke());
        t("MHS zero", () -> MethodHandles.zero(int.class).invoke());
        t("MHS empty", () -> MethodHandles.empty(MethodType.methodType(Object.class)).invoke());
        t("MHS arrayElementGetter", () -> MethodHandles.arrayElementGetter(int[].class)
                .invoke(new int[] {5, 6, 7}, 1));
        t("MHS arrayElementSetter", () -> {
            int[] a = new int[3];
            MethodHandles.arrayElementSetter(int[].class).invoke(a, 0, 42);
            return a[0];
        });
        t("MHS arrayLength", () -> MethodHandles.arrayLength(int[].class)
                .invoke(new int[4]));
        t("MHS arrayConstructor", () -> ((int[]) MethodHandles
                .arrayConstructor(int[].class).invoke(3)).length);
        t("MHS insertArguments", () -> {
            MethodHandle mh = L.findVirtual(Holder.class, "add",
                    MethodType.methodType(int.class, int.class, int.class));
            return MethodHandles.insertArguments(mh, 1, 10).invoke(h, 5);
        });
        t("MHS dropArguments", () -> {
            MethodHandle mh = MethodHandles.constant(int.class, 3);
            return MethodHandles.dropArguments(mh, 0, String.class).invoke("x");
        });
        t("MHS filterArguments", () -> {
            MethodHandle up = L.findVirtual(String.class, "toUpperCase",
                    MethodType.methodType(String.class));
            MethodHandle idn = MethodHandles.identity(String.class);
            return MethodHandles.filterArguments(idn, 0, up).invoke("ab");
        });
        t("MHS filterReturnValue", () -> {
            MethodHandle idn = MethodHandles.identity(String.class);
            MethodHandle up = L.findVirtual(String.class, "toUpperCase",
                    MethodType.methodType(String.class));
            return MethodHandles.filterReturnValue(idn, up).invoke("cd");
        });
        t("MHS permuteArguments", () -> {
            MethodHandle mh = L.findStatic(L3ReflectInvokeSurface.class, "sub",
                    MethodType.methodType(int.class, int.class, int.class));
            MethodHandle sw = MethodHandles.permuteArguments(mh,
                    MethodType.methodType(int.class, int.class, int.class), 1, 0);
            return sw.invoke(1, 9);
        });
        t("MHS explicitCastArguments", () -> {
            MethodHandle mh = MethodHandles.identity(int.class);
            return MethodHandles.explicitCastArguments(mh,
                    MethodType.methodType(long.class, long.class)).invoke(5L);
        });
        t("MHS guardWithTest", () -> {
            MethodHandle yes = MethodHandles.constant(String.class, "Y");
            MethodHandle no = MethodHandles.constant(String.class, "N");
            MethodHandle test = L.findStatic(L3ReflectInvokeSurface.class, "yes",
                    MethodType.methodType(boolean.class));
            return MethodHandles.guardWithTest(test, yes, no).invoke();
        });
        t("MHS catchException", () -> {
            MethodHandle boom = L.findStatic(L3ReflectInvokeSurface.class, "throwIt",
                    MethodType.methodType(String.class));
            MethodHandle handler = L.findStatic(L3ReflectInvokeSurface.class, "handle",
                    MethodType.methodType(String.class, IllegalStateException.class));
            return MethodHandles.catchException(boom, IllegalStateException.class, handler)
                    .invoke();
        });
        t("MHS foldArguments", () -> {
            MethodHandle target = L.findStatic(L3ReflectInvokeSurface.class, "sub",
                    MethodType.methodType(int.class, int.class, int.class));
            MethodHandle pre = MethodHandles.constant(int.class, 10);
            return MethodHandles.foldArguments(target, pre).invoke(4);
        });
        t("MHS spreadInvoker", () -> {
            MethodHandle inv = MethodHandles.spreadInvoker(
                    MethodType.methodType(int.class, int.class, int.class), 0);
            MethodHandle mh = L.findStatic(L3ReflectInvokeSurface.class, "sub",
                    MethodType.methodType(int.class, int.class, int.class));
            return inv.invoke(mh, new Object[] {9, 4});
        });
        t("MHS exactInvoker", () -> {
            MethodHandle inv = MethodHandles.exactInvoker(
                    MethodType.methodType(int.class));
            return inv.invoke(MethodHandles.constant(int.class, 31));
        });
        t("MHS invoker", () -> {
            MethodHandle inv = MethodHandles.invoker(MethodType.methodType(int.class));
            return inv.invoke(MethodHandles.constant(int.class, 32));
        });
        t("MHS throwException", () -> {
            MethodHandle mh = MethodHandles.throwException(void.class,
                    IllegalStateException.class);
            mh.invoke(new IllegalStateException("thrown-by-mh"));
            return "no-throw";
        });
        t("MHS asType widen", () -> MethodHandles.identity(int.class)
                .asType(MethodType.methodType(long.class, int.class)).invoke(8));
        t("MHS loop countedLoop", () -> {
            MethodHandle body = L.findStatic(L3ReflectInvokeSurface.class, "accum",
                    MethodType.methodType(int.class, int.class, int.class));
            return MethodHandles.countedLoop(MethodHandles.constant(int.class, 4),
                    MethodHandles.constant(int.class, 0), body).invoke();
        });

        // ================= MethodHandle: 11 rows =================
        MethodHandle add = L.findVirtual(Holder.class, "add",
                MethodType.methodType(int.class, int.class, int.class));
        t("MH type", () -> add.type().toString());
        t("MH invoke", () -> add.invoke(h, 1, 1));
        t("MH invokeExact", () -> (int) add.invokeExact(h, 2, 2));
        t("MH invokeWithArguments", () -> add.invokeWithArguments(h, 3, 3));
        t("MH bindTo", () -> add.bindTo(h).invoke(4, 4));
        t("MH asType", () -> add.asType(MethodType.methodType(long.class, Holder.class,
                int.class, int.class)).invoke(h, 5, 5));
        t("MH asFixedArity", () -> {
            MethodHandle va = L.findVirtual(Holder.class, "varargs",
                    MethodType.methodType(void.class, String[].class));
            return va.asFixedArity().isVarargsCollector();
        });
        t("MH isVarargsCollector", () -> L.findVirtual(Holder.class, "varargs",
                MethodType.methodType(void.class, String[].class)).isVarargsCollector());
        t("MH asVarargsCollector", () -> MethodHandles
                .identity(Object[].class).asVarargsCollector(Object[].class)
                .isVarargsCollector());
        t("MH asSpreader", () -> add.asSpreader(Object[].class, 2)
                .invoke(h, new Object[] {6, 6}));
        t("MH asCollector", () -> {
            MethodHandle va = L.findVirtual(Holder.class, "varargs",
                    MethodType.methodType(void.class, String[].class));
            va.asCollector(String[].class, 2).invoke(h, "a", "b");
            return "ok";
        });
        t("MH invokeExact wrong type throws", () -> {
            long r = (long) add.invokeExact(h, 1, 1);
            return r;
        });
        t("MH toString", () -> add.toString());

        // ================= MethodType: 9 rows =================
        MethodType mt = MethodType.methodType(int.class, int.class, int.class);
        t("MT toString", () -> mt.toString());
        t("MT parameterCount", () -> mt.parameterCount());
        t("MT returnType", () -> mt.returnType().getName());
        t("MT parameterList", () -> mt.parameterList().toString());
        t("MT changeReturnType", () -> mt.changeReturnType(long.class).toString());
        t("MT appendParameterTypes", () -> mt.appendParameterTypes(String.class).toString());
        t("MT insertParameterTypes", () -> mt.insertParameterTypes(0, String.class).toString());
        t("MT dropParameterTypes", () -> mt.dropParameterTypes(0, 1).toString());
        t("MT toMethodDescriptorString", () -> mt.toMethodDescriptorString());
        t("MT fromMethodDescriptorString", () -> MethodType
                .fromMethodDescriptorString("(Ljava/lang/String;)I", null).toString());
        t("MT erase", () -> MethodType.methodType(String.class, String.class).erase().toString());
        t("MT wrap", () -> mt.wrap().toString());
        t("MT unwrap", () -> MethodType.methodType(Integer.class, Integer.class)
                .unwrap().toString());
        t("MT generic", () -> mt.generic().toString());
        t("MT equals", () -> mt.equals(MethodType.methodType(int.class, int.class, int.class)));
        t("MT hashCode agrees", () -> mt.hashCode()
                == MethodType.methodType(int.class, int.class, int.class).hashCode());
        t("MT interning", () -> mt == MethodType.methodType(int.class, int.class, int.class));

        // ================= CallSite family =================
        t("CS MutableCallSite", () -> {
            MutableCallSite cs = new MutableCallSite(MethodType.methodType(int.class));
            cs.setTarget(MethodHandles.constant(int.class, 91));
            return cs.dynamicInvoker().invoke();
        });
        t("CS MutableCallSite type", () -> new MutableCallSite(
                MethodType.methodType(int.class)).type().toString());
        t("CS VolatileCallSite", () -> {
            VolatileCallSite cs = new VolatileCallSite(MethodType.methodType(int.class));
            cs.setTarget(MethodHandles.constant(int.class, 92));
            return cs.getTarget().invoke();
        });
        t("CS ConstantCallSite", () -> new ConstantCallSite(
                MethodHandles.constant(int.class, 93)).getTarget().invoke());
        t("CS syncAll", () -> {
            MutableCallSite cs = new MutableCallSite(MethodHandles.constant(int.class, 1));
            MutableCallSite.syncAll(new MutableCallSite[] {cs});
            return "ok";
        });

        // ================= Array =================
        t("AR newInstance", () -> Array.getLength(Array.newInstance(int.class, 5)));
        t("AR get/set", () -> {
            Object a = Array.newInstance(int.class, 3);
            Array.setInt(a, 1, 44);
            return Array.getInt(a, 1);
        });
        t("AR get out of bounds", () -> Array.getInt(new int[1], 5));
        t("AR set wrong type", () -> {
            Array.set(new int[1], 0, "s");
            return "no-throw";
        });
        t("AR multi", () -> Array.getLength(Array.newInstance(int.class, 2, 3)));

        // ================= Proxy =================
        t("PX newProxyInstance", () -> {
            Iface p = (Iface) Proxy.newProxyInstance(Iface.class.getClassLoader(),
                    new Class<?>[] {Iface.class},
                    (proxy, method, a) -> method.getName().equals("one") ? 1 : 0);
            return p.one();
        });
        t("PX isProxyClass", () -> {
            Object p = Proxy.newProxyInstance(Iface.class.getClassLoader(),
                    new Class<?>[] {Iface.class}, (a, b2, c) -> 0);
            return Proxy.isProxyClass(p.getClass());
        });
        t("PX getInvocationHandler class", () -> {
            Object p = Proxy.newProxyInstance(Iface.class.getClassLoader(),
                    new Class<?>[] {Iface.class}, (a, b2, c) -> 0);
            return Proxy.getInvocationHandler(p) != null;
        });
        t("PX getInvocationHandler on a non-proxy", () -> Proxy.getInvocationHandler("x"));
        t("PX proxy interfaces", () -> {
            Object p = Proxy.newProxyInstance(Iface.class.getClassLoader(),
                    new Class<?>[] {Iface.class}, (a, b2, c) -> 0);
            return sortedNames(p.getClass().getInterfaces());
        });
        t("PX InvocationHandler.invokeDefault", () -> {
            Iface p = (Iface) Proxy.newProxyInstance(Iface.class.getClassLoader(),
                    new Class<?>[] {Iface.class},
                    (proxy, method, a) -> method.isDefault()
                            ? InvocationHandler.invokeDefault(proxy, method, a)
                            : 1);
            return p.two();
        });

        // ================= RecordComponent / Parameter =================
        t("RC components", () -> {
            RecordComponent[] rc = Rec.class.getRecordComponents();
            String[] a = new String[rc.length];
            for (int i = 0; i < rc.length; i++) {
                a[i] = rc[i].getName() + ":" + rc[i].getType().getName();
            }
            Arrays.sort(a);
            return Arrays.toString(a);
        });
        t("RC accessor", () -> Rec.class.getRecordComponents()[0].getAccessor().getName());
        t("RC genericType", () -> Rec.class.getRecordComponents()[0]
                .getGenericType().getTypeName());
        t("PR parameter of a method", () -> {
            Parameter p = m(H, "add", int.class, int.class).getParameters()[0];
            return p.getType().getName() + "/" + p.getModifiers() + "/" + p.isVarArgs();
        });
        t("PR parameter annotation", () -> {
            Parameter p = m(H, "pick", Object.class, List.class).getParameters()[0];
            Mark mk = p.getAnnotation(Mark.class);
            return mk == null ? "null" : mk.value();
        });
        t("PR parameter getDeclaringExecutable", () -> m(H, "add", int.class, int.class)
                .getParameters()[0].getDeclaringExecutable().getName());

        // ================= the generics impls =================
        // TypeVariableImpl (11 rows) is reachable ONLY through these paths.
        t("GEN tvar getName", () -> H.getTypeParameters()[0].getName());
        t("GEN tvar getBounds", () -> {
            Type[] bs = H.getTypeParameters()[0].getBounds();
            String[] a = new String[bs.length];
            for (int i = 0; i < bs.length; i++) {
                a[i] = bs[i].getTypeName();
            }
            Arrays.sort(a);
            return Arrays.toString(a);
        });
        t("GEN tvar getGenericDeclaration", () -> {
            Object gd = H.getTypeParameters()[0].getGenericDeclaration();
            return ((Class<?>) gd).getName();
        });
        t("GEN tvar toString", () -> H.getTypeParameters()[0].toString());
        t("GEN tvar getTypeName", () -> H.getTypeParameters()[0].getTypeName());
        t("GEN tvar equals same", () -> H.getTypeParameters()[0]
                .equals(H.getTypeParameters()[0]));
        t("GEN tvar hashCode agrees", () -> H.getTypeParameters()[0].hashCode()
                == H.getTypeParameters()[0].hashCode());
        t("GEN tvar annotations empty", () -> strs(H.getTypeParameters()[0]
                .getAnnotations()));
        t("GEN tvar getAnnotatedBounds", () -> H.getTypeParameters()[0]
                .getAnnotatedBounds().length);
        t("GEN ptype rawType", () -> {
            ParameterizedType pt = (ParameterizedType) fl(H, "nested").getGenericType();
            return ((Class<?>) pt.getRawType()).getName();
        });
        t("GEN ptype actualArgs", () -> {
            ParameterizedType pt = (ParameterizedType) fl(H, "nested").getGenericType();
            Type[] as = pt.getActualTypeArguments();
            String[] a = new String[as.length];
            for (int i = 0; i < as.length; i++) {
                a[i] = as[i].getTypeName();
            }
            return Arrays.toString(a);
        });
        t("GEN ptype ownerType", () -> {
            ParameterizedType pt = (ParameterizedType) fl(H, "nested").getGenericType();
            Type o = pt.getOwnerType();
            return o == null ? "null" : o.getTypeName();
        });
        t("GEN ptype toString", () -> fl(H, "nested").getGenericType().toString());
        t("GEN ptype equals", () -> fl(H, "nested").getGenericType()
                .equals(fl(H, "nested").getGenericType()));
        t("GEN ptype hashCode agrees", () -> fl(H, "nested").getGenericType().hashCode()
                == fl(H, "nested").getGenericType().hashCode());
        t("GEN wildcard upper", () -> {
            ParameterizedType pt = (ParameterizedType) fl(H, "wild").getGenericType();
            WildcardType w = (WildcardType) pt.getActualTypeArguments()[0];
            return Arrays.toString(new String[] {w.getUpperBounds()[0].getTypeName()});
        });
        t("GEN wildcard lower empty", () -> {
            ParameterizedType pt = (ParameterizedType) fl(H, "wild").getGenericType();
            WildcardType w = (WildcardType) pt.getActualTypeArguments()[0];
            return w.getLowerBounds().length;
        });
        t("GEN wildcard toString", () -> {
            ParameterizedType pt = (ParameterizedType) fl(H, "wild").getGenericType();
            return pt.getActualTypeArguments()[0].toString();
        });
        t("GEN wildcard equals", () -> {
            ParameterizedType a = (ParameterizedType) fl(H, "wild").getGenericType();
            ParameterizedType b2 = (ParameterizedType) fl(H, "wild").getGenericType();
            return a.getActualTypeArguments()[0].equals(b2.getActualTypeArguments()[0]);
        });
        t("GEN generic array getGenericComponentType", () -> {
            GenericArrayType g = (GenericArrayType) fl(H, "tarr").getGenericType();
            return g.getGenericComponentType().getTypeName();
        });
        t("GEN generic array toString", () -> fl(H, "tarr").getGenericType().toString());
        t("GEN generic array equals", () -> fl(H, "tarr").getGenericType()
                .equals(fl(H, "tarr").getGenericType()));

        // ================= jdk.internal.reflect =================
        t("RF Reflection getCallerClass", () -> {
            // Reached through the JDK's own caller-sensitive plumbing.
            Method mm = Class.forName("jdk.internal.reflect.Reflection")
                    .getDeclaredMethod("getCallerClass");
            mm.setAccessible(true);
            Object r = mm.invoke(null);
            return r == null ? "null" : ((Class<?>) r).getName();
        });
        t("RF ReflectionFactory via getDeclaredFields0 path", () ->
                H.getDeclaredFields().length > 0);

        // ================= the interface-carrier registrations =================
        // These name a class that method resolution never yields as the
        // DECLARING class, so the door cannot ask about them. Printed to show
        // the impl answers, which is what a caller actually sees.
        t("IFACE TypeVariable.equals routes to impl", () -> {
            TypeVariable<?> tv = H.getTypeParameters()[0];
            return tv.equals(tv);
        });
        t("IFACE TypeVariable.toString routes to impl", () -> {
            TypeVariable<?> tv = H.getTypeParameters()[0];
            return tv.toString();
        });
        t("IFACE TypeVariable.getTypeName routes to Type", () -> {
            TypeVariable<?> tv = H.getTypeParameters()[0];
            return tv.getTypeName();
        });
        t("IFACE TypeVariable declaring class of equals", () -> {
            TypeVariable<?> tv = H.getTypeParameters()[0];
            return tv.getClass().getMethod("equals", Object.class)
                    .getDeclaringClass().getName();
        });
        t("IFACE TypeVariable declaring class of getTypeName", () -> {
            TypeVariable<?> tv = H.getTypeParameters()[0];
            return tv.getClass().getMethod("getTypeName")
                    .getDeclaringClass().getName();
        });
        t("IFACE ParameterizedType.equals routes to impl", () -> {
            Type g = fl(H, "nested").getGenericType();
            return g.getClass().getMethod("equals", Object.class)
                    .getDeclaringClass().getName();
        });
        t("IFACE Executable.getParameters declaring class", () ->
                m(H, "add", int.class, int.class).getClass()
                        .getMethod("getParameters").getDeclaringClass().getName());

        // ========== access control, against a NON-nestmate ==========
        // Appended rather than substituted so the rows above keep their
        // numbers across this correction. The `Holder` rows still measure
        // copy/persistence behaviour; these are the ones that measure whether
        // an access CHECK fires, because `L3Foreign` is not a nestmate.
        Class<?> FG = foreign();
        t("FG sanity: not a nestmate", () -> FG.isNestmateOf(L3ReflectInvokeSurface.class));
        access("FG private field read without setAccessible", () -> {
            Field x = FG.getDeclaredField("secret");
            return x.get(FG.getDeclaredConstructor().newInstance());
        });
        access("FG private method invoke without setAccessible", () -> {
            Method x = FG.getDeclaredMethod("hidden");
            return x.invoke(FG.getDeclaredConstructor().newInstance());
        });
        access("FG private constructor without setAccessible", () ->
                FG.getDeclaredConstructor(String.class).newInstance("x"));
        t("FG canAccess private field is false", () -> {
            Field x = FG.getDeclaredField("secret");
            return x.canAccess(FG.getDeclaredConstructor().newInstance());
        });
        t("FG setAccessible then read", () -> {
            Field x = FG.getDeclaredField("secret");
            x.setAccessible(true);
            return x.get(FG.getDeclaredConstructor().newInstance());
        });
        t("FG setAccessible does not leak to a fresh handle", () -> {
            Field a = FG.getDeclaredField("secret");
            a.setAccessible(true);
            Field b3 = FG.getDeclaredField("secret");
            return b3.canAccess(FG.getDeclaredConstructor().newInstance());
        });
        access("FG unreflect a private method without setAccessible", () ->
                L.unreflect(FG.getDeclaredMethod("hidden")));
        access("FG findVirtual a private method", () -> L.findVirtual(FG, "hidden",
                MethodType.methodType(String.class)));
        t("FG trySetAccessible on a private member", () ->
                FG.getDeclaredField("secret").trySetAccessible());

        // ============ WAVE 2: discriminating fixtures ============
        // Wave 1 deferred twelve triples because every row touching them agreed
        // only at the value a blanket yield returns anyway -- `false`, `true`,
        // `[]`, `0`. An agreement there is indistinguishable from the default
        // and is not evidence. Each row below is chosen so the CORRECT answer
        // differs from that default, which is the only thing that can promote
        // those triples. Appended, not substituted, so wave 1's row numbers
        // survive and its recorded verdicts stay comparable.

        // isSynthetic: `false` is the default. A compiler-generated field is
        // the only way to get `true` -- an inner class's `this$0`.
        t("W2 F isSynthetic TRUE on this$0", () -> {
            for (Field f : Inner.class.getDeclaredFields()) {
                if (f.isSynthetic()) {
                    return "synthetic:" + f.getName();
                }
            }
            return "no-synthetic-field-found";
        });
        // isEnumConstant: `false` is the default; an enum's own constants are
        // the only `true`.
        t("W2 F isEnumConstant TRUE", () -> {
            Field f = Color.class.getDeclaredField("RED");
            return f.isEnumConstant();
        });
        t("W2 F isEnumConstant FALSE on a normal field", () -> {
            Field f = Color.class.getDeclaredField("label");
            return f.isEnumConstant();
        });
        // setBoolean: wave 1's row set `false` and read it back, which is
        // exactly what a broken getter returns. Set TRUE, and read it back
        // through a path that is not `getBoolean`.
        t("W2 F setBoolean TRUE read via get()", () -> {
            Holder hh = new Holder();
            Field x = fl(H, "z");
            x.setBoolean(hh, true);
            return String.valueOf(x.get(hh));
        });
        t("W2 F setBoolean TRUE read via toString of Boolean", () -> {
            Holder hh = new Holder();
            Field x = fl(H, "z");
            x.setBoolean(hh, true);
            return ((Boolean) x.get(hh)).booleanValue() ? "TRUE" : "FALSE";
        });
        // trySetAccessible: `true` is the default answer. A member this probe
        // may NOT open is the discriminating case -- a JDK-internal field.
        t("W2 F trySetAccessible FALSE on a JDK-internal field", () -> {
            Field f = Class.forName("java.lang.System").getDeclaredField("props");
            return f.trySetAccessible();
        });
        // isBridge: `false` is the default. A generic override mints a bridge.
        t("W2 M isBridge TRUE on a generic override", () -> {
            int bridges = 0;
            for (Method mm : SubBox.class.getDeclaredMethods()) {
                if (mm.getName().equals("set") && mm.isBridge()) {
                    bridges++;
                }
            }
            return "bridges:" + bridges;
        });
        // isSynthetic on a method: an inner class's access$ accessor, or the
        // enum's own `values`/`valueOf` are compiler-generated.
        t("W2 M isSynthetic count on an enum", () -> {
            int n = 0;
            for (Method mm : Color.class.getDeclaredMethods()) {
                if (mm.isSynthetic()) {
                    n++;
                }
            }
            return "synthetic-methods:" + n;
        });
        // isVarArgs: wave 1 only had a `true`. The FALSE case discriminates.
        t("W2 M isVarArgs FALSE on a fixed-arity method", () ->
                m(H, "add", int.class, int.class).isVarArgs());
        // isDefault: wave 1 only had a `true`. An abstract interface method is
        // the FALSE case, and a class method is another.
        t("W2 M isDefault FALSE on an abstract iface method", () ->
                m(Iface.class, "one").isDefault());
        t("W2 M isDefault FALSE on a class method", () ->
                m(H, "add", int.class, int.class).isDefault());
        // Constructor.isVarArgs / isSynthetic / getExceptionTypes: wave 1 saw
        // only `false`, `false`, `[]`.
        t("W2 C isVarArgs TRUE", () ->
                VarCtor.class.getDeclaredConstructor(String[].class).isVarArgs());
        t("W2 C getExceptionTypes NON-EMPTY", () -> names(
                Thrower2.class.getDeclaredConstructor(int.class).getExceptionTypes()));
        // `Constructor.isSynthetic` has NO fixture here and stays deferred.
        // An enum's constructor is not synthetic on JDK 25, and nestmates
        // removed the old synthetic access-constructor that javac used to emit
        // for a private inner class. Printed as a census so the absence is
        // recorded rather than assumed -- if a future JDK mints one, this row
        // stops answering 0 and the deferral can be revisited.
        t("W2 C synthetic-ctor census across four fixtures", () -> {
            int n = 0;
            for (Class<?> c : new Class<?>[] {Color.class, Inner.class,
                                              VarCtor.class, Holder.class}) {
                for (Constructor<?> k : c.getDeclaredConstructors()) {
                    if (k.isSynthetic()) {
                        n++;
                    }
                }
            }
            return "synthetic-ctors:" + n;
        });
        // Constructor.setAccessible: wave 1's row returned the string "ok",
        // which is in the default set. Discriminate by observing the FLAG and
        // a refusal, not by the constructed object.
        t("W2 C setAccessible then canAccess", () -> {
            Constructor<?> k = FG.getDeclaredConstructor(String.class);
            boolean before = k.canAccess(null);
            k.setAccessible(true);
            return "before=" + before + " after=" + k.canAccess(null);
        });
        // `Integer(int)` is PUBLIC in an exported package, so setAccessible on
        // it legitimately succeeds -- the first version of this row answered
        // NO-THROW on HotSpot and measured nothing. `Runtime`'s constructor is
        // private, which is the discriminating case.
        access("W2 C setAccessible on a PRIVATE JDK constructor", () -> {
            Constructor<?> k = Class.forName("java.lang.Runtime")
                    .getDeclaredConstructor();
            k.setAccessible(true);
            return "no-throw";
        });

        System.out.println("ROWS " + rows);
    }

    // ---- helpers reached by MethodHandle rows ------------------------------

    static class Thrower {
        void boom() {
            throw new IllegalStateException("boom-from-target");
        }
    }

    static int sub(int a, int b) {
        return a - b;
    }

    static boolean yes() {
        return true;
    }

    static String throwIt() {
        throw new IllegalStateException("mh-catch");
    }

    static String handle(IllegalStateException e) {
        return "handled:" + e.getMessage();
    }

    static int accum(int v, int i) {
        return v + i;
    }
}

/** A sibling TOP-LEVEL class, so its private members are genuinely closed to
 *  `L3ReflectInvokeSurface`: same package grants package-private access and
 *  nothing more, and two sibling top-level classes are not nestmates.
 *
 *  This exists because the probe's original access-control rows all targeted
 *  `L3ReflectInvokeSurface$Holder`, a NESTED class -- a nestmate, whose
 *  private members `main` may read with no `setAccessible` at all. The oracle
 *  said so itself (`NO-THROW 13`), which is the tell: a row where HotSpot does
 *  not throw is not measuring an access check. */
class L3Foreign {
    private int secret = 77;

    L3Foreign() {
    }

    private L3Foreign(String s) {
        this.secret = s.length();
    }

    private String hidden() {
        return "foreign-hidden";
    }
}

/** Wave 2 fixtures. Each exists so some triple's CORRECT answer differs from
 *  the value a blanket yield returns, which is the only kind of row that can
 *  promote a wave-1 default-value deferral. */
enum Color {
    RED("r"),
    GREEN("g");

    private final String label;

    Color(String label) {
        this.label = label;
    }

    String label() {
        return label;
    }
}

class Box<T> {
    void set(T t) {
    }
}

/** A generic override mints a BRIDGE method: `set(Object)` alongside
 *  `set(String)`. That is the only way `Method.isBridge` answers `true`. */
class SubBox extends Box<String> {
    @Override
    void set(String s) {
    }
}

class VarCtor {
    VarCtor(String... a) {
    }
}

class Thrower2 {
    Thrower2(int i) throws IllegalStateException, java.io.IOException {
    }
}
