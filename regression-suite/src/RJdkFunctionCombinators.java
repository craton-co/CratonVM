import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.Comparator;
import java.util.List;
import java.util.Locale;
import java.util.function.BiConsumer;
import java.util.function.BiFunction;
import java.util.function.BiPredicate;
import java.util.function.BinaryOperator;
import java.util.function.Consumer;
import java.util.function.DoubleConsumer;
import java.util.function.DoublePredicate;
import java.util.function.DoubleUnaryOperator;
import java.util.function.Function;
import java.util.function.IntConsumer;
import java.util.function.IntPredicate;
import java.util.function.IntUnaryOperator;
import java.util.function.LongConsumer;
import java.util.function.LongPredicate;
import java.util.function.LongUnaryOperator;
import java.util.function.Predicate;
import java.util.function.Supplier;
import java.util.function.UnaryOperator;

/**
 * JDK-only corpus: the {@code java.util.function} combinator family -- the
 * {@code default} and {@code static} methods that build one functional
 * interface out of another ({@code and}, {@code or}, {@code negate},
 * {@code andThen}, {@code compose}, {@code identity}, {@code maxBy}/
 * {@code minBy}), plus {@code java.util.Comparator}'s combinators.
 *
 * <p>These are the widest-reach shapes in the JDK-only screen: every stream
 * pipeline, collection filter and Spring predicate chain goes through them.
 * CratonVM intercepted several of them with natives that MINT a stand-in class
 * no JDK image declares -- {@code Predicate$$Lambda$And}, {@code Consumer$AndThen},
 * {@code Function$AndThen}, {@code BinaryOperator$MaxBy} and friends -- so under
 * {@code --jdk-only} the caller got either a {@code NoClassDefFoundError} at the
 * combinator or an {@code AbstractMethodError} ("has no Code attribute") at the
 * first call of the composed function. The real methods have real bodies in
 * every supported image, so strict mode must run those instead.
 *
 * <p><b>Non-null is not the contract.</b> Every combinator here is asserted on
 * its computed VALUE and, where the spec defines one, on its SHORT-CIRCUIT
 * behaviour: {@code and} must not evaluate its right side once the left is
 * false, {@code or} must not once the left is true. A fabricated stand-in that
 * merely returns an object passes a {@code != null} check, and two defects in
 * this family survived a year behind exactly that.
 *
 * <p><b>The name check is deliberately an equality, not a {@code contains}.</b>
 * {@code RJdkLambdas} discriminates a real generated lambda with
 * {@code getName().contains("$$Lambda")}. That predicate is USELESS for
 * {@code Predicate.and}: the fabricated stand-in is itself spelled
 * {@code java.util.function.Predicate$$Lambda$And}, so it satisfies it. The
 * fabricated names are asserted against exactly.
 *
 * <p>Determinism: generated lambda class names carry an address-derived suffix
 * and are never printed. Only shape predicates and computed values are.
 */
public class RJdkFunctionCombinators {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    /**
     * Every stand-in class name this family has ever minted. A combinator's
     * result must be an instance of NONE of them.
     */
    static final String[] FABRICATED = {
        "java.util.function.Predicate$$Lambda$And",
        "java.util.function.Predicate$$Lambda$Or",
        "java.util.function.Predicate$$Lambda$Negate",
        "java.util.function.Consumer$AndThen",
        "java.util.function.Function$AndThen",
        "java.util.function.Function$Compose",
        "java.util.function.Function$Identity",
        "java.util.function.UnaryOperator$Identity",
        "java.util.function.BinaryOperator$MaxBy",
        "java.util.function.BinaryOperator$MinBy",
        "java.util.Comparator$Native",
    };

    static void notFabricated(Object o, String what) {
        check(o != null, what + " returned null");
        String n = o.getClass().getName();
        for (int i = 0; i < FABRICATED.length; i++) {
            check(!n.equals(FABRICATED[i]),
                    what + " is the fabricated compatibility class " + FABRICATED[i]);
        }
    }

    // ---- Predicate.and / or / negate / not ---------------------------------

    static void predicateCombinators() {
        Predicate<String> ne = s -> !s.isEmpty();
        Predicate<String> sh = s -> s.length() < 4;

        Predicate<String> and = ne.and(sh);
        notFabricated(and, "Predicate.and");
        check(and.test("ab"), "and: non-empty and short must hold");
        check(!and.test(""), "and: empty must fail the left side");
        check(!and.test("abcdef"), "and: long must fail the right side");

        Predicate<String> or = ne.or(sh);
        notFabricated(or, "Predicate.or");
        check(or.test("abcdef"), "or: non-empty satisfies the left side");
        check(or.test(""), "or: empty is short, so the right side saves it");

        Predicate<String> neg = ne.negate();
        notFabricated(neg, "Predicate.negate");
        check(neg.test(""), "negate: empty");
        check(!neg.test("ab"), "negate: non-empty");
        check(ne.negate().negate().test("ab"), "double negate is the original");

        Predicate<String> not = Predicate.not(ne);
        notFabricated(not, "Predicate.not");
        check(not.test(""), "Predicate.not(nonEmpty) on empty");
        check(!not.test("ab"), "Predicate.not(nonEmpty) on non-empty");

        Predicate<String> isEq = Predicate.isEqual("k");
        check(isEq.test("k"), "Predicate.isEqual match");
        check(!isEq.test("j"), "Predicate.isEqual mismatch");

        // Chains of three, both associations, must agree.
        Predicate<String> startsA = s -> s.startsWith("a");
        check(ne.and(sh).and(startsA).test("ab"), "and-chain true");
        check(!ne.and(sh).and(startsA).test("zb"), "and-chain false on the last");
        check(ne.and(sh.and(startsA)).test("ab"), "and-chain right-associated");
        check(!ne.or(sh).negate().test("ab"), "negate of an or");

        // Short circuit. `and` must not run the right side when the left is
        // false; `or` must not when the left is true. This is the assertion a
        // fabricated stand-in that eagerly evaluates both sides fails.
        int[] rightRuns = new int[1];
        Predicate<String> counting = s -> {
            rightRuns[0]++;
            return true;
        };
        Predicate<String> alwaysFalse = s -> false;
        check(!alwaysFalse.and(counting).test("x"), "and(false, _) is false");
        check(rightRuns[0] == 0, "and must NOT evaluate the right side after a false left");

        Predicate<String> alwaysTrue = s -> true;
        check(alwaysTrue.or(counting).test("x"), "or(true, _) is true");
        check(rightRuns[0] == 0, "or must NOT evaluate the right side after a true left");

        // ...and it must run it in the cases where the spec says it does.
        check(alwaysTrue.and(counting).test("x"), "and(true, true)");
        check(rightRuns[0] == 1, "and must evaluate the right side after a true left");
        check(alwaysFalse.or(counting).test("x"), "or(false, true)");
        check(rightRuns[0] == 2, "or must evaluate the right side after a false left");

        // Left-to-right order, observed through one shared counter.
        List<String> order = new ArrayList<String>();
        Predicate<String> left = s -> {
            order.add("L");
            return true;
        };
        Predicate<String> right = s -> {
            order.add("R");
            return true;
        };
        left.and(right).test("x");
        check(order.equals(Arrays.asList("L", "R")), "and evaluates left before right: " + order);

        System.out.println("CK RJdkFunctionCombinators predicate rightRuns=" + rightRuns[0]
                + " order=" + order);
    }

    // ---- Consumer.andThen / BiConsumer.andThen ------------------------------

    static void consumerCombinators() {
        StringBuilder sb = new StringBuilder();
        Consumer<String> c1 = sb::append;
        Consumer<String> both = c1.andThen(s -> sb.append(s.toUpperCase(Locale.ROOT)));
        notFabricated(both, "Consumer.andThen");
        both.accept("a");
        check(sb.toString().equals("aA"), "Consumer.andThen order/value, got " + sb);
        both.accept("b");
        check(sb.toString().equals("aAbB"), "Consumer.andThen is reusable, got " + sb);

        // Three deep, and the ORDER is the contract, not just the effect count.
        StringBuilder t = new StringBuilder();
        Consumer<String> three = ((Consumer<String>) s -> t.append("1"))
                .andThen(s -> t.append("2"))
                .andThen(s -> t.append("3"));
        three.accept("ignored");
        check(t.toString().equals("123"), "Consumer.andThen chains in order, got " + t);

        // andThen does NOT swallow an exception from the first consumer, and
        // must not run the second when the first threw.
        int[] secondRuns = new int[1];
        Consumer<String> boom = s -> {
            throw new IllegalStateException("first");
        };
        Consumer<String> after = s -> secondRuns[0]++;
        boolean threw = false;
        try {
            boom.andThen(after).accept("x");
        } catch (IllegalStateException e) {
            threw = "first".equals(e.getMessage());
        }
        check(threw, "Consumer.andThen must propagate the first consumer's exception");
        check(secondRuns[0] == 0, "Consumer.andThen must not run the second after a throw");

        StringBuilder bi = new StringBuilder();
        BiConsumer<String, Integer> b1 = (s, i) -> bi.append(s).append(i);
        BiConsumer<String, Integer> b2 = b1.andThen((s, i) -> bi.append("|"));
        notFabricated(b2, "BiConsumer.andThen");
        b2.accept("x", Integer.valueOf(7));
        check(bi.toString().equals("x7|"), "BiConsumer.andThen, got " + bi);

        System.out.println("CK RJdkFunctionCombinators consumer=" + sb + " three=" + t
                + " bi=" + bi);
    }

    // ---- Function / UnaryOperator / BiFunction ------------------------------

    static void functionCombinators() {
        Function<String, Integer> len = String::length;
        Function<Integer, Integer> dbl = i -> i.intValue() * 2;

        Function<String, Integer> at = len.andThen(dbl);
        notFabricated(at, "Function.andThen");
        check(at.apply("abcd").intValue() == 8, "Function.andThen value");

        Function<String, Integer> co = dbl.compose(len);
        notFabricated(co, "Function.compose");
        check(co.apply("abcde").intValue() == 10, "Function.compose value");

        // andThen and compose are transposes of each other, not aliases: assert
        // the ORDER is really opposite by using two non-commuting operations.
        Function<Integer, Integer> plus1 = i -> Integer.valueOf(i.intValue() + 1);
        Function<Integer, Integer> times3 = i -> Integer.valueOf(i.intValue() * 3);
        check(plus1.andThen(times3).apply(Integer.valueOf(2)).intValue() == 9,
                "andThen is (x+1)*3");
        check(plus1.compose(times3).apply(Integer.valueOf(2)).intValue() == 7,
                "compose is (x*3)+1");

        Function<String, String> id = Function.identity();
        notFabricated(id, "Function.identity");
        String sample = "same";
        check(id.apply(sample) == sample, "Function.identity must return the SAME reference");
        check(id.apply(null) == null, "Function.identity(null)");
        check(id.andThen(len).apply("abcd").intValue() == 4, "identity.andThen");

        UnaryOperator<String> uid = UnaryOperator.identity();
        notFabricated(uid, "UnaryOperator.identity");
        check(uid.apply(sample) == sample, "UnaryOperator.identity same reference");
        UnaryOperator<String> up = s -> s.toUpperCase(Locale.ROOT);
        // UnaryOperator inherits Function's combinators, which WIDEN the result
        // back to Function -- the assignments are explicit so the check does not
        // depend on target-type inference through a chained call.
        Function<String, String> upThenId = up.andThen(uid);
        Function<String, String> idThenUp = up.compose(uid);
        check(upThenId.apply("ab").equals("AB"), "UnaryOperator.andThen");
        check(idThenUp.apply("cd").equals("CD"), "UnaryOperator.compose");

        BiFunction<String, String, String> cat = (a, b) -> a + b;
        BiFunction<String, String, Integer> catLen = cat.andThen(len);
        notFabricated(catLen, "BiFunction.andThen");
        check(catLen.apply("ab", "cde").intValue() == 5, "BiFunction.andThen value");

        BiPredicate<String, String> sameLen = (a, b) -> a.length() == b.length();
        BiPredicate<String, String> bothShort = (a, b) -> a.length() < 3 && b.length() < 3;
        check(sameLen.and(bothShort).test("ab", "cd"), "BiPredicate.and true");
        check(!sameLen.and(bothShort).test("abcd", "efgh"), "BiPredicate.and false on right");
        check(sameLen.or(bothShort).test("abcd", "efgh"), "BiPredicate.or via left");
        check(sameLen.negate().test("a", "bc"), "BiPredicate.negate");
        notFabricated(sameLen.and(bothShort), "BiPredicate.and");
        notFabricated(sameLen.or(bothShort), "BiPredicate.or");
        notFabricated(sameLen.negate(), "BiPredicate.negate");

        int[] biRight = new int[1];
        BiPredicate<String, String> biCount = (a, b) -> {
            biRight[0]++;
            return true;
        };
        BiPredicate<String, String> biFalse = (a, b) -> false;
        check(!biFalse.and(biCount).test("a", "b"), "BiPredicate.and(false, _)");
        check(biRight[0] == 0, "BiPredicate.and must short-circuit");

        Supplier<String> sup = () -> "s";
        check(sup.get().equals("s"), "Supplier.get");

        System.out.println("CK RJdkFunctionCombinators function=" + at.apply("abcd")
                + " compose=" + co.apply("abcde") + " biRight=" + biRight[0]);
    }

    // ---- BinaryOperator.maxBy / minBy --------------------------------------

    static void binaryOperatorCombinators() {
        Comparator<String> byLen = Comparator.comparingInt(String::length);

        BinaryOperator<String> max = BinaryOperator.maxBy(byLen);
        notFabricated(max, "BinaryOperator.maxBy");
        check(max.apply("ab", "abcd").equals("abcd"), "maxBy picks the longer");
        check(max.apply("abcd", "ab").equals("abcd"), "maxBy is order-insensitive on value");

        BinaryOperator<String> min = BinaryOperator.minBy(byLen);
        notFabricated(min, "BinaryOperator.minBy");
        check(min.apply("ab", "abcd").equals("ab"), "minBy picks the shorter");
        check(min.apply("abcd", "ab").equals("ab"), "minBy is order-insensitive on value");

        // The documented tie-break: on compare()==0 both return the FIRST
        // argument, and this is identity, not equality.
        String first = new String("xy");
        String second = new String("zw");
        check(max.apply(first, second) == first, "maxBy tie returns the first argument");
        check(min.apply(first, second) == first, "minBy tie returns the first argument");

        System.out.println("CK RJdkFunctionCombinators maxBy=" + max.apply("ab", "abcd")
                + " minBy=" + min.apply("ab", "abcd"));
    }

    // ---- Primitive specialisations ------------------------------------------

    static void primitiveCombinators() {
        IntPredicate ipos = i -> i > 0;
        IntPredicate ismall = i -> i < 10;
        check(ipos.and(ismall).test(5), "IntPredicate.and true");
        check(!ipos.and(ismall).test(50), "IntPredicate.and false on right");
        check(ipos.or(ismall).test(50), "IntPredicate.or via left");
        check(ipos.negate().test(-1), "IntPredicate.negate");
        notFabricated(ipos.and(ismall), "IntPredicate.and");
        notFabricated(ipos.negate(), "IntPredicate.negate");

        int[] intRight = new int[1];
        IntPredicate icount = i -> {
            intRight[0]++;
            return true;
        };
        check(!((IntPredicate) i -> false).and(icount).test(1), "IntPredicate.and(false, _)");
        check(intRight[0] == 0, "IntPredicate.and must short-circuit");
        check(((IntPredicate) i -> true).or(icount).test(1), "IntPredicate.or(true, _)");
        check(intRight[0] == 0, "IntPredicate.or must short-circuit");

        LongPredicate lpos = l -> l > 0L;
        check(lpos.and(l -> l < 10L).test(5L), "LongPredicate.and");
        check(lpos.or(l -> l < -100L).test(1L), "LongPredicate.or");
        check(lpos.negate().test(-1L), "LongPredicate.negate");

        DoublePredicate dpos = d -> d > 0.0;
        check(dpos.and(d -> d < 1.0).test(0.5), "DoublePredicate.and");
        check(dpos.or(d -> d < -1.0).test(2.0), "DoublePredicate.or");
        check(dpos.negate().test(-0.5), "DoublePredicate.negate");

        IntUnaryOperator iinc = i -> i + 1;
        IntUnaryOperator itriple = i -> i * 3;
        check(iinc.andThen(itriple).applyAsInt(2) == 9, "IntUnaryOperator.andThen is (x+1)*3");
        check(iinc.compose(itriple).applyAsInt(2) == 7, "IntUnaryOperator.compose is (x*3)+1");
        check(IntUnaryOperator.identity().applyAsInt(42) == 42, "IntUnaryOperator.identity");

        LongUnaryOperator linc = l -> l + 1L;
        check(linc.andThen(l -> l * 3L).applyAsLong(2L) == 9L, "LongUnaryOperator.andThen");
        check(LongUnaryOperator.identity().applyAsLong(42L) == 42L, "LongUnaryOperator.identity");

        DoubleUnaryOperator dinc = d -> d + 1.0;
        check(dinc.andThen(d -> d * 3.0).applyAsDouble(2.0) == 9.0, "DoubleUnaryOperator.andThen");
        check(DoubleUnaryOperator.identity().applyAsDouble(2.5) == 2.5,
                "DoubleUnaryOperator.identity");

        StringBuilder ic = new StringBuilder();
        IntConsumer ithen = ((IntConsumer) i -> ic.append("a").append(i))
                .andThen(i -> ic.append("b").append(i));
        ithen.accept(3);
        check(ic.toString().equals("a3b3"), "IntConsumer.andThen order, got " + ic);

        StringBuilder lc = new StringBuilder();
        ((LongConsumer) l -> lc.append("a")).andThen(l -> lc.append("b")).accept(1L);
        check(lc.toString().equals("ab"), "LongConsumer.andThen order, got " + lc);

        StringBuilder dc = new StringBuilder();
        ((DoubleConsumer) d -> dc.append("a")).andThen(d -> dc.append("b")).accept(1.0);
        check(dc.toString().equals("ab"), "DoubleConsumer.andThen order, got " + dc);

        System.out.println("CK RJdkFunctionCombinators primitive intRight=" + intRight[0]
                + " ic=" + ic);
    }

    // ---- Comparator combinators ---------------------------------------------

    static void comparatorCombinators() {
        Comparator<String> byLen = Comparator.comparingInt(String::length);
        Comparator<String> natural = Comparator.naturalOrder();

        notFabricated(byLen, "Comparator.comparingInt");
        notFabricated(natural, "Comparator.naturalOrder");
        check(byLen.compare("ab", "abc") < 0, "comparingInt shorter first");
        check(byLen.compare("abc", "ab") > 0, "comparingInt longer second");
        check(byLen.compare("ab", "cd") == 0, "comparingInt equal lengths tie");

        Comparator<String> rev = byLen.reversed();
        notFabricated(rev, "Comparator.reversed");
        check(rev.compare("ab", "abc") > 0, "reversed flips the sign");
        check(rev.reversed().compare("ab", "abc") < 0, "double reversed is the original");

        // thenComparing must be consulted ONLY on a tie of the first key.
        Comparator<String> lenThenNatural = byLen.thenComparing(natural);
        notFabricated(lenThenNatural, "Comparator.thenComparing");
        check(lenThenNatural.compare("cd", "ab") > 0, "thenComparing tie broken by natural");
        check(lenThenNatural.compare("ab", "cd") < 0, "thenComparing tie broken the other way");
        check(lenThenNatural.compare("zz", "abc") < 0,
                "thenComparing must NOT be consulted when the first key decides");

        int[] tieRuns = new int[1];
        Comparator<String> counting = (a, b) -> {
            tieRuns[0]++;
            return 0;
        };
        byLen.thenComparing(counting).compare("ab", "abcd");
        check(tieRuns[0] == 0, "thenComparing must not run when the first key decides");
        byLen.thenComparing(counting).compare("ab", "cd");
        check(tieRuns[0] == 1, "thenComparing must run on a tie");

        // An explicitly-typed lambda, not `String::valueOf`: that method
        // reference is overloaded eight ways and its inference here is a
        // javac-version risk this fixture has no interest in testing.
        Comparator<String> keyExtracted = Comparator.comparing((String s) -> s);
        check(keyExtracted.compare("a", "b") < 0, "Comparator.comparing key extractor");
        Comparator<String> lenThenInt = byLen.thenComparingInt(s -> s.charAt(0));
        check(lenThenInt.compare("ab", "bb") < 0, "thenComparingInt on a tie");

        Comparator<String> revOrder = Comparator.reverseOrder();
        notFabricated(revOrder, "Comparator.reverseOrder");
        check(revOrder.compare("a", "b") > 0, "Comparator.reverseOrder");

        // SINGLETON IDENTITY. `Comparator.naturalOrder()` and
        // `Comparator.reverseOrder()` are `Comparators.NaturalOrderComparator
        // .INSTANCE` and `Collections.ReverseComparator.REVERSE_ORDER` in the
        // real java.base bytecode, and each one's `reversed()` returns the
        // other -- not a fresh wrapper. This is an IMPLEMENTATION pin and is
        // taken deliberately: it is the one assertion in this family that a
        // native minting a `java.util.Comparator$Native` carrier PER CALL
        // fails without having to be named. The name list above only catches
        // the fabrications someone already wrote down.
        check(Comparator.<String>naturalOrder() == natural,
                "Comparator.naturalOrder() must return the same instance on every call");
        check(Comparator.<String>reverseOrder() == revOrder,
                "Comparator.reverseOrder() must return the same instance on every call");
        check(natural.reversed() == revOrder,
                "naturalOrder().reversed() must BE reverseOrder()");
        check(revOrder.reversed() == natural,
                "reverseOrder().reversed() must BE naturalOrder()");
        check(Collections.<String>reverseOrder() == revOrder,
                "Collections.reverseOrder() and Comparator.reverseOrder() are one object");

        Comparator<String> nullsFirst = Comparator.nullsFirst(natural);
        notFabricated(nullsFirst, "Comparator.nullsFirst");
        check(nullsFirst.compare(null, "a") < 0, "nullsFirst: null sorts first");
        check(nullsFirst.compare("a", null) > 0, "nullsFirst: non-null sorts after");
        check(nullsFirst.compare(null, null) == 0, "nullsFirst: two nulls tie");
        check(nullsFirst.compare("a", "b") < 0, "nullsFirst delegates on two non-nulls");

        Comparator<String> nullsLast = Comparator.nullsLast(natural);
        notFabricated(nullsLast, "Comparator.nullsLast");
        check(nullsLast.compare(null, "a") > 0, "nullsLast: null sorts last");
        check(nullsLast.compare("a", null) < 0, "nullsLast: non-null sorts first");

        // The combinators must actually order a real sort, not merely answer
        // compare() correctly in isolation.
        List<String> data = new ArrayList<String>(
                Arrays.asList("bbb", "a", "cc", "aa", "dddd"));
        data.sort(byLen.thenComparing(natural));
        check(data.equals(Arrays.asList("a", "aa", "cc", "bbb", "dddd")),
                "sort by len then natural, got " + data);
        data.sort(byLen.thenComparing(natural).reversed());
        check(data.equals(Arrays.asList("dddd", "bbb", "cc", "aa", "a")),
                "reversed composite sort, got " + data);

        // A Predicate built from a Comparator, which is the shape stream
        // pipelines actually use.
        Predicate<String> longEnough = s -> byLen.compare(s, "aa") >= 0;
        List<String> kept = new ArrayList<String>();
        for (String s : Arrays.asList("a", "aa", "bbb")) {
            if (longEnough.and(s2 -> !s2.startsWith("b")).test(s)) {
                kept.add(s);
            }
        }
        check(kept.equals(Arrays.asList("aa")), "comparator-backed predicate chain, got " + kept);

        System.out.println("CK RJdkFunctionCombinators comparator tieRuns=" + tieRuns[0]
                + " sorted=" + data + " kept=" + kept);
    }

    // ---- the null contract --------------------------------------------------

    /** Runs {@code r}; returns true iff it threw {@code NullPointerException}. */
    static boolean npes(Runnable r) {
        try {
            r.run();
            return false;
        } catch (NullPointerException expected) {
            return true;
        }
    }

    /**
     * Every combinator in this family opens with {@code Objects.requireNonNull}
     * and is SPECIFIED {@code @throws NullPointerException if other is null}.
     *
     * <p>This is the discriminator the value checks above cannot make, and it
     * is the one shape a minting native gets wrong for free: a native that
     * fabricates a carrier and stores the two operands into it never LOOKS at
     * the argument at combination time, so it hands back a perfectly good
     * object and raises nothing. Every computed-value and short-circuit
     * assertion in this file passes against such an implementation, because
     * none of them ever passes a null.
     *
     * <p>The throw must be EAGER — at the combinator, not deferred to the first
     * {@code test}/{@code apply} — which is why nothing here invokes the result.
     * A deferred implementation returns normally and fails these checks, which
     * is the intent: {@code Objects.requireNonNull(other)} is the first
     * statement of each of these bodies.
     *
     * <p>{@code Comparator.nullsFirst(null)} is deliberately absent: a null
     * comparator there is LEGAL and means "consider all non-null values equal",
     * so it is asserted as a positive instead.
     */
    static void combinatorsRejectNull() {
        final Predicate<String> p = s -> true;
        final Function<String, String> f = s -> s;
        final Consumer<String> c = s -> { };
        final BiPredicate<String, String> bp = (x, y) -> true;
        final BiConsumer<String, String> bc = (x, y) -> { };
        final BiFunction<String, String, String> bf = (x, y) -> x;
        final IntPredicate ip = i -> true;
        final LongPredicate lp = l -> true;
        final DoublePredicate dp = d -> true;
        final IntUnaryOperator iu = i -> i;
        final Comparator<String> nat = Comparator.naturalOrder();

        check(npes(() -> p.and(null)), "Predicate.and(null) must throw NullPointerException");
        check(npes(() -> p.or(null)), "Predicate.or(null) must throw NullPointerException");
        check(npes(() -> Predicate.not(null)), "Predicate.not(null) must throw");
        check(npes(() -> f.andThen(null)), "Function.andThen(null) must throw");
        check(npes(() -> f.compose(null)), "Function.compose(null) must throw");
        check(npes(() -> c.andThen(null)), "Consumer.andThen(null) must throw");
        check(npes(() -> bc.andThen(null)), "BiConsumer.andThen(null) must throw");
        check(npes(() -> bf.andThen(null)), "BiFunction.andThen(null) must throw");
        check(npes(() -> bp.and(null)), "BiPredicate.and(null) must throw");
        check(npes(() -> bp.or(null)), "BiPredicate.or(null) must throw");
        check(npes(() -> ip.and(null)), "IntPredicate.and(null) must throw");
        check(npes(() -> lp.or(null)), "LongPredicate.or(null) must throw");
        check(npes(() -> dp.and(null)), "DoublePredicate.and(null) must throw");
        check(npes(() -> iu.andThen(null)), "IntUnaryOperator.andThen(null) must throw");
        check(npes(() -> BinaryOperator.maxBy(null)), "BinaryOperator.maxBy(null) must throw");
        check(npes(() -> BinaryOperator.minBy(null)), "BinaryOperator.minBy(null) must throw");
        check(npes(() -> nat.thenComparing((Comparator<String>) null)),
                "Comparator.thenComparing(null) must throw");
        check(npes(() -> Comparator.comparing(null)), "Comparator.comparing(null) must throw");

        // The positive half of the same rule: a null comparator IS legal here.
        Comparator<String> permissive = Comparator.nullsFirst(null);
        notFabricated(permissive, "Comparator.nullsFirst(null)");
        check(permissive.compare("a", "b") == 0,
                "nullsFirst(null) must consider all non-null values equal");
        check(permissive.compare(null, "a") < 0, "nullsFirst(null) still sorts null first");

        // Predicate.isEqual(null) is SPECIFIED to be Objects::isNull, not a
        // predicate that throws or that matches nothing.
        Predicate<String> isNull = Predicate.isEqual(null);
        notFabricated(isNull, "Predicate.isEqual(null)");
        check(isNull.test(null), "Predicate.isEqual(null) must match null");
        check(!isNull.test("x"), "Predicate.isEqual(null) must not match a non-null");

        System.out.println("CK RJdkFunctionCombinators nullRejected=18 nullsFirstNullLegal=true");
    }

    public static void main(String[] args) throws Throwable {
        predicateCombinators();
        consumerCombinators();
        functionCombinators();
        binaryOperatorCombinators();
        primitiveCombinators();
        comparatorCombinators();
        combinatorsRejectNull();
        System.out.println("CK RJdkFunctionCombinators checks=" + checks);
        System.out.println("PASS RJdkFunctionCombinators (" + checks + " checks)");
    }
}
