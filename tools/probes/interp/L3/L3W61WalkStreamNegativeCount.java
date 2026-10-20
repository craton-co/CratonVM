// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 61, lane L3 (review): `limit` / `skip` with a
// negative count on a `StackWalker.walk` stream (and on ordinary streams,
// which `--compatible` serves with the same natives) throw
// `IllegalArgumentException` with the count as the message. CratonVM's
// stream natives answered with a stream (`limit(-1)` empty under
// `--compatible`, whole under `--jdk-only` on the walk's stream). The first
// row is repeated last.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// walk-limit: java.lang.IllegalArgumentException: -1
// walk-skip: java.lang.IllegalArgumentException: -2
// walk-map-limit: java.lang.IllegalArgumentException: -1
// stream-limit: java.lang.IllegalArgumentException: -3
// stream-skip: java.lang.IllegalArgumentException: -1
// int-limit: java.lang.IllegalArgumentException: -1
// long-skip: java.lang.IllegalArgumentException: -5
// double-limit: java.lang.IllegalArgumentException: -1
// walk-limit-zero: 0
// walk-skip-zero: true
// walk-limit-again: java.lang.IllegalArgumentException: -1
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W61WalkStreamNegativeCount`.
import java.util.List;
import java.util.stream.DoubleStream;
import java.util.stream.IntStream;
import java.util.stream.LongStream;
import java.util.stream.Stream;

public class L3W61WalkStreamNegativeCount {
    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        try {
            System.out.println(name + ": " + r.run());
        } catch (Throwable t) {
            System.out.println(name + ": " + t.getClass().getName() + ": " + t.getMessage());
        }
    }

    public static void main(String[] args) {
        StackWalker w = StackWalker.getInstance();
        row("walk-limit", () -> w.walk(s -> s.limit(-1).count()));
        row("walk-skip", () -> w.walk(s -> s.skip(-2).count()));
        row("walk-map-limit", () -> w.walk(s -> s.map(StackWalker.StackFrame::getMethodName).limit(-1).count()));
        row("stream-limit", () -> Stream.of(1, 2, 3).limit(-3).count());
        row("stream-skip", () -> List.of(1, 2, 3).stream().skip(-1).count());
        row("int-limit", () -> IntStream.range(0, 3).limit(-1).count());
        row("long-skip", () -> LongStream.range(0, 3).skip(-5).count());
        row("double-limit", () -> DoubleStream.of(1.0, 2.0).limit(-1).count());
        row("walk-limit-zero", () -> w.walk(s -> s.limit(0).count()));
        row("walk-skip-zero", () -> w.walk(s -> s.skip(0).count() > 0));
        row("walk-limit-again", () -> w.walk(s -> s.limit(-1).count()));
    }
}
