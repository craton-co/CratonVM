import java.util.*;
import java.util.function.*;
import java.util.stream.*;

/** L3 — are the last unreached registrations DEAD, or is the counter lying?
 *
 *  `UtilCoverage4Sweep` calls every one of these and they still report zero
 *  invocations. Two explanations, opposite conclusions: either the counter is
 *  not incremented (a measurement bug, and the rows are fine), or a concrete
 *  receiver never reaches a registration made on its SUPERTYPE (the rows are
 *  dead weight, and a dormant registration is a trap that arms itself when
 *  something finally does produce that class -- which is how the
 *  `PriorityQueue$Itr` row cost this lane a probe).
 *
 *  The discriminator is the RECEIVER'S RUNTIME CLASS. `dispatch_virtual` probes
 *  the registry with it, so a registration on `java/util/TimeZone` is only ever
 *  reached by an object whose class IS `java.util.TimeZone`. This prints what
 *  the factories actually hand back.
 */
public class DeadDoorProbe {
    static int rows = 0;

    static void p(String tag, Object v) {
        rows++;
        System.out.println(rows + " " + tag + " |" + String.valueOf(v) + "|");
    }

    public static void main(String[] args) {
        // The registrations are on these classes; the receivers are not.
        p("TimeZone.getTimeZone class", TimeZone.getTimeZone("America/New_York").getClass().getName());
        p("TimeZone.getDefault class", TimeZone.getDefault().getClass().getName());
        p("TimeZone.getTimeZone(UTC) class", TimeZone.getTimeZone("UTC").getClass().getName());
        p("SimpleTimeZone class", new SimpleTimeZone(0, "X").getClass().getName());
        // A SimpleTimeZone IS a direct TimeZone subclass, so if the door walks
        // superclasses at all, this is where it would show.
        p("SimpleTimeZone getOffset", new SimpleTimeZone(3600000, "X").getOffset(0L));
        p("ZoneInfo getOffset", TimeZone.getTimeZone("America/New_York").getOffset(0L));

        p("HashMap class", new HashMap<String, Integer>().getClass().getName());
        p("Map-typed receiver class", ((Map<String, Integer>) new HashMap<String, Integer>())
                .getClass().getName());
        p("Collections.emptyMap class", Collections.emptyMap().getClass().getName());
        p("AbstractMap subclass forEach", new AbstractMap<String, Integer>() {
            public Set<Entry<String, Integer>> entrySet() {
                return Set.of(Map.entry("a", 1));
            }
        }.getClass().getSuperclass().getName());

        // An AbstractSet/AbstractCollection subclass that declares nothing --
        // the only receiver shape that CAN reach those registrations.
        AbstractSet<String> bare = new AbstractSet<>() {
            public Iterator<String> iterator() { return List.of("a", "b").iterator(); }
            public int size() { return 2; }
        };
        p("bare AbstractSet super", bare.getClass().getSuperclass().getName());
        p("bare AbstractSet hashCode", bare.hashCode() == ("a".hashCode() + "b".hashCode()));
        p("bare AbstractSet toArray", Arrays.toString(bare.toArray()));
        p("bare AbstractSet stream", bare.stream().count());
        p("bare AbstractSet toArray(IntFunction)",
                Arrays.toString(bare.toArray(String[]::new)));

        AbstractCollection<String> bag = new AbstractCollection<>() {
            public Iterator<String> iterator() { return List.of("a").iterator(); }
            public int size() { return 1; }
        };
        p("bare AbstractCollection toArray", Arrays.toString(bag.toArray()));
        p("bare AbstractCollection stream", bag.stream().count());

        // A Map implementation that is NOT an AbstractMap/HashMap, so the
        // interface default is the only body there is.
        Map<String, Integer> plain = new PlainMap();
        StringBuilder sb = new StringBuilder();
        plain.forEach((k, v) -> sb.append(k).append('=').append(v).append(','));
        p("plain Map forEach", sb.toString());
        p("plain Map class", plain.getClass().getName());

        // SequencedMap's interface defaults, on a SequencedMap that inherits
        // them rather than overriding.
        p("LinkedHashMap is SequencedMap", new LinkedHashMap<>() instanceof SequencedMap);

        // ---- THE SECOND DOOR. `dispatch_virtual` probes the registry with the
        // RECEIVER's runtime class, which is why nothing above reaches a
        // registration made on an abstract class or an interface. But it is not
        // the only door: `build_lambda_impl_cached` and the stackless path
        // probe with the DECLARING class, and a bound method reference goes
        // through those. If these registrations are reachable at all, this is
        // where they fire -- and "unreachable by one door" is not "dead".
        Consumer<BiConsumer<String, Integer>> mapForEach = plain::forEach;
        StringBuilder rb = new StringBuilder();
        mapForEach.accept((k, v) -> rb.append(k).append('=').append(v).append(','));
        p("Map::forEach methodref", rb.toString());

        Supplier<Stream<String>> setStream = bare::stream;
        p("Collection::stream methodref", setStream.get().count());

        IntSupplier setHash = bare::hashCode;
        p("AbstractSet::hashCode methodref", setHash.getAsInt() == ("a".hashCode() + "b".hashCode()));

        Supplier<Object[]> collToArray = bag::toArray;
        p("AbstractCollection::toArray methodref", Arrays.toString(collToArray.get()));

        IntFunction<String[]> gen = String[]::new;
        Function<IntFunction<String[]>, String[]> collGen = bare::toArray;
        p("Collection::toArray(IntFunction) methodref", Arrays.toString(collGen.apply(gen)));

        TimeZone ny2 = TimeZone.getTimeZone("America/New_York");
        LongToIntFunction zoneOffset = ny2::getOffset;
        p("TimeZone::getOffset methodref", zoneOffset.applyAsInt(0L));
        Supplier<String> zoneName = ny2::getDisplayName;
        p("TimeZone::getDisplayName methodref", zoneName.get());

        LinkedHashMap<String, Integer> sq2 = new LinkedHashMap<>();
        sq2.put("a", 1); sq2.put("b", 2);
        SequencedMap<String, Integer> asSeq = sq2;
        Supplier<Map.Entry<String, Integer>> poll = asSeq::pollFirstEntry;
        p("SequencedMap::pollFirstEntry methodref", String.valueOf(poll.get()) + " " + sq2);

        // A Spliterator reached through the interface, whose forEachRemaining
        // has its own registration.
        Spliterator<String> sp = bare.spliterator();
        StringBuilder spb = new StringBuilder();
        sp.forEachRemaining(x -> spb.append(x).append(','));
        p("Spliterator.forEachRemaining", spb.toString());

        System.out.println("DONE DeadDoorProbe");
    }

    /** A hand-rolled Map so `Map`'s own default methods are the ones that run. */
    static final class PlainMap extends java.util.AbstractMap<String, Integer> {
        public Set<Entry<String, Integer>> entrySet() {
            LinkedHashSet<Entry<String, Integer>> s = new LinkedHashSet<>();
            s.add(new AbstractMap.SimpleEntry<>("a", 1));
            s.add(new AbstractMap.SimpleEntry<>("b", 2));
            return s;
        }
    }
}
