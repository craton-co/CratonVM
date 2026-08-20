import java.io.Serializable;
import java.util.AbstractMap;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collection;
import java.util.Collections;
import java.util.HashMap;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.TreeMap;
import java.util.TreeSet;

/**
 * The subtype question for the immutable / unmodifiable collection factories:
 * {@code Map.of}, {@code List.of}, {@code Set.of}, {@code Collections.*}.
 *
 * <p><b>Why this file exists.</b> {@code Map} is not a {@code Collection} in
 * Java, and {@code Map.of("k","v") instanceof Collection} must be
 * {@code false}. Under {@code --jdk-only} CratonVM answers {@code true}, and
 * the {@code (Collection) Map.of(...)} cast that HotSpot refuses is let
 * through — so the failure surfaces later, at an unrelated call site, as
 * {@code NoSuchMethodError: ImmutableCollections$Map1.iterator()}. That is the
 * shape a wrong {@code checkcast} always has: the damage is not where the
 * decision was made. It was found through two Spring {@code ObjectUtilsTests}
 * assertions ({@code nullSafeConciseToString} branches on
 * {@code instanceof Collection} <em>before</em> {@code instanceof Map}) and
 * nothing in this suite covered it.
 *
 * <p><b>Both modes are wrong, in opposite directions.</b> {@code --jdk-only}
 * over-accepts {@code Collection}/{@code Iterable}; the default
 * {@code --real-jdk} under-accepts the {@code AbstractMap} superclass that
 * {@code getSuperclass()} itself reports. So the rows are asserted for both.
 *
 * <p><b>Reflection is read beside every opcode, deliberately.</b>
 * {@code Class.isInstance} / {@code isAssignableFrom} and the
 * {@code instanceof} / {@code checkcast} opcodes are two implementations of one
 * JVMS rule, and in this VM they disagree: the reflective pair is right and the
 * opcodes are not. Reading only one of them is how this stayed hidden.
 *
 * <p>Every expectation below was taken from Temurin 25.0.3.9 with
 * {@code CollTruth.java} before it was written here; none is recalled.
 *
 * <p><b>Expect this fixture to be RED until two fixes land.</b> It uses
 * {@code expect}/{@code drain}: a divergence is recorded and printed rather
 * than thrown at, so one run reports every failing row instead of one row per
 * rebuild of the VM.
 */
public class RImmutableFactoryTypes {
    static int checks;
    static final List<String> DIVERGENCES = new ArrayList<>();

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    /** Counted like {@link #check}, but records instead of throwing. */
    static void expect(boolean c, String m) {
        checks++;
        if (!c) {
            DIVERGENCES.add(m);
            System.out.println("DIVERGENCE RImmutableFactoryTypes " + m);
        }
    }

    static String castResult(Object o, int which) {
        try {
            Object x;
            switch (which) {
                case 0: x = (Collection<?>) o; break;
                case 1: x = (Iterable<?>) o; break;
                default: x = (Map<?, ?>) o; break;
            }
            return x == null ? "null" : "OK";
        } catch (ClassCastException e) {
            return "CCE";
        }
    }

    /**
     * One receiver, six answers. {@code isCollection} drives Collection,
     * Iterable and the two casts; {@code isMap} drives Map.
     */
    static void row(String name, Object o, boolean isCollection, boolean isMap) {
        expect((o instanceof Collection) == isCollection,
                name + ": instanceof Collection must be " + isCollection
                        + " (got " + (o instanceof Collection) + ")");
        expect((o instanceof Iterable) == isCollection,
                name + ": instanceof Iterable must be " + isCollection
                        + " (got " + (o instanceof Iterable) + ")");
        expect((o instanceof Map) == isMap,
                name + ": instanceof Map must be " + isMap
                        + " (got " + (o instanceof Map) + ")");
        // The checkcast opcode, read separately from instanceof: they are
        // separate opcodes and the JIT lowers them separately.
        expect(castResult(o, 0).equals(isCollection ? "OK" : "CCE"),
                name + ": (Collection) cast must be " + (isCollection ? "OK" : "CCE")
                        + " (got " + castResult(o, 0) + ")");
        expect(castResult(o, 1).equals(isCollection ? "OK" : "CCE"),
                name + ": (Iterable) cast must be " + (isCollection ? "OK" : "CCE")
                        + " (got " + castResult(o, 1) + ")");
        expect(castResult(o, 2).equals(isMap ? "OK" : "CCE"),
                name + ": (Map) cast must be " + (isMap ? "OK" : "CCE")
                        + " (got " + castResult(o, 2) + ")");
        // The reflective twins. These are the control: if THESE go red the
        // defect is somewhere else entirely.
        check(Collection.class.isInstance(o) == isCollection,
                name + ": Collection.class.isInstance");
        check(Collection.class.isAssignableFrom(o.getClass()) == isCollection,
                name + ": Collection.class.isAssignableFrom");
        check(Map.class.isInstance(o) == isMap, name + ": Map.class.isInstance");
    }

    static void maps() {
        // A Map is NEVER a Collection. Measured on HotSpot 25: every row false.
        row("Map.of()", Map.of(), false, true);
        row("Map.of(k,v)", Map.of("k", "v"), false, true);
        row("Map.of(k,v,k2,v2)", Map.of("k", "v", "k2", "v2"), false, true);
        row("Map.copyOf", Map.copyOf(new HashMap<String, String>()), false, true);
        row("Collections.emptyMap", Collections.emptyMap(), false, true);
        row("Collections.singletonMap", Collections.singletonMap("k", "v"), false, true);
        row("Collections.unmodifiableMap",
                Collections.unmodifiableMap(new HashMap<String, String>()), false, true);
        row("new HashMap", new HashMap<String, String>(), false, true);
        row("new TreeMap", new TreeMap<String, String>(), false, true);
        // `Map.entry` is neither: `java.util.KeyValueHolder` implements
        // `Map.Entry` only.
        row("Map.entry", Map.entry("k", "v"), false, false);
    }

    static void mapViews() {
        // A Map's VIEWS are Collections. This is the other half of the rule and
        // the half a blanket "collection-ish builtin" answer gets right by
        // accident, so it is asserted separately.
        Map<String, String> m = Map.of("k", "v");
        row("Map.of().values()", m.values(), true, false);
        row("Map.of().keySet()", m.keySet(), true, false);
        row("Map.of().entrySet()", m.entrySet(), true, false);
        check(m.keySet() instanceof Set, "keySet is a Set");
        check(m.entrySet() instanceof Set, "entrySet is a Set");
        check(!(m.values() instanceof Set), "values() is NOT a Set");
        check(!(m.values() instanceof List), "values() is NOT a List");
    }

    static void collections() {
        row("List.of()", List.of(), true, false);
        row("List.of(x)", List.of("x"), true, false);
        row("Set.of(x)", Set.of("x"), true, false);
        row("Arrays.asList", Arrays.asList("x"), true, false);
        row("Collections.emptyList", Collections.emptyList(), true, false);
        row("Collections.unmodifiableList",
                Collections.unmodifiableList(new ArrayList<String>()), true, false);
        row("Collections.unmodifiableSet",
                Collections.unmodifiableSet(new HashSet<String>()), true, false);
        row("Collections.unmodifiableSortedSet",
                Collections.unmodifiableSortedSet(new TreeSet<String>()), true, false);
        row("Collections.unmodifiableNavigableSet",
                Collections.unmodifiableNavigableSet(new TreeSet<String>()), true, false);
        row("new ArrayList", new ArrayList<String>(), true, false);

        // A List is not a Set and a Set is not a List — the same blanket that
        // makes a Map a Collection once made a HashSet a List.
        check(!(List.of("x") instanceof Set), "List.of is not a Set");
        check(!(Set.of("x") instanceof List), "Set.of is not a List");
    }

    /** The class the opcodes are answering ABOUT, and its declared supertypes. */
    static void hierarchy() {
        Object m1 = Map.of("k", "v");
        String n = m1.getClass().getName();
        System.out.println("CK RImmutableFactoryTypes Map.of(k,v).getClass()=" + n);
        // `--real-jdk` answers false here while reporting a superclass chain
        // through AbstractMap — no aliasing story explains a missing link in a
        // chain `getSuperclass()` itself reports, which is how the receiver was
        // shown not to be a genuine `ImmutableCollections$Map1`.
        expect(m1 instanceof AbstractMap,
                "Map.of(k,v) must be instanceof AbstractMap (got "
                        + (m1 instanceof AbstractMap) + "); getClass()=" + n);
        check(m1 instanceof Serializable, "Map.of is Serializable");
        check(!(m1 instanceof Runnable), "Map.of is not Runnable");
        check(!(m1 instanceof CharSequence), "Map.of is not a CharSequence");
    }

    /**
     * The consequence, not the predicate. A wrong {@code checkcast} does not
     * fail where the decision was made.
     */
    static void consequence() {
        boolean cce = false;
        String later = null;
        try {
            Collection<?> c = (Collection<?>) (Object) Map.of("k", "v");
            later = String.valueOf(c.iterator());
        } catch (ClassCastException e) {
            cce = true;
        } catch (Throwable t) {
            later = t.getClass().getName() + ": " + t.getMessage();
        }
        expect(cce, "(Collection) Map.of(k,v) must throw ClassCastException at the "
                + "cast; instead the cast succeeded and the next call gave: " + later);
        checks++;
    }

    static void drain() {
        if (!DIVERGENCES.isEmpty()) {
            throw new AssertionError(DIVERGENCES.size()
                    + " divergence(s) from HotSpot 25: " + DIVERGENCES);
        }
    }

    public static void main(String[] args) {
        maps();
        mapViews();
        collections();
        hierarchy();
        consequence();
        System.out.println("CK RImmutableFactoryTypes checks=" + checks);
        drain();
        System.out.println("PASS RImmutableFactoryTypes (" + checks + " checks)");
    }
}
