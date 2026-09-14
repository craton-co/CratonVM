import java.util.ArrayList;
import java.util.Collections;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;

/**
 * JDK-only corpus: the no-arg {@code System.getenv()} Map.
 *
 * {@code System.getenv()} is registered from
 * {@code register_essential_natives_with_shims}, so it survives strict mode and
 * runs. It then built a real {@code java.util.HashMap} and wrapped it in
 * {@code cratonvm/internal/UnmodifiableMap} — a compatibility stand-in, which
 * {@code --jdk-only} refuses. The refusal was correct; the survival of its
 * caller was the defect, and the caller is on every framework's first page:
 * Spring takes the resulting {@code NoClassDefFoundError} in
 * {@code AbstractEnvironment.<init>}, before bean one. The fix is to call the
 * real {@code java.util.Collections.unmodifiableMap} on the map the native
 * already holds.
 *
 * <p><b>Non-null is not the contract.</b> Two defects survived this year behind
 * {@code != null} and length checks, so every assertion here is an identity or
 * an equality: the wrapper's class, the four mutators, agreement between
 * {@code getenv()} and {@code getenv(String)} for <em>every</em> key, and
 * mutual consistency of the three views.
 *
 * <p><b>Determinism.</b> The environment differs between machines and between
 * the HotSpot and CratonVM arms of a comparison, so nothing derived from its
 * contents is ever printed, and no {@code check()} call sits inside a loop over
 * the environment — a per-key check would make {@code checks=N} depend on how
 * many variables the shell happened to export. Per-key failures are collected
 * and reported through one fixed check each, and the message names the offending
 * key only when it fails.
 *
 * <p><b>Deliberately out of scope:</b> whether the three <em>views</em> refuse
 * mutation ({@code keySet().remove}, {@code entrySet().iterator().remove},
 * {@code Map.Entry.setValue}). Those are a separate surface with its own
 * compatible-mode gaps (see W7-50 §6 on {@code UnmodifiableMapEntry}); mixing
 * them in here would make this vector red for a reason that is not the map.
 */
public class RJdkEnvMap {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError("RJdkEnvMap: " + m);
        }
    }

    /** A name no environment can plausibly hold. */
    static final String ABSENT = "CRATONVM_RJDKENVMAP_ABSENT_KEY";

    /** Runs {@code r}; returns true iff it threw UnsupportedOperationException. */
    static boolean refuses(Runnable r) {
        try {
            r.run();
            return false;
        } catch (UnsupportedOperationException expected) {
            return true;
        }
    }

    /**
     * The wrapper must be a real JDK class, asserted by CLASS IDENTITY against
     * one the JDK itself produces — not by a name prefix.
     *
     * <p>A prefix screen is the wrong instrument and it is worth saying why,
     * because the obvious spelling of this check is the broken one. The three
     * screens below ({@code !startsWith("cratonvm")}, no {@code /}, and
     * {@code startsWith("java.")}) catch the fabrication this bug actually was
     * — {@code cratonvm/internal/UnmodifiableMap} — and nothing else. Three of
     * the nine fabricated classes the JDK-only corpus covers are spelled INSIDE
     * JDK package namespaces
     * ({@code java/util/concurrent/atomic/…$RustJvmImpl},
     * {@code javax/net/ssl/SSLSocketOutputStream},
     * {@code java/util/concurrent/CompletedFuture}), so a {@code java.} prefix
     * reports that whole species as clean. They are kept because their failure
     * MESSAGES name the defect precisely; the identity check below is what
     * actually decides the question.
     *
     * <p>The oracle is DERIVED at run time rather than pinned to the literal
     * {@code java.util.Collections$UnmodifiableMap}: the fix is specified as
     * "call the real {@code java.util.Collections.unmodifiableMap} on the map
     * the native already holds", so the wrapper must be exactly the class that
     * call produces, whatever a given JDK spells it. A VM that mints its own
     * read-only wrapper — under any name, in any package — fails this and
     * passed every prefix screen.
     */
    static void theWrapperIsNotAFabricatedClass() {
        Map<String, String> env = System.getenv();
        check(env != null, "System.getenv() returned null");
        // Anti-vacuity: an empty map passes every consistency check below.
        check(!env.isEmpty(), "System.getenv() is empty; every check below would be vacuous");

        String name = env.getClass().getName();
        check(!name.startsWith("cratonvm"),
                "System.getenv() returned a fabricated wrapper: " + name);
        check(!name.contains("/"),
                "Class.getName() must be the binary name, not the internal form: " + name);
        check(name.startsWith("java."),
                "System.getenv()'s wrapper must be a java.* class, got: " + name);

        // The identity check the three prefix screens above cannot make.
        Class<?> jdkWrapper = Collections.unmodifiableMap(new HashMap<String, String>()).getClass();
        check(env.getClass() == jdkWrapper,
                "System.getenv()'s wrapper must BE the class java.util.Collections"
                        + ".unmodifiableMap produces, got " + name + " and not "
                        + jdkWrapper.getName());

        // The same object every time — `ProcessEnvironment` holds one static
        // `theUnmodifiableEnvironment`, and Spring's
        // StandardEnvironmentTests.getSystemEnvironment asserts it with isSameAs.
        check(System.getenv() == System.getenv(),
                "System.getenv() must return the same object on every call");
        check(env == System.getenv(), "System.getenv() identity must be stable across calls");
        System.out.println("CK RJdkEnvMap fabricatedWrapper=false stableIdentity=true"
                + " wrapperIsJdkUnmodifiableMap=" + (env.getClass() == jdkWrapper));
    }

    /**
     * Genuinely read-only, not merely named so. A wrapper whose whole contract
     * is a refusal that quietly does not refuse is the same species of defect as
     * the fabricated class it replaced.
     */
    static void theMapIsGenuinelyUnmodifiable() {
        final Map<String, String> env = System.getenv();
        final int sizeBefore = env.size();

        check(refuses(new Runnable() {
            public void run() {
                env.put(ABSENT, "x");
            }
        }), "put() must throw UnsupportedOperationException");

        check(refuses(new Runnable() {
            public void run() {
                env.remove(ABSENT);
            }
        }), "remove() must throw UnsupportedOperationException");

        check(refuses(new Runnable() {
            public void run() {
                env.clear();
            }
        }), "clear() must throw UnsupportedOperationException");

        final Map<String, String> extra = new HashMap<String, String>();
        extra.put(ABSENT, "x");
        check(refuses(new Runnable() {
            public void run() {
                env.putAll(extra);
            }
        }), "putAll() must throw UnsupportedOperationException");

        // Nothing leaked through a refusal that only *looked* like one.
        check(env.size() == sizeBefore,
                "the environment map changed size across four refused mutations");
        check(env.get(ABSENT) == null, ABSENT + " must not be present after a refused put");
        System.out.println("CK RJdkEnvMap refusedMutators=4 sizeUnchanged=true");
    }

    /**
     * The no-arg map and the single-argument lookup are two different natives
     * over one environment. {@code System.getenv(String)} was never broken, so
     * it is the oracle the map is checked against — for every key, not for a
     * sample.
     */
    static void theMapAgreesWithTheSingleArgumentLookup() {
        Map<String, String> env = System.getenv();

        List<String> mismatched = new ArrayList<String>();
        List<String> notContained = new ArrayList<String>();
        List<String> nullish = new ArrayList<String>();
        for (Map.Entry<String, String> e : env.entrySet()) {
            String k = e.getKey();
            if (k == null || e.getValue() == null) {
                nullish.add(String.valueOf(k));
                continue;
            }
            String direct = System.getenv(k);
            if (!e.getValue().equals(direct)) {
                mismatched.add(k + ": map=" + e.getValue() + " getenv(name)=" + direct);
            }
            if (!env.containsKey(k) || !env.containsValue(e.getValue())
                    || !e.getValue().equals(env.get(k))) {
                notContained.add(k);
            }
        }
        check(nullish.isEmpty(), "the environment map has null keys or values: " + nullish);
        check(mismatched.isEmpty(),
                "System.getenv() disagrees with System.getenv(name) for: " + mismatched);
        check(notContained.isEmpty(),
                "get/containsKey/containsValue disagree with entrySet for: " + notContained);

        // The negative half: a name nothing exports must be absent from both.
        check(env.get(ABSENT) == null, "get(" + ABSENT + ") must be null");
        check(!env.containsKey(ABSENT), "containsKey(" + ABSENT + ") must be false");
        check(System.getenv(ABSENT) == null, "System.getenv(" + ABSENT + ") must be null");
        System.out.println("CK RJdkEnvMap agreesWithSingleArgLookup=true absentKeyNull=true");
    }

    /**
     * {@code entrySet} / {@code keySet} / {@code values} must describe the same
     * map. A wrapper that delegates some views to the backing map and
     * synthesises others is exactly what a size-only check cannot see.
     */
    static void theThreeViewsAreSelfConsistent() {
        Map<String, String> env = System.getenv();
        int size = env.size();

        Set<String> keys = env.keySet();
        java.util.Collection<String> values = env.values();
        Set<Map.Entry<String, String>> entries = env.entrySet();

        check(keys.size() == size, "keySet().size()=" + keys.size() + " but size()=" + size);
        check(values.size() == size, "values().size()=" + values.size() + " but size()=" + size);
        check(entries.size() == size, "entrySet().size()=" + entries.size() + " but size()=" + size);

        // keySet == the entrySet's keys, as SETS (order is not part of the
        // contract; membership is).
        List<String> entryKeys = new ArrayList<String>();
        List<String> entryValues = new ArrayList<String>();
        for (Map.Entry<String, String> e : entries) {
            entryKeys.add(e.getKey());
            entryValues.add(e.getValue());
        }
        check(entryKeys.size() == size, "iterating entrySet yielded " + entryKeys.size()
                + " entries but size() is " + size);
        check(keys.containsAll(entryKeys), "keySet() is missing keys that entrySet() has");

        List<String> keyList = new ArrayList<String>(keys);
        check(keyList.size() == size, "iterating keySet() yielded " + keyList.size()
                + " keys but size() is " + size);
        check(entryKeys.containsAll(keyList), "entrySet() is missing keys that keySet() has");

        // values() is a multiset, not a set — duplicates are legal, so compare
        // sorted lists rather than sets.
        List<String> valueList = new ArrayList<String>(values);
        check(valueList.size() == size, "iterating values() yielded " + valueList.size()
                + " values but size() is " + size);
        Collections.sort(valueList);
        List<String> sortedEntryValues = new ArrayList<String>(entryValues);
        Collections.sort(sortedEntryValues);
        check(valueList.equals(sortedEntryValues),
                "values() and the entrySet's values are not the same multiset");

        // Every key resolves, through the view, to the value the entry carries.
        List<String> broken = new ArrayList<String>();
        for (String k : keyList) {
            if (!env.containsKey(k) || env.get(k) == null) {
                broken.add(k);
            }
        }
        check(broken.isEmpty(), "keySet() holds keys the map cannot resolve: " + broken);

        // Repeated view calls describe the same map (a view that re-snapshots a
        // stale backing shows up here).
        check(env.keySet().size() == size, "keySet() is unstable across calls");
        check(env.entrySet().size() == size, "entrySet() is unstable across calls");
        check(env.values().size() == size, "values() is unstable across calls");
        check(env.size() == size, "size() is unstable across calls");
        System.out.println("CK RJdkEnvMap viewsConsistent=true viewsStable=true");
    }

    public static void main(String[] args) {
        theWrapperIsNotAFabricatedClass();
        theMapIsGenuinelyUnmodifiable();
        theMapAgreesWithTheSingleArgumentLookup();
        theThreeViewsAreSelfConsistent();
        System.out.println("CK RJdkEnvMap checks=" + checks);
        System.out.println("PASS RJdkEnvMap (" + checks + " checks)");
    }
}
