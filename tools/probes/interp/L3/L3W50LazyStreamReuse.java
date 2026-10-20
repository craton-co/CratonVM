// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 50, lane L3 (review) -- a second terminal
// operation on a stream stage whose first terminal was a short-circuit one
// (`findFirst`, `findAny`, `anyMatch`, `allMatch`, `noneMatch`), or the other
// way round. HotSpot throws `IllegalStateException("stream has already been
// operated upon or closed")` at the second. Under `--compatible` this VM runs
// `filter`/`map`/`skip`/`limit`/`peek` lazily (`stream_make_lazy_derived`),
// and the five short-circuit terminals pulled the lazy chain without linking
// the stage (`stream_pull`, not the `stream_elements` funnel), so until wave
// 50 the second terminal answered from the drained source:
// `findFirst-twice: returned Optional.empty`, `anyMatch-twice: returned
// false`, `count-then-findFirst: returned Optional.empty`,
// `findFirst-then-count: returned 0` (`--jdk-only` runs the eager stages and
// matched). Since wave 50 they link first (`stream_pull_terminal`).
//
// The first row is repeated as the last row.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, measured; the same with -Xint)
// findFirst-twice: java.lang.IllegalStateException: stream has already been operated upon or closed
// findAny-twice: java.lang.IllegalStateException: stream has already been operated upon or closed
// anyMatch-twice: java.lang.IllegalStateException: stream has already been operated upon or closed
// allMatch-twice: java.lang.IllegalStateException: stream has already been operated upon or closed
// noneMatch-twice: java.lang.IllegalStateException: stream has already been operated upon or closed
// count-then-findFirst: java.lang.IllegalStateException: stream has already been operated upon or closed
// findFirst-then-count: java.lang.IllegalStateException: stream has already been operated upon or closed
// findFirst-then-filter: java.lang.IllegalStateException: stream has already been operated upon or closed
// once-each: returned 2 true false true 3
// chained-once: returned [4]
// findFirst-twice-again: java.lang.IllegalStateException: stream has already been operated upon or closed
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W50LazyStreamReuse`.
import java.util.List;
import java.util.stream.Stream;

public class L3W50LazyStreamReuse {
    interface Call {
        Object run() throws Throwable;
    }

    static void row(String n, Call c) {
        String out;
        try {
            out = "returned " + c.run();
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(n + ": " + out);
    }

    static final List<Integer> L = List.of(1, 2, 3);

    static Object findFirstTwice() {
        Stream<Integer> s = L.stream().filter(x -> x > 1);
        s.findFirst();
        return s.findFirst();
    }

    public static void main(String[] a) {
        row("findFirst-twice", L3W50LazyStreamReuse::findFirstTwice);
        row("findAny-twice", () -> {
            Stream<Integer> s = L.stream().map(x -> x);
            s.findAny();
            return s.findAny();
        });
        row("anyMatch-twice", () -> {
            Stream<Integer> s = L.stream().map(x -> x);
            s.anyMatch(x -> x > 1);
            return s.anyMatch(x -> x > 1);
        });
        row("allMatch-twice", () -> {
            Stream<Integer> s = L.stream().peek(x -> { });
            s.allMatch(x -> x > 0);
            return s.allMatch(x -> x > 0);
        });
        row("noneMatch-twice", () -> {
            Stream<Integer> s = L.stream().skip(0);
            s.noneMatch(x -> x > 5);
            return s.noneMatch(x -> x > 5);
        });
        row("count-then-findFirst", () -> {
            Stream<Integer> s = L.stream().skip(1);
            s.count();
            return s.findFirst();
        });
        row("findFirst-then-count", () -> {
            Stream<Integer> s = L.stream().limit(2);
            s.findFirst();
            return s.count();
        });
        row("findFirst-then-filter", () -> {
            Stream<Integer> s = L.stream().filter(x -> true);
            s.findFirst();
            return s.filter(x -> true).count();
        });
        row("once-each", () -> L.stream().filter(x -> x > 1).findFirst().get() + " "
                + L.stream().map(x -> x).anyMatch(x -> x == 3) + " "
                + L.stream().skip(1).allMatch(x -> x > 2) + " "
                + L.stream().limit(2).noneMatch(x -> x > 2) + " "
                + L.stream().peek(x -> { }).count());
        row("chained-once", () -> L.stream().filter(x -> x > 1).map(x -> x + 1).skip(1).limit(1).toList());
        row("findFirst-twice-again", L3W50LazyStreamReuse::findFirstTwice);
    }
}
