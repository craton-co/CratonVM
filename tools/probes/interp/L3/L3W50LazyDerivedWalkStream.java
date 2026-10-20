// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 50, lane L3 -- streams derived inside a
// `StackWalker.walk` function by the LAZY intermediate operations (`filter`,
// `map`, `flatMap`, `limit`, `skip`, `peek`), used after the walk. Under
// `--compatible` this VM defers those operations (`stream_make_lazy_derived`,
// a `cratonvm/stream/LazyOp` chain); `--jdk-only` refuses the record and runs
// the eager stages. Until wave 50 the lazy stage dropped the walk slot
// (`STREAM_FIELD_WALK`), so under `--compatible` every derived row below
// returned a value (`L3W43EscapedWalkStream` `derived-escaped` and five rows of
// `L3W44DerivedWalkStream` showed it on the wave-49 host run).
//
// The first row runs on a cold VM and is repeated as the last row.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, measured; the same with -Xint)
// filter-escaped: java.lang.IllegalStateException: This stack stream is not valid for walking.
// peek-escaped: java.lang.IllegalStateException: This stack stream is not valid for walking.
// skip-escaped: java.lang.IllegalStateException: This stack stream is not valid for walking.
// limit-escaped: java.lang.IllegalStateException: This stack stream is not valid for walking.
// flatmap-escaped: java.lang.IllegalStateException: This stack stream is not valid for walking.
// two-lazy-escaped: java.lang.IllegalStateException: This stack stream is not valid for walking.
// lazy-then-sorted-escaped: java.lang.IllegalStateException: This stack stream is not valid for walking.
// foreach-escaped: java.lang.IllegalStateException: This stack stream is not valid for walking.
// iterator-escaped: java.lang.IllegalStateException: This stack stream is not valid for walking.
// lazy-inside: returned true
// lazy-reused-inside: java.lang.IllegalStateException: stream has already been operated upon or closed
// list-stream-escapes: returned 2
// filter-escaped-again: java.lang.IllegalStateException: This stack stream is not valid for walking.
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W50LazyDerivedWalkStream`.
import java.lang.StackWalker.StackFrame;
import java.util.Iterator;
import java.util.List;
import java.util.stream.Stream;

public class L3W50LazyDerivedWalkStream {
    interface Call {
        Object run() throws Throwable;
    }

    static void row(String name, Call call) {
        String out;
        try {
            Object r = call.run();
            out = "returned " + r;
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    static final StackWalker W = StackWalker.getInstance();

    static Object filterEscaped() {
        Stream<StackFrame> d = W.walk(s -> s.filter(f -> true));
        return d.count();
    }

    public static void main(String[] args) {
        row("filter-escaped", L3W50LazyDerivedWalkStream::filterEscaped);
        row("peek-escaped", () -> {
            Stream<StackFrame> d = W.walk(s -> s.peek(f -> { }));
            return d.count();
        });
        row("skip-escaped", () -> {
            Stream<StackFrame> d = W.walk(s -> s.skip(1));
            return d.count();
        });
        row("limit-escaped", () -> {
            Stream<StackFrame> d = W.walk(s -> s.limit(1));
            return d.findFirst().isPresent();
        });
        row("flatmap-escaped", () -> {
            Stream<String> d = W.walk(s -> s.flatMap(f -> Stream.of(f.getMethodName())));
            return d.toList().size() > 0;
        });
        row("two-lazy-escaped", () -> {
            Stream<String> d = W.walk(s -> s.filter(f -> true).map(StackFrame::getMethodName));
            return d.anyMatch(n -> n.equals("main"));
        });
        row("lazy-then-sorted-escaped", () -> {
            Stream<String> d = W.walk(s -> s.map(StackFrame::getMethodName).sorted());
            return d.count();
        });
        row("foreach-escaped", () -> {
            Stream<StackFrame> d = W.walk(s -> s.filter(f -> true));
            int[] n = new int[1];
            d.forEach(f -> n[0]++);
            return n[0];
        });
        row("iterator-escaped", () -> {
            Stream<StackFrame> d = W.walk(s -> s.map(f -> f));
            Iterator<StackFrame> it = d.iterator();
            return it.hasNext();
        });
        row("lazy-inside", () -> W.walk(s -> s.skip(1).map(StackFrame::getMethodName).limit(3).count() > 0));
        row("lazy-reused-inside", () -> W.walk(s -> {
            s.filter(f -> true);
            return s.map(f -> f).count();
        }));
        row("list-stream-escapes", () -> {
            Stream<Integer> d = W.walk(s -> List.of(1, 2, 3).stream().skip(1));
            return d.count();
        });
        row("filter-escaped-again", L3W50LazyDerivedWalkStream::filterEscaped);
    }
}
