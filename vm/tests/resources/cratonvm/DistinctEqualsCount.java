package cratonvm;

import java.util.stream.Stream;

/**
 * Fixture for {@code vm/tests/collection_dedup_is_hash_bucketed.rs}.
 *
 * <p>{@code Stream.distinct()} is specified by {@code HashSet}: an element is a
 * duplicate only when an earlier one has the same {@code hashCode()} AND
 * {@code equals} it. The native that shadows it used to ask {@code equals} of
 * every earlier survivor, so {@code n} distinct elements cost {@code n(n-1)/2}
 * Java calls — 1.1 s for the 1152 CLDR language tags that
 * {@code Calendar.getInstance} dedups on its first call per locale.
 *
 * <p>Every method answers an {@code int} so the Rust side needs no string
 * decoding. No lambdas: the fixture must not depend on the indy path it is
 * not about.
 *
 * <p>Compile: {@code javac -d vm/tests/resources
 * vm/tests/resources/cratonvm/DistinctEqualsCount.java} (vm/build.rs also
 * compiles it; the committed .class is the fallback when no javac is found).
 */
public final class DistinctEqualsCount {

    static int equalsCalls;

    static final class Key {
        final int id;
        final int hash;

        Key(int id, int hash) {
            this.id = id;
            this.hash = hash;
        }

        @Override
        public boolean equals(Object o) {
            equalsCalls++;
            return o instanceof Key && ((Key) o).id == id;
        }

        @Override
        public int hashCode() {
            return hash;
        }
    }

    /**
     * {@code n} pairwise-unequal keys with well-spread hashes. Answers the number
     * of {@code equals} calls {@code distinct()} made, or -1 when it kept the
     * wrong number of elements.
     */
    public static int spreadEqualsCalls(int n) {
        Key[] keys = new Key[n];
        for (int i = 0; i < n; i++) {
            keys[i] = new Key(i, i * 0x9E3779B9);
        }
        equalsCalls = 0;
        long kept = Stream.of(keys).distinct().count();
        return kept == n ? equalsCalls : -1;
    }

    /**
     * Duplicates collapse onto their FIRST occurrence, encounter order is kept,
     * and {@code null} is an element like any other. Ids are encoded one decimal
     * digit per survivor, {@code null} as 0: the JDK answers {@code 1203}.
     */
    public static int firstOccurrenceOrder() {
        Object[] kept = Stream.of(new Key(1, 7), new Key(2, 7), new Key(1, 7), null,
                new Key(3, 9), null, new Key(2, 7)).distinct().toArray();
        int code = 0;
        for (Object o : kept) {
            code = code * 10 + (o == null ? 0 : ((Key) o).id);
        }
        return code;
    }

    /**
     * Two keys that are {@code equals} but disagree on {@code hashCode} are NOT
     * duplicates to a {@code HashSet}, so {@code distinct()} keeps both: 2.
     */
    public static int equalButDifferentHash() {
        return (int) Stream.of(new Key(5, 1), new Key(5, 2)).distinct().count();
    }

    /**
     * Every key in one hash bucket: correctness must not depend on the spread.
     * {@code n} keys with ids {@code i % distinctIds} and one shared hash keep
     * exactly {@code distinctIds}.
     */
    public static int collidingHashesKept(int n, int distinctIds) {
        Key[] keys = new Key[n];
        for (int i = 0; i < n; i++) {
            keys[i] = new Key(i % distinctIds, 42);
        }
        return (int) Stream.of(keys).distinct().count();
    }

    /**
     * {@code Set.of(E...)} must reject duplicates without comparing every pair:
     * the CLDR adapter hands it all 1152 language tags. Answers the number of
     * {@code equals} calls for {@code n} pairwise-unequal, well-spread keys, or
     * -1 when the set came back the wrong size.
     */
    public static int setOfEqualsCalls(int n) {
        Key[] keys = new Key[n];
        for (int i = 0; i < n; i++) {
            keys[i] = new Key(i, i * 0x9E3779B9);
        }
        equalsCalls = 0;
        int size = java.util.Set.of(keys).size();
        return size == n ? equalsCalls : -1;
    }

    /**
     * ...and must still reject a real one. 1 when {@code Set.of} throws
     * {@code IllegalArgumentException} for two equal, same-hash keys among
     * others; 0 when it does not.
     */
    public static int setOfRejectsSameHashDuplicate() {
        try {
            java.util.Set.of(new Key(1, 3), new Key(2, 4), new Key(3, 5), new Key(2, 4), new Key(4, 6));
            return 0;
        } catch (IllegalArgumentException expected) {
            return 1;
        }
    }

    /** Strings dedup by value, not identity: {@code "a","b",new "a"} keeps 2. */
    public static int stringsByValue() {
        String a1 = new String(new char[] {'a'});
        String a2 = new String(new char[] {'a'});
        return (int) Stream.of(a1, "b", a2).distinct().count();
    }
}
