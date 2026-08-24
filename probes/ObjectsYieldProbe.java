import java.util.Objects;

/**
 * Behavioural parity for every `java.util.Objects` static whose real-JDK-path
 * registration was retagged `Intrinsic` -> `SyntheticStub`, so a loaded real
 * `java/util/Objects` now runs its own bytecode instead of the fallback native.
 *
 * The retag is a THROUGHPUT change (see the
 * springboot-configurationpropertysources Term 3 note), so the whole risk is
 * that the two bodies do not answer identically. Every row prints
 * `name=value`, so the output diffs against HotSpot verbatim -- including the
 * exception rows, which are where a native and a real body are most likely to
 * disagree: the native raised its own NPE with its own message, and the real
 * body's message is the JDK's.
 *
 * Rows cover: the value answers, null on both sides, the exception TYPE and
 * MESSAGE, the identity-vs-equals distinction (a `new String` that is `equals`
 * but not `==`), arrays (where `Objects.equals` must NOT deep-compare and
 * `deepEquals` must), `hash` varargs including the empty and null-array cases,
 * and the supplier-driven `requireNonNullElseGet` including a supplier that
 * itself returns null.
 */
public class ObjectsYieldProbe {

    static void p(String k, Object v) { System.out.println("OYP " + k + "=" + v); }

    static void ex(String k, Runnable r) {
        try { r.run(); p(k, "no-throw"); }
        catch (Throwable t) { p(k, t.getClass().getName() + "|" + t.getMessage()); }
    }

    public static void main(String[] args) {
        String a = "prop-42";
        String b = new String("prop-42");   // equals but not ==
        String c = "other";
        Object nul = null;

        // ---- equals ----
        p("eq.same",      Objects.equals(a, a));
        p("eq.equalNotId",Objects.equals(a, b));
        p("eq.diff",      Objects.equals(a, c));
        p("eq.nullNull",  Objects.equals(nul, nul));
        p("eq.nullLeft",  Objects.equals(nul, a));
        p("eq.nullRight", Objects.equals(a, nul));
        // arrays: equals is reference-ish (Object.equals), NOT element-wise
        int[] x = {1,2,3}, y = {1,2,3};
        p("eq.arraySame", Objects.equals(x, x));
        p("eq.arrayEqual",Objects.equals(x, y));
        p("deepEq.array", Objects.deepEquals(x, y));
        p("deepEq.null",  Objects.deepEquals(nul, nul));

        // ---- hashCode / hash ----
        p("hc.value",     Objects.hashCode(a) == a.hashCode());
        p("hc.null",      Objects.hashCode(nul));
        p("hash.one",     Objects.hash(a) == java.util.Arrays.hashCode(new Object[]{a}));
        p("hash.empty",   Objects.hash());
        p("hash.two",     Objects.hash(a, c) == java.util.Arrays.hashCode(new Object[]{a, c}));
        p("hash.withNull",Objects.hash(a, null));
        ex("hash.nullArr", () -> p("hash.nullArrV", Objects.hash((Object[]) null)));

        // ---- toString ----
        p("ts.value",     Objects.toString(a));
        p("ts.null",      Objects.toString(nul));
        p("ts.nullDef",   Objects.toString(nul, "DEF"));
        p("ts.valDef",    Objects.toString(a, "DEF"));
        p("ts.nullNullDef", Objects.toString(nul, null));

        // ---- isNull / nonNull ----
        p("isNull.t",     Objects.isNull(nul));
        p("isNull.f",     Objects.isNull(a));
        p("nonNull.t",    Objects.nonNull(a));
        p("nonNull.f",    Objects.nonNull(nul));

        // ---- requireNonNull: value, type AND message ----
        p("rnn.value",    Objects.requireNonNull(a));
        p("rnn.identity", Objects.requireNonNull(a) == a);
        ex("rnn.null",    () -> Objects.requireNonNull(nul));
        ex("rnn.nullMsg", () -> Objects.requireNonNull(nul, "my-message"));
        p("rnn.msgOk",    Objects.requireNonNull(a, "unused") == a);

        // ---- requireNonNullElse ----
        p("rnne.first",   Objects.requireNonNullElse(a, c));
        p("rnne.second",  Objects.requireNonNullElse(nul, c));
        ex("rnne.bothNull", () -> Objects.requireNonNullElse(nul, null));

        // ---- requireNonNullElseGet ----
        p("rnneg.first",  Objects.requireNonNullElseGet(a, () -> c));
        p("rnneg.supplied", Objects.requireNonNullElseGet(nul, () -> c));
        ex("rnneg.nullSupplier", () -> Objects.requireNonNullElseGet(nul, null));
        ex("rnneg.supplierNull", () -> Objects.requireNonNullElseGet(nul, () -> null));

        // ---- checkIndex family ----
        p("ci.ok",        Objects.checkIndex(2, 5));
        ex("ci.high",     () -> Objects.checkIndex(5, 5));
        ex("ci.neg",      () -> Objects.checkIndex(-1, 5));
        p("cfti.ok",      Objects.checkFromToIndex(1, 3, 5));
        ex("cfti.bad",    () -> Objects.checkFromToIndex(3, 1, 5));
        p("cfis.ok",      Objects.checkFromIndexSize(1, 2, 5));
        ex("cfis.bad",    () -> Objects.checkFromIndexSize(4, 2, 5));

        // ---- compare ----
        p("cmp.same",     Objects.compare(a, a, java.util.Comparator.naturalOrder()));
        p("cmp.lt",       Objects.compare("a", "b", java.util.Comparator.<String>naturalOrder()) < 0);

        // ---- toIdentityString is null-hostile ----
        ex("tis.null",    () -> Objects.toIdentityString(null));
        p("tis.nonNull",  Objects.toIdentityString(a) != null);
    }
}
