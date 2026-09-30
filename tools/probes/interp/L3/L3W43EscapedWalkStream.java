// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 43, lane L3 -- a `StackWalker.walk` stream used
// outside, or twice inside, its function:
// docs/internal/fixed-bugs/interpreter-L3-a-stackwalker-stream-stays-usable-after-walk-returns-FIXED-20261007.md.
//
// Rows:
//   escaped-count       -- the function returns the stream; `count()` after;
//   escaped-map         -- the escaped stream's `map(...).toList()` after;
//   escaped-iterator    -- the escaped stream's `iterator().hasNext()` after;
//   consumed-escaped    -- `count()` inside, then the escaped stream's `count()`;
//   reused-inside       -- `count()` twice inside the function;
//   walk-still-works    -- an ordinary walk afterwards;
//   derived-escaped     -- `filter(...)` inside, the derived stream's `count()`
//                          after (differs on CratonVM, see below).
//
// HotSpot 25 prints (no agent; the same with -Xint; measured, JDK 25.0.3):
//     escaped-count: java.lang.IllegalStateException: This stack stream is not valid for walking.
//     escaped-map: java.lang.IllegalStateException: This stack stream is not valid for walking.
//     escaped-iterator: java.lang.IllegalStateException: This stack stream is not valid for walking.
//     consumed-escaped: java.lang.IllegalStateException: stream has already been operated upon or closed
//     reused-inside: java.lang.IllegalStateException: stream has already been operated upon or closed
//     walk-still-works: returned L3W43EscapedWalkStream.lambda$main$10
//     derived-escaped: java.lang.IllegalStateException: This stack stream is not valid for walking.
//
// CratonVM before wave 43 (the walk's synthetic stream has no validity
// state and no linked flag): `escaped-count: returned 3`-style counts on the
// first four rows and on `reused-inside`. Since wave 43 the first six rows
// print HotSpot's lines; `derived-escaped` still returned a count until wave
// 44 (an intermediate operation inside the function snapshots the frames into
// a new stream, which now names the walk's stream and refuses with it:
// docs/internal/fixed-bugs/interpreter-L3-a-stream-derived-inside-a-stack-walk-stays-usable-FIXED-20261008.md).
// `escaped-iterator` throws at `iterator()` on CratonVM and at `hasNext()` on
// HotSpot; the row prints the same line.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W43EscapedWalkStream`.
import java.lang.StackWalker.StackFrame;
import java.util.Iterator;
import java.util.stream.Stream;

public class L3W43EscapedWalkStream {
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
        row("escaped-count", () -> {
            Stream<StackFrame> s = w.walk(x -> x);
            return s.count();
        });
        row("escaped-map", () -> {
            Stream<StackFrame> s = w.walk(x -> x);
            return s.map(StackFrame::getMethodName).toList();
        });
        row("escaped-iterator", () -> {
            Stream<StackFrame> s = w.walk(x -> x);
            Iterator<StackFrame> it = s.iterator();
            return it.hasNext();
        });
        row("consumed-escaped", () -> {
            Stream<StackFrame> s = w.walk(x -> {
                x.count();
                return x;
            });
            return s.count();
        });
        row("reused-inside", () -> w.walk(x -> {
            x.count();
            return x.count();
        }));
        row("walk-still-works", () -> w.walk(x -> x
                .map(f -> f.getClassName() + "." + f.getMethodName())
                .findFirst()
                .orElse("none")));
        row("derived-escaped", () -> {
            Stream<StackFrame> s = w.walk(x -> x.filter(f -> true));
            return s.count();
        });
    }
}
