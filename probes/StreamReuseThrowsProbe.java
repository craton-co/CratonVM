// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// W7-65 -- `java.util.stream` linked-or-consumed parity.
//
// TWO-SIDED BY CONSTRUCTION. This probe is written against the one failure
// mode that the differential and the 70-vector corpus cannot see: a
// `linkedOrConsumed` flag set once too OFTEN, which turns a working stream
// into an `IllegalStateException` on the most pervasive path in the Spring
// Boot / Tomcat arms.
//
//   SECTION A (reuse.*)      -- a stream that HAS been linked or consumed must
//                               throw on the next operation. Under-setting
//                               shows up here as `no-throw`.
//   SECTION B (ordinary.*)   -- a WIDE sample of single-use pipelines must run
//                               to completion. Over-setting shows up here as
//                               `THREW:...` where a value is expected.
//   SECTION C (mutation.*)   -- section B's shapes with one extra operation
//                               injected on an intermediate stage. Every one
//                               MUST throw. This is what makes section B's
//                               greens load-bearing: if the flag were never
//                               set at all, section B would still be green and
//                               section C would go red. A build can only be
//                               green on B *and* C if the flag is set exactly
//                               where it belongs.
//
// Section B and section C are the same pipelines. The only difference between
// them is one extra operation applied to a stage that section B leaves alone.
// So no assertion in B is true independently of the code under test: flip the
// flag one notch too eager and B goes red; flip it one notch too lazy and C
// goes red.
//
// Every expected value in `StreamReuseThrowsProbe.expected.txt` was MEASURED on
// HotSpot 25 (Eclipse Adoptium jdk-25.0.3.9), never written from memory.
//
//   javac -d out probes/StreamReuseThrowsProbe.java
//   java -cp out StreamReuseThrowsProbe
//   cratonvm --real-jdk -cp out StreamReuseThrowsProbe

import java.util.ArrayList;
import java.util.Arrays;
import java.util.Comparator;
import java.util.Iterator;
import java.util.List;
import java.util.Optional;
import java.util.Spliterator;
import java.util.TreeMap;
import java.util.stream.Collectors;
import java.util.stream.DoubleStream;
import java.util.stream.IntStream;
import java.util.stream.LongStream;
import java.util.stream.Stream;

public final class StreamReuseThrowsProbe {

    static final String LINKED_MSG = "stream has already been operated upon or closed";

    static int ordinaryFailures = 0;
    static int mutationsThatThrew = 0;
    static int mutationsTotal = 0;

    public static void main(String[] args) {
        sectionA();
        sectionB();
        sectionC();
        line("SELFCHECK.ordinaryFailures", ordinaryFailures);
        line("SELFCHECK.mutationsThatThrew", mutationsThatThrew + "/" + mutationsTotal);
        line("SELFCHECK.verdict",
                (ordinaryFailures == 0 && mutationsThatThrew == mutationsTotal) ? "PASS" : "FAIL");
        System.out.println("PROBE-DONE");
    }

    // ---- fresh sources; never share a Stream between rows -------------------
    static List<String> src() {
        return new ArrayList<>(List.of("bb", "a", "ccc", "a"));
    }

    // ======================================================================
    // SECTION A -- a linked-or-consumed stream must throw on the next op.
    // ======================================================================
    static void sectionA() {
        // A1 is the row the shadow differential reports.
        line("reuse.terminalThenTerminal", thrownBy(() -> {
            Stream<String> s = src().stream();
            s.count();
            s.count();
        }));
        line("reuse.terminalThenTerminalMsg", messageOf(() -> {
            Stream<String> s = src().stream();
            s.count();
            s.count();
        }));
        line("reuse.linkThenLink", thrownBy(() -> {
            Stream<String> s = src().stream();
            s.filter(x -> true);
            s.map(x -> x);
        }));
        line("reuse.linkThenTerminal", thrownBy(() -> {
            Stream<String> s = src().stream();
            s.filter(x -> true);
            s.count();
        }));
        line("reuse.terminalThenLink", thrownBy(() -> {
            Stream<String> s = src().stream();
            s.count();
            s.filter(x -> true);
        }));
        line("reuse.collectThenCollect", thrownBy(() -> {
            Stream<String> s = src().stream();
            s.collect(Collectors.toList());
            s.collect(Collectors.toList());
        }));
        line("reuse.toArrayTwice", thrownBy(() -> {
            Stream<String> s = src().stream();
            s.toArray();
            s.toArray();
        }));
        line("reuse.forEachTwice", thrownBy(() -> {
            Stream<String> s = src().stream();
            s.forEach(x -> { });
            s.forEach(x -> { });
        }));
        line("reuse.iteratorThenCount", thrownBy(() -> {
            Stream<String> s = src().stream();
            s.iterator();
            s.count();
        }));
        line("reuse.spliteratorThenCount", thrownBy(() -> {
            Stream<String> s = src().stream();
            s.spliterator();
            s.count();
        }));
        line("reuse.closeThenCount", thrownBy(() -> {
            Stream<String> s = src().stream();
            s.close();
            s.count();
        }));
        line("reuse.onCloseAfterConsume", thrownBy(() -> {
            Stream<String> s = src().stream();
            s.count();
            s.onClose(() -> { });
        }));
        line("reuse.derivedStageReused", thrownBy(() -> {
            Stream<String> mid = src().stream().filter(x -> true);
            mid.count();
            mid.count();
        }));
        line("reuse.intStreamTwice", thrownBy(() -> {
            IntStream s = IntStream.rangeClosed(1, 5);
            s.sum();
            s.sum();
        }));
        line("reuse.longStreamTwice", thrownBy(() -> {
            LongStream s = LongStream.rangeClosed(1L, 5L);
            s.sum();
            s.sum();
        }));
        line("reuse.doubleStreamTwice", thrownBy(() -> {
            DoubleStream s = DoubleStream.of(1.0, 2.0);
            s.sum();
            s.sum();
        }));
        // NOT a reuse: `parallel()`/`sequential()`/`unordered()` are neither a
        // link nor a consume in the JDK. They return the same stage.
        line("reuse.parallelIsNotAnOp", thrownBy(() -> {
            Stream<String> s = src().stream();
            s.parallel();
            s.sequential();
            s.unordered();
            s.count();
        }));
        // NOT a reuse: two FRESH streams off the same source are both legal.
        line("reuse.twoFreshStreamsFromOneSource", thrownBy(() -> {
            List<String> list = src();
            list.stream().count();
            list.stream().count();
        }));
        // NOT a reuse: a consumed stream may still be closed.
        line("reuse.consumeThenClose", thrownBy(() -> {
            Stream<String> s = src().stream();
            s.count();
            s.close();
        }));
        // NOT a reuse: close() is idempotent.
        line("reuse.closeTwice", thrownBy(() -> {
            Stream<String> s = src().stream();
            s.close();
            s.close();
        }));
    }

    // ======================================================================
    // SECTION B -- ordinary single-use pipelines. Every one must produce a
    // value. A `THREW:` here is an over-set.
    // ======================================================================
    static void sectionB() {
        ordinary("filterMapToList", () -> src().stream().filter(s -> s.length() > 1)
                .map(String::toUpperCase).sorted().toList());
        ordinary("filterMapCollect", () -> src().stream().filter(s -> !s.isEmpty())
                .map(s -> s + "!").collect(Collectors.toList()));
        ordinary("flatMapDistinctSorted", () -> src().stream()
                .flatMap(s -> Arrays.stream(s.split("")))
                .distinct().sorted().toList());
        ordinary("sortedDistinctLimit", () -> src().stream().sorted().distinct().limit(2).toList());
        ordinary("skipLimitPeek", () -> {
            StringBuilder seen = new StringBuilder();
            List<String> out = src().stream().sorted().peek(seen::append).skip(1).limit(2).toList();
            return out + "|peeked=" + seen;
        });
        ordinary("peekMapForEach", () -> {
            StringBuilder seen = new StringBuilder();
            src().stream().peek(seen::append).map(String::toUpperCase).forEach(seen::append);
            return seen.toString();
        });
        ordinary("longChainEightOps", () -> src().stream()
                .filter(s -> true)
                .map(s -> s)
                .distinct()
                .sorted()
                .peek(s -> { })
                .skip(0)
                .limit(10)
                .flatMap(Stream::of)
                .toList());
        ordinary("reduceIdentity", () -> src().stream().sorted().reduce("", (a, b) -> a + b));
        ordinary("reduceOptional", () -> src().stream().sorted()
                .reduce((a, b) -> a + b).orElse("none"));
        ordinary("anyMatch", () -> src().stream().filter(s -> s.length() > 0).anyMatch(s -> s.equals("a")));
        ordinary("allMatchNoneMatch", () -> src().stream().map(String::trim).allMatch(s -> !s.isEmpty())
                + "/" + src().stream().map(String::trim).noneMatch(String::isEmpty));
        ordinary("findFirst", () -> src().stream().sorted().filter(s -> s.length() == 3)
                .findFirst().orElse("none"));
        ordinary("minMax", () -> src().stream().map(s -> s).min(Comparator.naturalOrder()).orElse("?")
                + "/" + src().stream().map(s -> s).max(Comparator.naturalOrder()).orElse("?"));
        ordinary("countAfterOps", () -> src().stream().filter(s -> true).map(s -> s).distinct().count());
        ordinary("toArray", () -> Arrays.toString(src().stream().sorted().map(s -> s).toArray()));
        ordinary("toArrayGenerator", () -> Arrays.toString(
                src().stream().sorted().map(s -> s).toArray(String[]::new)));
        ordinary("collectJoining", () -> src().stream().sorted().map(s -> s)
                .collect(Collectors.joining("-", "<", ">")));
        ordinary("collectToSet", () -> new TreeMap<>(src().stream().distinct()
                .collect(Collectors.toMap(s -> s, String::length))).toString());
        ordinary("collectGroupingBy", () -> new TreeMap<>(src().stream().map(s -> s)
                .collect(Collectors.groupingBy(String::length))).toString());
        ordinary("collectCounting", () -> src().stream().filter(s -> true)
                .collect(Collectors.counting()));
        ordinary("collect3Arg", () -> src().stream().map(s -> s)
                .collect(StringBuilder::new, StringBuilder::append, StringBuilder::append).toString());
        ordinary("iteratorDrain", () -> {
            Iterator<String> it = src().stream().sorted().map(s -> s).iterator();
            StringBuilder sb = new StringBuilder();
            while (it.hasNext()) {
                sb.append(it.next());
            }
            return sb.toString();
        });
        ordinary("spliteratorDrain", () -> {
            Spliterator<String> sp = src().stream().sorted().map(s -> s).spliterator();
            StringBuilder sb = new StringBuilder();
            sp.forEachRemaining(sb::append);
            return sb.toString();
        });
        ordinary("parallelChain", () -> src().stream().parallel().filter(s -> true)
                .map(String::toUpperCase).sorted().toList());
        ordinary("sequentialAfterParallel", () -> src().stream().parallel().sequential()
                .map(s -> s).sorted().toList());
        ordinary("unorderedChain", () -> src().stream().unordered().map(s -> s).sorted().toList());
        ordinary("intStreamChain", () -> IntStream.rangeClosed(1, 6).filter(i -> i % 2 == 0)
                .map(i -> i * 10).sorted().boxed().toList());
        ordinary("intStreamSumMinMaxAvg", () -> IntStream.rangeClosed(1, 5).map(i -> i).sum()
                + "/" + IntStream.rangeClosed(1, 5).map(i -> i).min().getAsInt()
                + "/" + IntStream.rangeClosed(1, 5).map(i -> i).max().getAsInt()
                + "/" + IntStream.rangeClosed(1, 5).map(i -> i).average().getAsDouble());
        ordinary("intStreamMapToObj", () -> IntStream.rangeClosed(1, 3).map(i -> i)
                .mapToObj(Integer::toString).collect(Collectors.joining(",")));
        ordinary("intStreamMapToLongDouble", () -> IntStream.rangeClosed(1, 3).map(i -> i)
                .asLongStream().sum()
                + "/" + IntStream.rangeClosed(1, 3).map(i -> i).asDoubleStream().sum());
        ordinary("intStreamToArray", () -> Arrays.toString(
                IntStream.rangeClosed(1, 4).filter(i -> true).map(i -> i).toArray()));
        ordinary("intStreamIterator", () -> {
            var it = IntStream.rangeClosed(1, 4).map(i -> i).iterator();
            StringBuilder sb = new StringBuilder();
            while (it.hasNext()) {
                sb.append(it.nextInt());
            }
            return sb.toString();
        });
        ordinary("longStreamChain", () -> LongStream.rangeClosed(1L, 6L).filter(i -> i % 2 == 0)
                .map(i -> i * 10L).boxed().toList());
        ordinary("longStreamMapToObj", () -> LongStream.rangeClosed(1L, 3L).map(i -> i)
                .mapToObj(Long::toString).collect(Collectors.joining(",")));
        ordinary("longStreamStats", () -> LongStream.rangeClosed(1L, 5L).map(i -> i).sum()
                + "/" + LongStream.rangeClosed(1L, 5L).map(i -> i).count());
        ordinary("doubleStreamChain", () -> DoubleStream.of(3.5, 1.5, 2.5).filter(d -> d > 1.0)
                .map(d -> d * 2.0).sorted().boxed().toList());
        ordinary("doubleStreamStats", () -> DoubleStream.of(1.0, 2.0, 3.0).map(d -> d).sum()
                + "/" + DoubleStream.of(1.0, 2.0, 3.0).map(d -> d).average().getAsDouble());
        ordinary("doubleStreamMapToObj", () -> DoubleStream.of(1.0, 2.0).map(d -> d)
                .mapToObj(Double::toString).collect(Collectors.joining(",")));
        ordinary("mapToIntBoxedSorted", () -> src().stream().map(s -> s).mapToInt(String::length)
                .boxed().sorted().toList());
        ordinary("streamOfIterateLimit", () -> Stream.iterate(1, x -> x * 2).limit(5)
                .map(x -> x).toList());
        ordinary("streamGenerateLimit", () -> Stream.generate(() -> "g").limit(3)
                .map(s -> s).toList());
        ordinary("streamConcat", () -> Stream.concat(src().stream().sorted(), src().stream().sorted())
                .map(s -> s).count());
        ordinary("streamEmptyReduce", () -> Stream.<String>empty().map(s -> s)
                .reduce((a, b) -> a).orElse("empty"));
        ordinary("streamOfNullableAndOf", () -> Stream.of("x", "y").map(s -> s).toList().toString()
                + "/" + Stream.ofNullable("z").map(s -> s).count());
        ordinary("arraysStreamChain", () -> Arrays.stream(new String[] { "b", "a" })
                .sorted().map(s -> s).toList());
        // A stream consumed inside try-with-resources: forEach consumes it,
        // then close() runs. close() after a consume must NOT throw.
        ordinary("tryWithResources", () -> {
            StringBuilder sb = new StringBuilder();
            boolean[] closed = { false };
            try (Stream<String> s = src().stream().onClose(() -> closed[0] = true)) {
                s.sorted().map(x -> x).forEach(sb::append);
            }
            return sb + "|closed=" + closed[0];
        });
        // Two independent pipelines off ONE source object. Legal; must not throw.
        ordinary("twoPipelinesOneSource", () -> {
            List<String> list = src();
            long a = list.stream().filter(s -> true).map(s -> s).count();
            long b = list.stream().filter(s -> true).map(s -> s).count();
            return a + "/" + b;
        });
        // The same source consumed three times in a row.
        ordinary("threePipelinesOneSource", () -> {
            List<String> list = src();
            return list.stream().sorted().toList().toString()
                    + list.stream().map(s -> s).count()
                    + list.stream().distinct().count();
        });
        // A stream held in a local across statements, then consumed ONCE.
        ordinary("streamHeldInLocal", () -> {
            Stream<String> s = src().stream();
            Stream<String> t = s.filter(x -> true);
            Stream<String> u = t.map(x -> x);
            return u.sorted().toList();
        });
        ordinary("nestedFlatMapInnerStreams", () -> src().stream()
                .flatMap(s -> Stream.of(s, s).map(String::toUpperCase))
                .sorted().toList());
        ordinary("collectionStreamOnMapEntries", () -> {
            TreeMap<String, Integer> m = new TreeMap<>();
            m.put("a", 1);
            m.put("b", 2);
            return m.entrySet().stream().filter(e -> e.getValue() > 0)
                    .map(e -> e.getKey() + "=" + e.getValue())
                    .collect(Collectors.joining(","));
        });
    }

    // ======================================================================
    // SECTION C -- the mutation controls. Each is a section-B shape with ONE
    // extra operation applied to an intermediate stage that section B leaves
    // untouched. Every row MUST throw, or section B's green is vacuous.
    // ======================================================================
    static void sectionC() {
        mutation("filterMapToList", () -> {
            Stream<String> stage = src().stream().filter(s -> s.length() > 1);
            stage.count();                                  // <-- injected
            return stage.map(String::toUpperCase).sorted().toList();
        });
        mutation("flatMapDistinctSorted", () -> {
            Stream<String> stage = src().stream().flatMap(s -> Arrays.stream(s.split("")));
            stage.distinct();                               // <-- injected
            return stage.distinct().sorted().toList();
        });
        mutation("sortedDistinctLimit", () -> {
            Stream<String> stage = src().stream().sorted();
            stage.limit(1);                                 // <-- injected
            return stage.distinct().limit(2).toList();
        });
        mutation("longChainEightOps", () -> {
            Stream<String> stage = src().stream().filter(s -> true).map(s -> s).distinct();
            stage.toList();                                 // <-- injected
            return stage.sorted().peek(s -> { }).skip(0).limit(10).toList();
        });
        mutation("reduceIdentity", () -> {
            Stream<String> stage = src().stream().sorted();
            stage.findFirst();                              // <-- injected
            return stage.reduce("", (a, b) -> a + b);
        });
        mutation("collectJoining", () -> {
            Stream<String> stage = src().stream().sorted().map(s -> s);
            stage.collect(Collectors.toList());             // <-- injected
            return stage.collect(Collectors.joining("-", "<", ">"));
        });
        mutation("iteratorDrain", () -> {
            Stream<String> stage = src().stream().sorted().map(s -> s);
            stage.iterator();                               // <-- injected
            return stage.iterator().hasNext();
        });
        mutation("spliteratorDrain", () -> {
            Stream<String> stage = src().stream().sorted().map(s -> s);
            stage.spliterator();                            // <-- injected
            return stage.spliterator().estimateSize();
        });
        mutation("parallelChain", () -> {
            Stream<String> stage = src().stream().parallel().filter(s -> true);
            stage.count();                                  // <-- injected
            return stage.map(String::toUpperCase).sorted().toList();
        });
        mutation("intStreamChain", () -> {
            IntStream stage = IntStream.rangeClosed(1, 6).filter(i -> i % 2 == 0);
            stage.sum();                                    // <-- injected
            return stage.map(i -> i * 10).sorted().boxed().toList();
        });
        mutation("longStreamChain", () -> {
            LongStream stage = LongStream.rangeClosed(1L, 6L).filter(i -> i % 2 == 0);
            stage.count();                                  // <-- injected
            return stage.map(i -> i * 10L).boxed().toList();
        });
        mutation("doubleStreamChain", () -> {
            DoubleStream stage = DoubleStream.of(3.5, 1.5, 2.5).filter(d -> d > 1.0);
            stage.sum();                                    // <-- injected
            return stage.map(d -> d * 2.0).sorted().boxed().toList();
        });
        mutation("mapToIntBoxedSorted", () -> {
            IntStream stage = src().stream().map(s -> s).mapToInt(String::length);
            stage.count();                                  // <-- injected
            return stage.boxed().sorted().toList();
        });
        mutation("streamHeldInLocal", () -> {
            Stream<String> s = src().stream();
            Stream<String> t = s.filter(x -> true);
            s.map(x -> x);                                  // <-- injected: s already linked
            return t.map(x -> x).sorted().toList();
        });
        mutation("tryWithResources", () -> {
            StringBuilder sb = new StringBuilder();
            try (Stream<String> s = src().stream()) {
                s.forEach(sb::append);                      // consumes
                s.forEach(sb::append);                      // <-- injected
            }
            return sb.toString();
        });
        mutation("collectionStreamOnMapEntries", () -> {
            TreeMap<String, Integer> m = new TreeMap<>();
            m.put("a", 1);
            var stage = m.entrySet().stream().filter(e -> e.getValue() > 0);
            stage.count();                                  // <-- injected
            return stage.map(e -> e.getKey()).collect(Collectors.joining(","));
        });
    }

    // ---- plumbing -----------------------------------------------------------
    interface Body {
        Object run() throws Throwable;
    }

    interface VoidBody {
        void run() throws Throwable;
    }

    static void ordinary(String name, Body b) {
        Object v;
        try {
            v = b.run();
        } catch (Throwable t) {
            ordinaryFailures++;
            line("ordinary." + name, "THREW:" + t.getClass().getName() + ":" + t.getMessage());
            return;
        }
        line("ordinary." + name, v);
    }

    static void mutation(String name, Body b) {
        mutationsTotal++;
        try {
            b.run();
        } catch (IllegalStateException e) {
            mutationsThatThrew++;
            line("mutation." + name, "IllegalStateException:" + e.getMessage());
            return;
        } catch (Throwable t) {
            line("mutation." + name, "OTHER:" + t.getClass().getName() + ":" + t.getMessage());
            return;
        }
        line("mutation." + name, "no-throw");
    }

    static String thrownBy(VoidBody b) {
        try {
            b.run();
        } catch (Throwable t) {
            return t.getClass().getName();
        }
        return "no-throw";
    }

    static String messageOf(VoidBody b) {
        try {
            b.run();
        } catch (Throwable t) {
            return String.valueOf(t.getMessage());
        }
        return "no-throw";
    }

    static void line(String key, Object value) {
        String v;
        if (value instanceof Optional<?> o) {
            v = o.isPresent() ? "Optional[" + o.get() + "]" : "Optional.empty";
        } else {
            v = String.valueOf(value);
        }
        System.out.println(key + "=" + v);
    }
}
