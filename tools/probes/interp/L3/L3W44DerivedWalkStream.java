// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 44, lane L3 -- a stream DERIVED from a
// `StackWalker.walk` stream inside the walk's function, used after the walk:
// docs/internal/fixed-bugs/interpreter-L3-a-stream-derived-inside-a-stack-walk-stays-usable-FIXED-20261008.md.
//
// Rows:
//   filter-escaped        -- `filter(...)` inside, the derived stream's `count()` after;
//   map-escaped           -- `map(...)` inside, `toList()` after;
//   chain-escaped         -- `filter(...).map(...).limit(...)` inside, `count()` after;
//   sorted-escaped        -- `sorted(...)` inside, `findFirst()` after;
//   consumed-then-escaped -- `filter(...)` inside, counted inside, `count()` again after;
//   derived-inside        -- `filter(...).count()` inside (still works);
//   derived-after-used    -- the escaped derived stream's `map(...)` after (throws at the op);
//   other-stream-escapes  -- a stream of a list built inside the function escapes (still works).
//
// HotSpot 25 prints (no agent; the same with -Xint; measured, JDK 25.0.3, Windows box):
//     filter-escaped: java.lang.IllegalStateException: This stack stream is not valid for walking.
//     map-escaped: java.lang.IllegalStateException: This stack stream is not valid for walking.
//     chain-escaped: java.lang.IllegalStateException: This stack stream is not valid for walking.
//     sorted-escaped: java.lang.IllegalStateException: This stack stream is not valid for walking.
//     consumed-then-escaped: java.lang.IllegalStateException: stream has already been operated upon or closed
//     derived-inside: returned true
//     derived-after-used: java.lang.IllegalStateException: This stack stream is not valid for walking.
//     other-stream-escapes: returned 3
//
// CratonVM base `5248262b7` (read from the code: an intermediate operation
// builds a new element-snapshot stream, `make_derived_stream`, which the end
// of the walk never marks): the first four rows and `derived-after-used`
// return a value (`filter-escaped: returned <n>`); the other three match.
// Since wave 44 a stream derived from a walk's stream records the walk
// stream (`native-collections`' `STREAM_FIELD_WALK`) and every row matches.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W44DerivedWalkStream`.
import java.lang.StackWalker.StackFrame;
import java.util.Comparator;
import java.util.List;
import java.util.stream.Stream;

public class L3W44DerivedWalkStream {
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

    public static void main(String[] args) {
        StackWalker w = StackWalker.getInstance();
        row("filter-escaped", () -> {
            Stream<StackFrame> d = w.walk(s -> s.filter(f -> true));
            return d.count();
        });
        row("map-escaped", () -> {
            Stream<String> d = w.walk(s -> s.map(StackFrame::getMethodName));
            return d.toList().size();
        });
        row("chain-escaped", () -> {
            Stream<String> d = w.walk(s -> s.filter(f -> true).map(StackFrame::getMethodName).limit(2));
            return d.count();
        });
        row("sorted-escaped", () -> {
            Stream<String> d = w.walk(s -> s.map(StackFrame::getMethodName).sorted(Comparator.naturalOrder()));
            return d.findFirst().isPresent();
        });
        row("consumed-then-escaped", () -> {
            Stream<StackFrame> d = w.walk(s -> {
                Stream<StackFrame> f = s.filter(x -> true);
                f.count();
                return f;
            });
            return d.count();
        });
        row("derived-inside", () -> w.walk(s -> s.filter(f -> true).count() > 0));
        row("derived-after-used", () -> {
            Stream<StackFrame> d = w.walk(s -> s.filter(f -> true));
            return d.map(StackFrame::getMethodName).toList().size();
        });
        row("other-stream-escapes", () -> {
            Stream<Integer> d = w.walk(s -> List.of(1, 2, 3).stream().filter(x -> x > 0));
            return d.count();
        });
    }
}
