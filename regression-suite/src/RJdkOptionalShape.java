import java.lang.constant.ClassDesc;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.time.Duration;
import java.util.Optional;
import java.util.OptionalDouble;
import java.util.OptionalInt;
import java.util.OptionalLong;
import java.util.stream.Collectors;
import java.util.stream.DoubleStream;
import java.util.stream.IntStream;
import java.util.stream.LongStream;
import java.util.stream.Stream;

/**
 * {@code java.util.Optional}'s LAYOUT, and the nine natives that write an {@code int} presence flag
 * into the slot the real class uses for {@code value}.
 *
 * <h2>The defect this file is the executable form of</h2>
 *
 * <p>Diagnosed in
 * docs/known-issues/jdk-only/C12-3-optional-value-slot-holds-an-int-flag.md. The real class has
 * exactly one instance field and it is a REFERENCE (javap, JDK 25):
 *
 * <pre>
 *   private final T value;
 *   public boolean isPresent();   // aload_0; getfield value; ifnull -&gt; false
 *   public T get();               // aload_0; getfield value; ifnonnull -&gt; return it,
 *                                 //          else throw NoSuchElementException
 * </pre>
 *
 * <p>Nine sites in {@code native-builtins/src/http2.rs} model an {@code Optional} as
 * <i>(flag, payload)</i>: slot 0 gets {@code Value::Int(present ? 1 : 0)} and slot 1 gets the real
 * value, which no real bytecode ever reads. The over-allocation is invisible (the allocator takes
 * {@code num_fields.max(real)}, so the GC bounds guard is satisfied and nothing crashes), and the
 * interpreter's shared null test
 * ({@code ref_operand_is_null}, {@code vm/src/runtime/interpreter.rs}) counts
 * {@code Object(None)}, {@code Uninitialized} and {@code Long(0)} as null -- and <b>nothing
 * else</b>. {@code Int(0)} is not null. So:
 *
 * <ul>
 *   <li>an EMPTY {@code Optional} answers {@code isPresent() == true};
 *   <li>{@code get()} returns the FLAG rather than the value, or rather than throwing;
 *   <li>{@code orElse}, {@code orElseGet}, {@code ifPresent}, {@code map}, {@code filter},
 *       {@code flatMap} and {@code stream} all inherit both errors, because every one of them is
 *       bytecode over the same field.
 * </ul>
 *
 * <p><b>The flag-plus-payload idiom is not wrong in general -- it is wrong for THIS class.</b>
 * {@code OptionalInt}, {@code OptionalLong} and {@code OptionalDouble} really do declare
 * {@code private final boolean isPresent} at slot 0 and {@code value} at slot 1 (javap). The
 * natives applied the PRIMITIVE Optional's layout to the REFERENCE one. That is why
 * {@link #prim()} exists beside {@link #core()}: the two shapes must be asked separately, and a
 * fixture that only exercised {@code OptionalInt} would read green on exactly the layout that is
 * broken.
 *
 * <h2>REACHING THE DEFECT -- read this before trusting a green run</h2>
 *
 * <p>The defect is in Optionals MINTED BY THE VM. An {@code Optional} this file constructs in Java
 * is built by the real {@code java.util.Optional} bytecode and cannot exhibit it. {@link #core()}
 * and {@link #prim()} are therefore CONTRACT COVERAGE and a NEGATIVE CONTROL -- they say the class
 * library works -- and they are not the rows that catch C12-3.
 *
 * <p>The rows that reach the diagnosed natives are in {@link #httpmint()}, and they need no network
 * and no TLS peer: {@code HttpClient.connectTimeout()}, {@code HttpRequest.timeout()},
 * {@code HttpRequest.version()} and {@code HttpRequest.bodyPublisher()} are four of the nine sites
 * in C12-3's table, and all four answer off a builder. Each is asked twice -- once absent (which
 * catches the inverted {@code isPresent()}) and once with a value the test itself chose (which
 * catches {@code get()} returning the flag: a {@code Duration} of exactly 7000 ms, an
 * {@code HttpClient.Version} whose {@code name()} is {@code HTTP_2}, and a body publisher whose
 * {@code contentLength()} is 2). The remaining sites -- {@code sslSession()},
 * {@code previousResponse()} and the {@code HttpResponse} half of {@code version()} -- need a real
 * response and are therefore NOT reachable here; that gap is stated in the record rather than
 * papered over.
 *
 * <p>{@link #stream()}, {@link #version()}, {@link #process()} and {@link #misc()} exercise other
 * JDK surfaces that RETURN an {@code Optional} and are candidates to be VM-implemented on this VM
 * (stream terminal operations, {@code Runtime.Version} accessors, {@code ProcessHandle} and its
 * {@code Info}, {@code ModuleDescriptor}, {@code describeConstable}, {@code StackWalker}). Whether
 * any given one of them is minted Rust-side is not established by this file, and a comment beside
 * each says which route is expected to.
 *
 * <h2>Why the checks are LAWS rather than expected values</h2>
 *
 * <p>Most of the interesting routes are host-dependent in their PRESENCE ({@code info().command()}
 * is present on some platforms and not others) but not in their CONTRACT. {@link #laws} asserts the
 * twenty-one things that are true of every {@code Optional} whatever it holds, in a fixed number of
 * checks so the published count does not move with the host, and the sharpest of them is the type
 * check: a present {@code Optional} must {@code get()} an instance of its DECLARED element type. A
 * flag of {@code 1} is not a {@code Duration}, not an {@code Instant} and not a
 * {@code ProcessHandle}, so the type law fires on every present row regardless of what the value
 * would have been. The deterministic-value rows are asserted on top of the laws, never instead of
 * them.
 *
 * <h2>Mode independence</h2>
 *
 * <p>{@code http2.rs} is registered in both arms and {@code java.util.Optional} is a real JDK class
 * either way, so this is a default-mode compatibility concern and belongs in {@code CORE_CLASSES},
 * not in the {@code RJdk*} policy corpus -- the same reasoning {@code run.sh} already records for
 * {@code RJdkViews}. The {@code RJdk} prefix here is a naming convention only.
 */
public class RJdkOptionalShape {
    static int checks;

    static int mark;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    /**
     * Close a block and assert its own size. A block that silently loses rows to an edit still
     * prints {@code CK}, and a hard-coded number nobody re-derives is how a shrinking vector goes
     * unnoticed. Mismatch is a failure, not a warning.
     */
    static void sectionEnd(String name, int expected) {
        int n = checks - mark;
        mark = checks;
        if (n != expected) {
            throw new AssertionError(
                    "block " + name + " ran " + n + " checks, header says " + expected);
        }
        System.out.println("CK RJdkOptionalShape " + name + "=" + n);
    }

    /** An observable on a line the harness's extract() keeps, so it reaches the cross-VM diff. */
    static void ob(String key, int v) {
        System.out.println("CK RJdkOptionalShape " + key + "=" + v);
    }

    /** As {@link #ob(String, int)}, for a name. Never a raw value: those are host-dependent. */
    static void obName(String key, String v) {
        System.out.println("CK RJdkOptionalShape " + key + "=" + v);
    }

    /**
     * THE SEAM. Every {@code Optional} this file examines passes through here, and the mutation
     * check replaces this body with the defect's own shape -- an {@code Optional} whose value slot
     * holds the {@code int} presence flag, i.e. {@code Optional.of(present ? 1 : 0)}, which is
     * exactly what a real {@code java.util.Optional} looks like once a native has written
     * {@code Value::Int} into slot 0 and {@code ref_operand_is_null} has declined to call it null.
     * One seam, so the mutant differs from the fixture by one line per Optional flavour.
     */
    static <T> Optional<T> opt(Optional<T> o) {
        return o;
    }

    static OptionalInt optI(OptionalInt o) {
        return o;
    }

    static OptionalLong optL(OptionalLong o) {
        return o;
    }

    static OptionalDouble optD(OptionalDouble o) {
        return o;
    }

    /** The class of the throwable {@code op} produced, or {@code "none"}. */
    static String nameOf(Throwable t) {
        return t == null ? "none" : t.getClass().getName();
    }

    /**
     * A value's CLASS, never the value. Process command lines, user names and file paths are not
     * ASCII on every host, and an assertion message that carries one depends on console encoding.
     */
    static String describe(Object o) {
        return o == null ? "null" : o.getClass().getName();
    }

    static final Object SENTINEL = new Object();
    static final Object MARK = new Object();
    static final Object ALT = new Object();
    static final Optional<Object> ALT_OPT = Optional.of(ALT);

    /** How many checks {@link #laws} runs. Fixed, so a block's total does not move with the host. */
    static final int LAWS = 21;

    /**
     * The twenty-one things that are true of EVERY {@code java.util.Optional}, present or absent.
     *
     * <p>Fixed count by construction: each law is one {@code check}, and the present/absent arms
     * assert DIFFERENT propositions rather than different NUMBERS of them. That matters because
     * several of the routes this file drives are present on one platform and absent on another, and
     * a block whose size depended on that would make the published check count a host fact.
     *
     * @param type the DECLARED element type of the route that produced {@code o}. This is the law
     *     that catches C12-3 on a present Optional: the flag is an {@code int}, and an {@code int}
     *     is not a {@code Duration}.
     */
    static void laws(String site, Optional<?> o, Class<?> type) {
        check(o != null, site + ": the route returned null instead of an Optional");
        @SuppressWarnings("unchecked")
        Optional<Object> oo = (Optional<Object>) o;
        check("java.util.Optional".equals(oo.getClass().getName()),
                site + ": must be a java.util.Optional, got " + describe(oo));
        boolean present = oo.isPresent();
        check(present != oo.isEmpty(),
                site + ": isPresent() and isEmpty() must disagree; both said " + present);

        Object got = null;
        Throwable getEx = null;
        try {
            got = oo.get();
        } catch (Throwable t) {
            getEx = t;
        }
        if (present) {
            check(got != null && type.isInstance(got),
                    site + ": get() on a PRESENT Optional must return a " + type.getName()
                            + ", got " + describe(got) + " (threw " + nameOf(getEx) + ") -- an int"
                            + " presence flag in the value slot is what this looks like");
        } else {
            check("java.util.NoSuchElementException".equals(nameOf(getEx)),
                    site + ": get() on an EMPTY Optional must throw NoSuchElementException, it"
                            + " returned " + describe(got) + " / threw " + nameOf(getEx));
        }

        Object orElse = oo.orElse(SENTINEL);
        check(present ? orElse == got : orElse == SENTINEL,
                site + ": orElse must yield the value when present and the argument when absent;"
                        + " present=" + present + ", got " + describe(orElse));
        Object orElseGet = oo.orElseGet(() -> SENTINEL);
        check(present ? orElseGet == got : orElseGet == SENTINEL,
                site + ": orElseGet must yield the value when present and the supplied default when"
                        + " absent; present=" + present + ", got " + describe(orElseGet));

        Object thrown = null;
        Throwable oteEx = null;
        try {
            thrown = oo.orElseThrow();
        } catch (Throwable t) {
            oteEx = t;
        }
        check(present ? thrown == got
                        : "java.util.NoSuchElementException".equals(nameOf(oteEx)),
                site + ": orElseThrow() must agree with get(); present=" + present + ", got "
                        + describe(thrown) + " / threw " + nameOf(oteEx));
        Object supplied = null;
        Throwable supEx = null;
        try {
            supplied = oo.orElseThrow(() -> new IllegalStateException("absent"));
        } catch (Throwable t) {
            supEx = t;
        }
        check(present ? supplied == got
                        : "java.lang.IllegalStateException".equals(nameOf(supEx)),
                site + ": orElseThrow(supplier) must throw the SUPPLIED exception when absent;"
                        + " present=" + present + ", threw " + nameOf(supEx));

        int[] ran = new int[4];
        Optional<Object> mapped = oo.map(x -> {
            ran[0]++;
            return MARK;
        });
        check(mapped.isPresent() == present && (!present || mapped.get() == MARK),
                site + ": map must be empty when absent and hold the mapped value when present");
        check(ran[0] == (present ? 1 : 0),
                site + ": map's function must run exactly " + (present ? 1 : 0) + " time(s), ran "
                        + ran[0] + " -- an empty Optional that runs the function is reporting"
                        + " itself present");

        Optional<Object> flat = oo.flatMap(x -> {
            ran[1]++;
            return Optional.of(MARK);
        });
        check(flat.isPresent() == present && (!present || flat.get() == MARK)
                        && ran[1] == (present ? 1 : 0),
                site + ": flatMap must not run its function on an empty Optional; ran " + ran[1]);

        Optional<Object> never = oo.filter(x -> {
            ran[2]++;
            return false;
        });
        check(!never.isPresent() && ran[2] == (present ? 1 : 0),
                site + ": filter(false) must be empty and must run the predicate exactly "
                        + (present ? 1 : 0) + " time(s), ran " + ran[2]);
        Optional<Object> always = oo.filter(x -> true);
        check(always.isPresent() == present,
                site + ": filter(true) must preserve presence");

        oo.ifPresent(x -> ran[3]++);
        check(ran[3] == (present ? 1 : 0),
                site + ": ifPresent must invoke the consumer exactly " + (present ? 1 : 0)
                        + " time(s), invoked " + ran[3]);
        int[] both = new int[2];
        oo.ifPresentOrElse(x -> both[0]++, () -> both[1]++);
        check(both[0] == (present ? 1 : 0) && both[1] == (present ? 0 : 1),
                site + ": ifPresentOrElse must take exactly one arm; took present=" + both[0]
                        + " absent=" + both[1]);

        check(oo.equals(Optional.empty()) == !present,
                site + ": equals(Optional.empty()) must be " + !present);
        check(oo.equals(oo) && !oo.equals(null) && !oo.equals(SENTINEL),
                site + ": equals must be reflexive, must reject null, and must reject a bare value");
        check(oo.hashCode() == (present ? got.hashCode() : 0),
                site + ": hashCode must be the value's hash when present and 0 when absent, got "
                        + oo.hashCode());
        check(oo.toString().equals(present ? "Optional[" + got + "]" : "Optional.empty"),
                site + ": toString must be \"Optional.empty\" or \"Optional[<value>]\"");
        check(oo.stream().count() == (present ? 1L : 0L),
                site + ": stream() must yield exactly " + (present ? 1 : 0) + " element(s), yielded "
                        + oo.stream().count());
        Optional<Object> ored = oo.or(() -> ALT_OPT);
        check(present ? ored.get() == got : ored.get() == ALT,
                site + ": or() must keep the value when present and take the alternative when"
                        + " absent");
    }

    static final int LAWS_I = 13;

    /** {@link #laws} for {@code OptionalInt}, whose layout is genuinely (isPresent, value). */
    static void lawsInt(String site, OptionalInt o, boolean wantPresent) {
        check("java.util.OptionalInt".equals(o.getClass().getName()),
                site + ": must be a java.util.OptionalInt, got " + describe(o));
        boolean present = o.isPresent();
        check(present == wantPresent, site + ": isPresent() must be " + wantPresent);
        check(present != o.isEmpty(), site + ": isPresent() and isEmpty() must disagree");
        int got = 0;
        Throwable ex = null;
        try {
            got = o.getAsInt();
        } catch (Throwable t) {
            ex = t;
        }
        check(present ? ex == null : "java.util.NoSuchElementException".equals(nameOf(ex)),
                site + ": getAsInt() must throw NoSuchElementException when empty, threw "
                        + nameOf(ex));
        check(o.orElse(-7) == (present ? got : -7), site + ": orElse must respect presence");
        check(o.orElseGet(() -> -7) == (present ? got : -7),
                site + ": orElseGet must respect presence");
        Throwable ote = null;
        try {
            o.orElseThrow();
        } catch (Throwable t) {
            ote = t;
        }
        check(present ? ote == null : "java.util.NoSuchElementException".equals(nameOf(ote)),
                site + ": orElseThrow() must agree with getAsInt()");
        Throwable sup = null;
        try {
            o.orElseThrow(() -> new IllegalStateException("absent"));
        } catch (Throwable t) {
            sup = t;
        }
        check(present ? sup == null : "java.lang.IllegalStateException".equals(nameOf(sup)),
                site + ": orElseThrow(supplier) must throw the supplied exception when empty");
        int[] ran = new int[1];
        o.ifPresent(x -> ran[0]++);
        check(ran[0] == (present ? 1 : 0), site + ": ifPresent must respect presence");
        check(o.equals(OptionalInt.empty()) == !present,
                site + ": equals(OptionalInt.empty()) must be " + !present);
        check(o.hashCode() == (present ? Integer.hashCode(got) : 0),
                site + ": hashCode must be Integer.hashCode(value) when present and 0 when empty");
        check(o.toString().equals(present ? "OptionalInt[" + got + "]" : "OptionalInt.empty"),
                site + ": toString must be \"OptionalInt.empty\" or \"OptionalInt[<value>]\"");
        check(o.stream().count() == (present ? 1L : 0L), site + ": stream() must respect presence");
    }

    static final int LAWS_L = 12;

    static void lawsLong(String site, OptionalLong o, boolean wantPresent) {
        check("java.util.OptionalLong".equals(o.getClass().getName()),
                site + ": must be a java.util.OptionalLong, got " + describe(o));
        boolean present = o.isPresent();
        check(present == wantPresent, site + ": isPresent() must be " + wantPresent);
        check(present != o.isEmpty(), site + ": isPresent() and isEmpty() must disagree");
        long got = 0L;
        Throwable ex = null;
        try {
            got = o.getAsLong();
        } catch (Throwable t) {
            ex = t;
        }
        check(present ? ex == null : "java.util.NoSuchElementException".equals(nameOf(ex)),
                site + ": getAsLong() must throw NoSuchElementException when empty, threw "
                        + nameOf(ex));
        check(o.orElse(-7L) == (present ? got : -7L), site + ": orElse must respect presence");
        check(o.orElseGet(() -> -7L) == (present ? got : -7L),
                site + ": orElseGet must respect presence");
        Throwable ote = null;
        try {
            o.orElseThrow();
        } catch (Throwable t) {
            ote = t;
        }
        check(present ? ote == null : "java.util.NoSuchElementException".equals(nameOf(ote)),
                site + ": orElseThrow() must agree with getAsLong()");
        int[] ran = new int[1];
        o.ifPresent(x -> ran[0]++);
        check(ran[0] == (present ? 1 : 0), site + ": ifPresent must respect presence");
        check(o.equals(OptionalLong.empty()) == !present,
                site + ": equals(OptionalLong.empty()) must be " + !present);
        check(o.hashCode() == (present ? Long.hashCode(got) : 0),
                site + ": hashCode must be Long.hashCode(value) when present and 0 when empty");
        check(o.toString().equals(present ? "OptionalLong[" + got + "]" : "OptionalLong.empty"),
                site + ": toString must be \"OptionalLong.empty\" or \"OptionalLong[<value>]\"");
        check(o.stream().count() == (present ? 1L : 0L), site + ": stream() must respect presence");
    }

    static final int LAWS_D = 12;

    static void lawsDouble(String site, OptionalDouble o, boolean wantPresent) {
        check("java.util.OptionalDouble".equals(o.getClass().getName()),
                site + ": must be a java.util.OptionalDouble, got " + describe(o));
        boolean present = o.isPresent();
        check(present == wantPresent, site + ": isPresent() must be " + wantPresent);
        check(present != o.isEmpty(), site + ": isPresent() and isEmpty() must disagree");
        double got = 0.0;
        Throwable ex = null;
        try {
            got = o.getAsDouble();
        } catch (Throwable t) {
            ex = t;
        }
        check(present ? ex == null : "java.util.NoSuchElementException".equals(nameOf(ex)),
                site + ": getAsDouble() must throw NoSuchElementException when empty, threw "
                        + nameOf(ex));
        // Compared by raw bits, never by ==: -0.0 == 0.0 is true and NaN != NaN, so an
        // equality-shaped check passes against exactly the defects this file hunts.
        check(Double.doubleToRawLongBits(o.orElse(-7.0))
                        == Double.doubleToRawLongBits(present ? got : -7.0),
                site + ": orElse must respect presence");
        check(Double.doubleToRawLongBits(o.orElseGet(() -> -7.0))
                        == Double.doubleToRawLongBits(present ? got : -7.0),
                site + ": orElseGet must respect presence");
        Throwable ote = null;
        try {
            o.orElseThrow();
        } catch (Throwable t) {
            ote = t;
        }
        check(present ? ote == null : "java.util.NoSuchElementException".equals(nameOf(ote)),
                site + ": orElseThrow() must agree with getAsDouble()");
        int[] ran = new int[1];
        o.ifPresent(x -> ran[0]++);
        check(ran[0] == (present ? 1 : 0), site + ": ifPresent must respect presence");
        check(o.equals(OptionalDouble.empty()) == !present,
                site + ": equals(OptionalDouble.empty()) must be " + !present);
        check(o.hashCode() == (present ? Double.hashCode(got) : 0),
                site + ": hashCode must be Double.hashCode(value) when present and 0 when empty");
        check(o.toString().equals(present ? "OptionalDouble[" + got + "]" : "OptionalDouble.empty"),
                site + ": toString must be \"OptionalDouble.empty\" or \"OptionalDouble[<v>]\"");
        check(o.stream().count() == (present ? 1L : 0L), site + ": stream() must respect presence");
    }

    // -----------------------------------------------------------------------
    // 1. core -- the full contract on ORDINARY java.util.Optional values.
    //
    // NEGATIVE CONTROL as well as coverage: these Optionals are built by the
    // real class's own bytecode, so C12-3 cannot reach them. If this block is
    // red, the defect is in java.util.Optional itself or in the interpreter,
    // not in the nine natives -- and the httpmint block below would then be
    // reporting a symptom of something else.
    // -----------------------------------------------------------------------
    static void core() {
        laws("core.empty", opt(Optional.<String>empty()), String.class);
        laws("core.of", opt(Optional.of("v")), String.class);
        laws("core.ofNullable-null", opt(Optional.<String>ofNullable(null)), String.class);
        laws("core.ofNullable-value", opt(Optional.ofNullable("v")), String.class);
        laws("core.of-integer", opt(Optional.of(Integer.valueOf(0))), Integer.class);

        // Optional.of(null) is the one factory that must reject its argument.
        Throwable t = null;
        try {
            Optional.of(null);
        } catch (Throwable x) {
            t = x;
        }
        check("java.lang.NullPointerException".equals(nameOf(t)),
                "Optional.of(null) must throw NullPointerException, got " + nameOf(t));

        // The exact exception, message included: a VM that fabricates a throwable of the right
        // class with no message is a different answer from the JDK's.
        t = null;
        try {
            Optional.empty().get();
        } catch (Throwable x) {
            t = x;
        }
        check("java.util.NoSuchElementException".equals(nameOf(t)),
                "Optional.empty().get() must throw NoSuchElementException, got " + nameOf(t));
        check(t != null && "No value present".equals(t.getMessage()),
                "the NoSuchElementException must carry the message \"No value present\"");
        t = null;
        try {
            Optional.empty().orElseThrow();
        } catch (Throwable x) {
            t = x;
        }
        check("java.util.NoSuchElementException".equals(nameOf(t))
                        && "No value present".equals(t.getMessage()),
                "Optional.empty().orElseThrow() must throw the same NoSuchElementException");

        check("x".equals(Optional.empty().orElse("x")),
                "Optional.empty().orElse(\"x\") must be \"x\"");
        check("v".equals(opt(Optional.of("v")).orElse("x")),
                "Optional.of(\"v\").orElse(\"x\") must be \"v\"");
        check("Optional.empty".equals(opt(Optional.empty()).toString()),
                "Optional.empty().toString() must be exactly \"Optional.empty\"");
        check("Optional[v]".equals(opt(Optional.of("v")).toString()),
                "Optional.of(\"v\").toString() must be exactly \"Optional[v]\"");
        check(opt(Optional.empty()).hashCode() == 0, "Optional.empty().hashCode() must be 0");
        check(opt(Optional.of("v")).hashCode() == "v".hashCode(),
                "Optional.of(\"v\").hashCode() must be \"v\".hashCode()");
        check(opt(Optional.empty()).equals(Optional.ofNullable(null)),
                "Optional.empty() must equal Optional.ofNullable(null)");
        check(opt(Optional.of("v")).equals(Optional.of("v")),
                "two Optionals over equal values must be equal");
        check(!opt(Optional.of("v")).equals("v"),
                "an Optional must not equal its own bare value");
        check(!opt(Optional.of("v")).equals(Optional.of("w")),
                "Optionals over different values must not be equal");

        // Optional.of(0) is the load-bearing row for this defect's arithmetic: the flag for an
        // ABSENT Optional is Int(0), and a genuinely present Optional holding the boxed Integer 0
        // must be distinguishable from it. Same slot content by value, opposite answers.
        check(opt(Optional.of(Integer.valueOf(0))).isPresent(),
                "Optional.of(Integer.valueOf(0)) must be PRESENT -- a zero VALUE is not an absent"
                        + " Optional, and a VM that stores an int flag in the value slot cannot"
                        + " tell the two apart");
        check(!opt(Optional.<Integer>empty()).isPresent(),
                "Optional.<Integer>empty() must be ABSENT");
        check(!opt(Optional.of(Integer.valueOf(0))).equals(Optional.empty()),
                "Optional.of(0) must not equal Optional.empty()");
        check(opt(Optional.of(Integer.valueOf(0))).hashCode() == 0
                        && opt(Optional.<Integer>empty()).hashCode() == 0,
                "both happen to hash to 0 -- stated so nobody mistakes the hash for the"
                        + " discriminator");

        ob("core-of-zero-present", opt(Optional.of(Integer.valueOf(0))).isPresent() ? 1 : 0);
        ob("core-empty-present", opt(Optional.empty()).isPresent() ? 1 : 0);

        sectionEnd("core", 5 * LAWS + 18);
    }

    // -----------------------------------------------------------------------
    // 2. prim -- OptionalInt / OptionalLong / OptionalDouble, WHOSE LAYOUTS
    //    DIFFER, and that is the point.
    //
    // javap, JDK 25:
    //   java.util.Optional        private final T value;                 <- ONE slot, a reference
    //   java.util.OptionalInt     private final boolean isPresent;       <- slot 0
    //                             private final int value;               <- slot 1
    //   java.util.OptionalLong    private final boolean isPresent; long value;
    //   java.util.OptionalDouble  private final boolean isPresent; double value;
    //
    // The (flag, payload) idiom the nine natives use is CORRECT here and wrong
    // for the reference Optional. This block is what makes that claim testable
    // rather than rhetorical.
    // -----------------------------------------------------------------------
    static void prim() {
        lawsInt("prim.int.empty", optI(OptionalInt.empty()), false);
        lawsInt("prim.int.of", optI(OptionalInt.of(5)), true);
        lawsInt("prim.int.of-zero", optI(OptionalInt.of(0)), true);
        lawsLong("prim.long.empty", optL(OptionalLong.empty()), false);
        lawsLong("prim.long.of", optL(OptionalLong.of(5L)), true);
        lawsDouble("prim.double.empty", optD(OptionalDouble.empty()), false);
        lawsDouble("prim.double.of", optD(OptionalDouble.of(5.5)), true);

        check("OptionalInt.empty".equals(optI(OptionalInt.empty()).toString()),
                "OptionalInt.empty().toString() must be exactly \"OptionalInt.empty\"");
        check("OptionalInt[5]".equals(optI(OptionalInt.of(5)).toString()),
                "OptionalInt.of(5).toString() must be exactly \"OptionalInt[5]\"");
        check("OptionalLong[5]".equals(optL(OptionalLong.of(5L)).toString()),
                "OptionalLong.of(5).toString() must be exactly \"OptionalLong[5]\"");
        check("OptionalDouble[5.5]".equals(optD(OptionalDouble.of(5.5)).toString()),
                "OptionalDouble.of(5.5).toString() must be exactly \"OptionalDouble[5.5]\"");
        check(optI(OptionalInt.of(5)).hashCode() == 5, "OptionalInt.of(5).hashCode() must be 5");
        check(optL(OptionalLong.of(5L)).hashCode() == 5,
                "OptionalLong.of(5).hashCode() must be 5");
        check(optD(OptionalDouble.of(5.5)).hashCode() == 1075183616,
                "OptionalDouble.of(5.5).hashCode() must be 1075183616");
        check(optI(OptionalInt.of(0)).hashCode() == 0
                        && optI(OptionalInt.empty()).hashCode() == 0,
                "OptionalInt.of(0) and OptionalInt.empty() both hash to 0 -- the hash is not the"
                        + " discriminator here either");
        check(!optI(OptionalInt.of(0)).equals(OptionalInt.empty()),
                "OptionalInt.of(0) must not equal OptionalInt.empty()");

        // NaN and the two zeros: OptionalDouble.equals is specified on the BITS, not on ==.
        check(optD(OptionalDouble.of(Double.NaN)).equals(OptionalDouble.of(Double.NaN)),
                "OptionalDouble.of(NaN).equals(OptionalDouble.of(NaN)) must be TRUE -- unlike ==");
        check(!optD(OptionalDouble.of(0.0)).equals(OptionalDouble.of(-0.0)),
                "OptionalDouble.of(0.0).equals(OptionalDouble.of(-0.0)) must be FALSE -- unlike ==");

        // The four families are NOT interchangeable, and equals must say so.
        check(!opt(Optional.empty()).equals(OptionalInt.empty()),
                "Optional.empty() must not equal OptionalInt.empty()");
        check(!optI(OptionalInt.empty()).equals(OptionalLong.empty()),
                "OptionalInt.empty() must not equal OptionalLong.empty()");

        ob("prim-int-empty-present", optI(OptionalInt.empty()).isPresent() ? 1 : 0);
        ob("prim-int-zero-present", optI(OptionalInt.of(0)).isPresent() ? 1 : 0);

        sectionEnd("prim", 3 * LAWS_I + 2 * LAWS_L + 2 * LAWS_D + 13);
    }

    // -----------------------------------------------------------------------
    // 3. stream -- terminal operations that MINT an Optional.
    //
    // Expected minting route: the stream pipeline's terminal ops. If any of
    // findFirst / findAny / reduce / max / min / average is implemented
    // Rust-side on this VM (W7-2 records the primitive-stream terminal surface
    // as a place where they are), the Optional it returns is minted by that
    // native and this block reaches C12-3's shape through it. If they are all
    // pure bytecode, this block is contract coverage and says so by passing.
    //
    // Every row is deterministic on any host: fixed elements, sequential
    // streams, and findAny on a sequential stream is specified to be free but
    // is in practice findFirst -- so this block asserts only that it is one of
    // the two ELEMENTS, never which.
    // -----------------------------------------------------------------------
    static void stream() {
        laws("stream.findFirst.empty", opt(Stream.<String>of().findFirst()), String.class);
        laws("stream.findFirst", opt(Stream.of(7, 8).findFirst()), Integer.class);
        laws("stream.findAny", opt(Stream.of(7, 8).findAny()), Integer.class);
        laws("stream.max.empty", opt(Stream.<Integer>of().max(Integer::compare)), Integer.class);
        laws("stream.max", opt(Stream.of(7, 8).max(Integer::compare)), Integer.class);
        laws("stream.min", opt(Stream.of(7, 8).min(Integer::compare)), Integer.class);
        laws("stream.reduce.empty", opt(Stream.<Integer>of().reduce((x, y) -> x + y)),
                Integer.class);
        laws("stream.reduce", opt(Stream.of(7, 8).reduce((x, y) -> x + y)), Integer.class);
        laws("stream.reducing", opt(Stream.of(7, 8).collect(Collectors.reducing((x, y) -> x + y))),
                Integer.class);
        laws("stream.maxBy", opt(Stream.of(7, 8).collect(Collectors.maxBy(Integer::compare))),
                Integer.class);

        check(!opt(Stream.of().findFirst()).isPresent(),
                "Stream.of().findFirst() must be EMPTY");
        check(opt(Stream.of(7, 8).findFirst()).get().equals(Integer.valueOf(7)),
                "Stream.of(7, 8).findFirst() must be 7 on a SEQUENTIAL stream");
        Integer any = opt(Stream.of(7, 8).findAny()).get();
        check(any.equals(Integer.valueOf(7)) || any.equals(Integer.valueOf(8)),
                "findAny must return one of the stream's ELEMENTS, got " + any);
        check(opt(Stream.of(7, 8).max(Integer::compare)).get().equals(Integer.valueOf(8)),
                "Stream.of(7, 8).max must be 8");
        check(opt(Stream.of(7, 8).min(Integer::compare)).get().equals(Integer.valueOf(7)),
                "Stream.of(7, 8).min must be 7");
        check(opt(Stream.of(7, 8).reduce((x, y) -> x + y)).get().equals(Integer.valueOf(15)),
                "Stream.of(7, 8).reduce(+) must be 15");
        check(!opt(Stream.<Integer>of().reduce((x, y) -> x + y)).isPresent(),
                "reduce over an EMPTY stream must be an empty Optional, not 0");
        check(opt(Stream.of(7, 8).collect(Collectors.reducing((x, y) -> x + y)))
                        .get().equals(Integer.valueOf(15)),
                "Collectors.reducing must produce Optional[15]");
        check(!opt(Stream.<Integer>of().collect(Collectors.reducing((x, y) -> x + y))).isPresent(),
                "Collectors.reducing over an empty stream must be empty");

        lawsInt("stream.int.max.empty", optI(IntStream.of().max()), false);
        lawsInt("stream.int.max", optI(IntStream.of(3, 9, 4).max()), true);
        lawsInt("stream.int.min", optI(IntStream.of(3, 9, 4).min()), true);
        lawsInt("stream.int.findFirst", optI(IntStream.of(3, 9).findFirst()), true);
        lawsInt("stream.int.reduce", optI(IntStream.of(3, 9).reduce((x, y) -> x + y)), true);
        lawsLong("stream.long.max.empty", optL(LongStream.of().max()), false);
        lawsLong("stream.long.max", optL(LongStream.of(3L, 9L).max()), true);
        lawsDouble("stream.double.max.empty", optD(DoubleStream.of().max()), false);
        lawsDouble("stream.double.max", optD(DoubleStream.of(3.5, 9.5).max()), true);
        lawsDouble("stream.int.average.empty", optD(IntStream.of().average()), false);
        lawsDouble("stream.int.average", optD(IntStream.of(3, 9).average()), true);

        check(optI(IntStream.of(3, 9, 4).max()).getAsInt() == 9, "IntStream.max must be 9");
        check(optI(IntStream.of(3, 9, 4).min()).getAsInt() == 3, "IntStream.min must be 3");
        check(optI(IntStream.of(3, 9).findFirst()).getAsInt() == 3,
                "IntStream.findFirst must be 3");
        check(optI(IntStream.of(3, 9).reduce((x, y) -> x + y)).getAsInt() == 12,
                "IntStream.reduce(+) must be 12");
        check(optL(LongStream.of(3L, 9L).max()).getAsLong() == 9L, "LongStream.max must be 9");
        check(Double.doubleToRawLongBits(optD(DoubleStream.of(3.5, 9.5).max()).getAsDouble())
                        == Double.doubleToRawLongBits(9.5),
                "DoubleStream.max must be 9.5");
        check(Double.doubleToRawLongBits(optD(IntStream.of(3, 9).average()).getAsDouble())
                        == Double.doubleToRawLongBits(6.0),
                "IntStream.average must be 6.0");

        ob("stream-empty-findFirst-present", opt(Stream.of().findFirst()).isPresent() ? 1 : 0);
        ob("stream-int-empty-max-present", optI(IntStream.of().max()).isPresent() ? 1 : 0);

        sectionEnd("stream", 10 * LAWS + 5 * LAWS_I + 2 * LAWS_L + 4 * LAWS_D + 16);
    }

    // -----------------------------------------------------------------------
    // 4. version -- Runtime.Version accessors.
    //
    // Expected minting route: `Runtime.version()` is answered by the VM (it has
    // to be -- the numbers are the VM's own), and its Optional-returning
    // accessors are natural candidates to be built Rust-side alongside it.
    //
    // Runtime.Version.parse(String) rows are used for the VALUE assertions
    // because they are fixed by the ARGUMENT rather than by the host: a parsed
    // "17.0.1+12-LTS" has build()=Optional[12] and optional()=Optional[LTS] on
    // any JDK 25. Runtime.version() itself gets laws only.
    // -----------------------------------------------------------------------
    static void version() {
        Runtime.Version lts = Runtime.Version.parse("17.0.1+12-LTS");
        Runtime.Version plain = Runtime.Version.parse("25");
        Runtime.Version ea = Runtime.Version.parse("21-ea+3");

        laws("version.parse-lts.build", opt(lts.build()), Integer.class);
        laws("version.parse-lts.optional", opt(lts.optional()), String.class);
        laws("version.parse-lts.pre", opt(lts.pre()), String.class);
        laws("version.parse-plain.build", opt(plain.build()), Integer.class);
        laws("version.parse-plain.optional", opt(plain.optional()), String.class);
        laws("version.parse-plain.pre", opt(plain.pre()), String.class);
        laws("version.parse-ea.build", opt(ea.build()), Integer.class);
        laws("version.parse-ea.pre", opt(ea.pre()), String.class);
        laws("version.parse-ea.optional", opt(ea.optional()), String.class);
        laws("version.runtime.build", opt(Runtime.version().build()), Integer.class);
        laws("version.runtime.optional", opt(Runtime.version().optional()), String.class);
        laws("version.runtime.pre", opt(Runtime.version().pre()), String.class);

        check(opt(lts.build()).get().equals(Integer.valueOf(12)),
                "Version.parse(\"17.0.1+12-LTS\").build() must be Optional[12]");
        check("LTS".equals(opt(lts.optional()).get()),
                "Version.parse(\"17.0.1+12-LTS\").optional() must be Optional[LTS]");
        check(!opt(lts.pre()).isPresent(),
                "Version.parse(\"17.0.1+12-LTS\").pre() must be EMPTY");
        check(lts.feature() == 17, "Version.parse(\"17.0.1+12-LTS\").feature() must be 17");
        check(!opt(plain.build()).isPresent(), "Version.parse(\"25\").build() must be EMPTY");
        check(!opt(plain.optional()).isPresent(), "Version.parse(\"25\").optional() must be EMPTY");
        check(!opt(plain.pre()).isPresent(), "Version.parse(\"25\").pre() must be EMPTY");
        check("ea".equals(opt(ea.pre()).get()), "Version.parse(\"21-ea+3\").pre() must be [ea]");
        check(opt(ea.build()).get().equals(Integer.valueOf(3)),
                "Version.parse(\"21-ea+3\").build() must be Optional[3]");
        check(!opt(ea.optional()).isPresent(),
                "Version.parse(\"21-ea+3\").optional() must be EMPTY");

        ob("version-plain-build-present", opt(plain.build()).isPresent() ? 1 : 0);
        ob("version-lts-build", opt(lts.build()).get().intValue());

        sectionEnd("version", 12 * LAWS + 10);
    }

    // -----------------------------------------------------------------------
    // 5. misc -- ModuleDescriptor, describeConstable, StackWalker.
    //
    // Expected minting routes, in descending order of how likely each is to be
    // Rust-side on this VM:
    //   * ModuleDescriptor accessors -- W2-3 records the descriptor answering
    //     fabricated EMPTY SETS, so it is served by the VM;
    //   * StackWalker.walk -- the frames come from the VM's own stack;
    //   * describeConstable -- constant-pool shaped, and the JIT has thin
    //     direct helpers for that neighbourhood.
    //
    // Only java.base's mainClass() is asserted by VALUE (it is specified empty
    // for a module with no main class). Its version() is host-dependent and
    // gets laws only.
    // -----------------------------------------------------------------------
    static void misc() {
        laws("misc.module.mainClass", opt(Object.class.getModule().getDescriptor().mainClass()),
                String.class);
        laws("misc.module.rawVersion", opt(Object.class.getModule().getDescriptor().rawVersion()),
                String.class);
        laws("misc.constable.integer", opt(Integer.valueOf(1).describeConstable()), Integer.class);
        laws("misc.constable.string", opt("x".describeConstable()), String.class);
        laws("misc.constable.class", opt(String.class.describeConstable()), ClassDesc.class);
        // The laws run INSIDE the walk on purpose: a StackFrame is specified to be valid only
        // for the duration of the walk function, and laws() calls toString() and hashCode() on
        // the value. Examining it after the walk returned would make this row's own validity a
        // JDK implementation detail rather than a statement about the Optional.
        Boolean swPresent = StackWalker.getInstance().walk(s -> {
            Optional<StackWalker.StackFrame> first = opt(s.findFirst());
            laws("misc.stackwalker", first, StackWalker.StackFrame.class);
            return Boolean.valueOf(first.isPresent());
        });

        check(!opt(Object.class.getModule().getDescriptor().mainClass()).isPresent(),
                "java.base's ModuleDescriptor.mainClass() must be EMPTY");
        check(opt(Integer.valueOf(1).describeConstable()).get().equals(Integer.valueOf(1)),
                "Integer.valueOf(1).describeConstable() must be Optional[1]");
        check("x".equals(opt("x".describeConstable()).get()),
                "\"x\".describeConstable() must be Optional[x]");
        check(swPresent.booleanValue(),
                "StackWalker.walk(findFirst) must be PRESENT -- this frame exists");

        ob("misc-module-mainClass-present",
                opt(Object.class.getModule().getDescriptor().mainClass()).isPresent() ? 1 : 0);

        sectionEnd("misc", 6 * LAWS + 4);
    }

    // -----------------------------------------------------------------------
    // 6. process -- ProcessHandle and ProcessHandle.Info.
    //
    // Expected minting route: every one of these is a syscall answer, so the
    // Optional wrapping it is built where the syscall is. W7-10 and W7-46
    // record the process cluster as VM-implemented, and W7-10 specifically
    // records ProcessHandle's interface STUB BODIES -- which is the shape that
    // produces an Optional with nothing in it.
    //
    // PRESENCE here is a platform fact (info().command() is present on Windows
    // and Linux, commandLine() is present on neither reliably, arguments() is
    // absent on both), so these rows get LAWS and never a presence assertion.
    // The type law is what catches C12-3: a flag of 1 is not an Instant.
    // -----------------------------------------------------------------------
    static void process() {
        ProcessHandle self = ProcessHandle.current();
        ProcessHandle.Info info = self.info();

        laws("process.parent", opt(self.parent()), ProcessHandle.class);
        laws("process.of-invalid", opt(ProcessHandle.of(-1L)), ProcessHandle.class);
        laws("process.info.command", opt(info.command()), String.class);
        laws("process.info.commandLine", opt(info.commandLine()), String.class);
        laws("process.info.user", opt(info.user()), String.class);
        laws("process.info.startInstant", opt(info.startInstant()), java.time.Instant.class);
        laws("process.info.totalCpuDuration", opt(info.totalCpuDuration()), Duration.class);
        laws("process.info.arguments", opt(info.arguments()), String[].class);

        // The one presence fact that is not a platform fact: this process is alive, so its own
        // handle must be obtainable by pid, and it must be equal to the one we already hold.
        check(opt(ProcessHandle.of(self.pid())).isPresent(),
                "ProcessHandle.of(this process's own pid) must be PRESENT");
        check(self.equals(opt(ProcessHandle.of(self.pid())).get()),
                "ProcessHandle.of(own pid) must yield a handle EQUAL to ProcessHandle.current()"
                        + " -- if get() hands back a flag this cannot hold");
        check(opt(ProcessHandle.of(self.pid())).get().pid() == self.pid(),
                "the handle from of(pid) must report the same pid -- an int flag has no pid()");
        check(self.isAlive(), "ProcessHandle.current().isAlive() must be true");

        ob("process-self-by-pid-present", opt(ProcessHandle.of(self.pid())).isPresent() ? 1 : 0);

        sectionEnd("process", 8 * LAWS + 4);
    }

    // -----------------------------------------------------------------------
    // 7. httpmint -- THE ROWS THAT REACH C12-3, and the reason this file
    //    exists. Runs LAST because it is the one most likely to abort the VM
    //    rather than fail an assertion: an Int in a reference slot reaches
    //    getClass() and toString() here.
    //
    // Four of the nine sites in C12-3's table, all reachable off a builder with
    // no network, no DNS and no TLS peer:
    //
    //   http2.rs:1289  HttpClient.connectTimeout()  Optional<Duration>
    //   http2.rs:1727  HttpRequest.timeout()        Optional<Duration>
    //   http2.rs:1750  HttpRequest.version()        Optional<HttpClient.Version>
    //   http2.rs:1706  HttpRequest.bodyPublisher()  Optional<BodyPublisher>
    //
    // Each is asked ABSENT (which catches `isPresent()` reading Int(0) as
    // non-null) and PRESENT with a value this test chose (which catches get()
    // handing back Int(1) instead of the payload parked at slot 1). The
    // present rows do not merely compare -- they INVOKE a method on the value
    // (Duration.toMillis, Version.name, BodyPublisher.contentLength), because
    // the record's own prediction is that the caller gets "an Int where every
    // caller dereferences an object".
    //
    // The five sites that are NOT reachable here, and why:
    //   http2.rs:2121  HttpResponse.sslSession()      needs a real TLS response
    //   http2.rs:2108  HttpResponse.previousResponse() needs a real redirect
    //   http2.rs:2209                                  same object graph
    // Their absence is a stated gap, not an oversight.
    // -----------------------------------------------------------------------
    static void httpmint() {
        HttpClient bare = HttpClient.newBuilder().build();
        HttpClient timed = HttpClient.newBuilder().connectTimeout(Duration.ofMillis(1500)).build();
        URI uri = URI.create("http://cratonvm.invalid/x");
        HttpRequest plain = HttpRequest.newBuilder(uri).GET().build();
        HttpRequest full = HttpRequest.newBuilder(uri)
                .timeout(Duration.ofMillis(7000))
                .version(HttpClient.Version.HTTP_2)
                .POST(HttpRequest.BodyPublishers.ofString("hi"))
                .build();

        laws("http.client.connectTimeout.absent", opt(bare.connectTimeout()), Duration.class);
        laws("http.client.connectTimeout.present", opt(timed.connectTimeout()), Duration.class);
        laws("http.request.timeout.absent", opt(plain.timeout()), Duration.class);
        laws("http.request.timeout.present", opt(full.timeout()), Duration.class);
        laws("http.request.version.absent", opt(plain.version()), HttpClient.Version.class);
        laws("http.request.version.present", opt(full.version()), HttpClient.Version.class);
        laws("http.request.bodyPublisher.absent", opt(plain.bodyPublisher()),
                HttpRequest.BodyPublisher.class);
        laws("http.request.bodyPublisher.present", opt(full.bodyPublisher()),
                HttpRequest.BodyPublisher.class);
        laws("http.client.authenticator", opt(bare.authenticator()),
                java.net.Authenticator.class);
        laws("http.client.proxy", opt(bare.proxy()), java.net.ProxySelector.class);
        laws("http.client.cookieHandler", opt(bare.cookieHandler()),
                java.net.CookieHandler.class);
        laws("http.client.executor", opt(bare.executor()),
                java.util.concurrent.Executor.class);

        // THE ABSENT HALF. Int(0) is not null to ref_operand_is_null, so a flag-shaped Optional
        // answers TRUE here.
        ob("mint-connectTimeout-absent-present", opt(bare.connectTimeout()).isPresent() ? 1 : 0);
        ob("mint-timeout-absent-present", opt(plain.timeout()).isPresent() ? 1 : 0);
        ob("mint-version-absent-present", opt(plain.version()).isPresent() ? 1 : 0);
        ob("mint-bodyPublisher-absent-present", opt(plain.bodyPublisher()).isPresent() ? 1 : 0);
        check(!opt(bare.connectTimeout()).isPresent(),
                "a client built with NO connect timeout must answer connectTimeout().isPresent()"
                        + " == false");
        check(!opt(plain.timeout()).isPresent(),
                "a request built with NO timeout must answer timeout().isPresent() == false");
        check(!opt(plain.version()).isPresent(),
                "a request built with NO explicit version must answer version().isPresent()"
                        + " == false");
        check(!opt(plain.bodyPublisher()).isPresent(),
                "a GET request must answer bodyPublisher().isPresent() == false");

        Throwable t = null;
        try {
            opt(bare.connectTimeout()).get();
        } catch (Throwable x) {
            t = x;
        }
        check("java.util.NoSuchElementException".equals(nameOf(t)),
                "connectTimeout().get() with no timeout set must throw NoSuchElementException,"
                        + " got " + nameOf(t));
        check(opt(plain.timeout()).orElse(Duration.ofMillis(42)).toMillis() == 42L,
                "timeout().orElse(42ms) with no timeout set must be the ARGUMENT, not a flag");

        // THE PRESENT HALF. Each of these dereferences the value.
        ob("mint-connectTimeout-present-millis",
                (int) opt(timed.connectTimeout()).get().toMillis());
        ob("mint-timeout-present-millis", (int) opt(full.timeout()).get().toMillis());
        obName("mint-version-present-name", opt(full.version()).get().name());
        ob("mint-bodyPublisher-present-len", (int) opt(full.bodyPublisher()).get().contentLength());
        check(opt(timed.connectTimeout()).get().toMillis() == 1500L,
                "connectTimeout().get() must be the Duration the builder was given (1500 ms), not"
                        + " a presence flag");
        check(opt(full.timeout()).get().toMillis() == 7000L,
                "timeout().get() must be the Duration the builder was given (7000 ms), not a"
                        + " presence flag");
        check(opt(full.timeout()).get().equals(Duration.ofMillis(7000)),
                "timeout().get() must EQUAL Duration.ofMillis(7000)");
        check("HTTP_2".equals(opt(full.version()).get().name()),
                "version().get() must be the HttpClient.Version enum constant HTTP_2");
        check(opt(full.version()).get() == HttpClient.Version.HTTP_2,
                "version().get() must be IDENTICAL to the enum constant -- an enum has one"
                        + " instance per name");
        check(opt(full.bodyPublisher()).get().contentLength() == 2L,
                "bodyPublisher().get().contentLength() must be 2 for the body \"hi\"");
        check(opt(timed.connectTimeout()).get().equals(Duration.ofMillis(1500)),
                "connectTimeout().get() must EQUAL Duration.ofMillis(1500)");

        // The non-Optional twin, as a route discriminator. `HttpClient.version()` returns an
        // HttpClient.Version DIRECTLY and its native already had this class of bug found and
        // fixed once (the comment on `version_enum` in http2.rs records it). If the direct twin
        // is right while the Optional-wrapped one is wrong, the wrapper is the defect.
        obName("mint-client-version-direct", timed.version().name());
        check(timed.version() == HttpClient.Version.HTTP_2 || timed.version() == HttpClient.Version.HTTP_1_1,
                "HttpClient.version() must be an HttpClient.Version constant");

        // Two builder round-trips that need NO enum constant, added by E13.
        //
        // Every other present row above reaches its native through an argument
        // the VM has to mint -- a Duration, an HttpClient.Version. These two do
        // not, which is why they are here: they are the only rows in this block
        // that can still speak if the enum constants themselves are unavailable.
        //
        // ofByteArray: the argument is a `[B`, and the native that used to read
        // it matched `Value::Int` against the array reference, so every
        // publisher it minted claimed length 0 while the `ofString` sibling one
        // method above always read its reference argument.
        ob("mint-ofByteArray-len",
                (int) HttpRequest.BodyPublishers.ofByteArray(new byte[5]).contentLength());
        check(HttpRequest.BodyPublishers.ofByteArray(new byte[5]).contentLength() == 5L,
                "BodyPublishers.ofByteArray(new byte[5]).contentLength() must be 5, not 0 -- a"
                        + " native that reads the array as an int measures nothing");

        // expectContinue is THE NEGATIVE CONTROL of the builder family, in
        // Java. Its descriptor is (Z): a primitive boolean really does arrive
        // as an int, so the idiom that is wrong for the six reference-taking
        // setters is CORRECT here. A blanket "no builder reads an int" fix
        // breaks this row and nothing else in the block.
        HttpRequest continued = HttpRequest.newBuilder(uri).GET().expectContinue(true).build();
        ob("mint-expectContinue-roundtrip", continued.expectContinue() ? 1 : 0);
        check(continued.expectContinue(),
                "expectContinue(true) must round-trip -- (Z) is a PRIMITIVE parameter and the"
                        + " int-reading idiom is right here");

        sectionEnd("httpmint", 12 * LAWS + 16);
    }

    // Ordered by how likely each family is to ABORT the VM rather than fail an assertion,
    // ascending -- an int in a reference slot can reach getClass() and toString(), and a Rust
    // panic truncates the run. `core` and `prim` are the negative controls and go first, so a VM
    // that dies in `httpmint` has already reported whether the class library itself is sound.
    static final String[] FAMILIES = {
        "core", "prim", "stream", "version", "misc", "process", "httpmint",
    };

    static void runFamily(String name) {
        if ("core".equals(name)) {
            core();
        } else if ("prim".equals(name)) {
            prim();
        } else if ("stream".equals(name)) {
            stream();
        } else if ("version".equals(name)) {
            version();
        } else if ("misc".equals(name)) {
            misc();
        } else if ("process".equals(name)) {
            process();
        } else if ("httpmint".equals(name)) {
            httpmint();
        } else {
            throw new AssertionError("unknown family: " + name);
        }
    }

    public static void main(String[] args) {
        String only = null;
        for (int k = 0; k < args.length; k++) {
            if (args[k].startsWith("--only=")) {
                only = args[k].substring("--only=".length());
            } else if ("--list".equals(args[k])) {
                for (int j = 0; j < FAMILIES.length; j++) {
                    System.out.println("CK RJdkOptionalShape family=" + FAMILIES[j]);
                }
                return;
            }
        }
        if (only == null) {
            for (int k = 0; k < FAMILIES.length; k++) {
                runFamily(FAMILIES[k]);
            }
        } else {
            System.out.println("CK RJdkOptionalShape only=" + only);
            runFamily(only);
        }
        System.out.println("CK RJdkOptionalShape checks=" + checks);
        System.out.println("PASS RJdkOptionalShape (" + checks + " checks)");
    }
}
