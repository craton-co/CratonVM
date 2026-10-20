// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 114, lane L6 -- page
// i113-orch-virtual-thread-frames-enter-only-leaf-bodies-compiled. A
// request-shaped bench: many virtual threads, each serving requests through a
// NON-leaf application handler that calls a few helpers (static, virtual,
// interface, a String hash), with one park per request. No hot loop of its
// own in any method: the handler and its helpers can only run compiled on a
// virtual thread if an interpreted frame may ENTER a non-leaf body, which the
// learned OSR door never gives them.
//
// Rows (each on virtual threads, then on platform threads):
//
//   request   512 tasks x 12 requests; `serve` (interpreted, it parks) calls
//             `Handler.handle` (non-leaf), then parks once (parkNanos 1000).
//             Under CRATONVM_VTHREAD_COMPILED_APP_ENTRY the handler is entered
//             compiled and is never under a park: no pin.
//   blocking  the same, but the park is INSIDE the handler (between parse and
//             render). Under the app-entry arm the handler's first pinned park
//             denies it, then it runs interpreted and enters its helpers
//             compiled. Shows the deny set converging (`learned_pins` small).
//
// One warm-up rep, then `reps` timed reps (arg 0, default 6). Arg 1 picks one
// row. stdout TIME lines: `TIME <row>-<kind> median=<us>` and `last=<us>`;
// per-rep lines on stderr (also `TIME `-prefixed). The checksum lines are the
// deterministic output.
//
// A/B (one binary, interleaved, medians of >= 5 runs; both modes):
//   (A) cratonvm L6W114VirtualThreadRequestBench
//   (B) CRATONVM_VTHREAD_LOCAL_CALL_COUNT=1 cratonvm L6W114VirtualThreadRequestBench
//   (C) CRATONVM_VTHREAD_NONVIRTUAL_DOORS=1 cratonvm L6W114VirtualThreadRequestBench
//   (D) CRATONVM_VTHREAD_COMPILED_APP_ENTRY=1 cratonvm L6W114VirtualThreadRequestBench
//   (E) C and D together
//   (java L6W114VirtualThreadRequestBench for HotSpot)
// Census once per arm: CRATONVM_DBG_VTHREAD_CENSUS=1 (learned_pins and
// learned_denied_methods stay a handful; yields_pinned_by_compiled plateaus).
//
// HOTSPOT_EXPECTED_BEGIN
// checksum request match=true
// checksum blocking match=true
// HOTSPOT_EXPECTED_END

import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.locks.LockSupport;

public class L6W114VirtualThreadRequestBench {
    static final class Request {
        final int id;
        final String path;
        final int[] params;

        Request(int id, String path, int[] params) {
            this.id = id;
            this.path = path;
            this.params = params;
        }
    }

    interface Route {
        long apply(Request r, long seed);
    }

    static final class SumRoute implements Route {
        @Override
        public long apply(Request r, long seed) {
            long s = seed;
            for (int p : r.params) {
                s += p;
            }
            return s;
        }
    }

    static final class HashRoute implements Route {
        @Override
        public long apply(Request r, long seed) {
            return seed ^ (r.path.hashCode() * 31L + r.id);
        }
    }

    static final class Session {
        long state;

        Session(long seed) {
            state = seed;
        }

        long touch(int v) {
            state = state * 6364136223846793005L + v;
            return state >>> 33;
        }
    }

    static final String[] PATHS = new String[8];
    static final Route[] ROUTES = {new SumRoute(), new HashRoute()};

    static {
        for (int i = 0; i < PATHS.length; i++) {
            PATHS[i] = "/api/v1/item/" + i;
        }
    }

    static Request parse(int task, int n) {
        int[] params = new int[4];
        for (int i = 0; i < params.length; i++) {
            params[i] = (task * 7 + n * 3 + i) & 0xff;
        }
        return new Request(task * 100 + n, PATHS[(task + n) & 7], params);
    }

    static boolean validate(Request r) {
        return r.path.length() > 4 && r.params.length == 4;
    }

    static long render(Request r, long value) {
        return (value ^ r.id) & 0xffffffL;
    }

    /** The non-leaf handler: helpers only, no loop of its own. */
    static long handle(Session s, int task, int n) {
        Request r = parse(task, n);
        if (!validate(r)) {
            return -1;
        }
        long v = s.touch(r.id);
        v = ROUTES[r.id & 1].apply(r, v);
        v += s.touch(r.params[0]);
        return render(r, v);
    }

    /** The same handler with its one park inside, between parse and render. */
    static long handleBlocking(Session s, int task, int n) {
        Request r = parse(task, n);
        if (!validate(r)) {
            return -1;
        }
        long v = s.touch(r.id);
        LockSupport.parkNanos(1_000);
        v = ROUTES[r.id & 1].apply(r, v);
        v += s.touch(r.params[0]);
        return render(r, v);
    }

    static final int REQUESTS = 12;

    static long serve(int task) {
        Session s = new Session(task);
        long acc = 0;
        for (int n = 0; n < REQUESTS; n++) {
            acc += handle(s, task, n);
            LockSupport.parkNanos(1_000);
        }
        return acc;
    }

    static long serveBlocking(int task) {
        Session s = new Session(task);
        long acc = 0;
        for (int n = 0; n < REQUESTS; n++) {
            acc += handleBlocking(s, task, n);
        }
        return acc;
    }

    static long row(String which, boolean virtual) throws InterruptedException {
        int tasks = 512;
        AtomicLong total = new AtomicLong();
        Thread[] ts = new Thread[tasks];
        boolean blocking = which.equals("blocking");
        for (int k = 0; k < tasks; k++) {
            final int task = k;
            Runnable r = blocking
                    ? () -> total.addAndGet(serveBlocking(task))
                    : () -> total.addAndGet(serve(task));
            ts[k] = virtual ? Thread.ofVirtual().start(r) : Thread.ofPlatform().start(r);
        }
        for (Thread t : ts) {
            t.join();
        }
        return total.get();
    }

    static long[] expected = new long[2];

    static boolean run(String which, boolean virtual, int reps) throws InterruptedException {
        String rowName = which + "-" + (virtual ? "virtual" : "platform");
        long first = row(which, virtual);
        boolean match = true;
        long[] us = new long[reps];
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            long s = row(which, virtual);
            us[r] = (System.nanoTime() - t0) / 1_000;
            match &= s == first;
            System.err.println("TIME " + rowName + " rep=" + r + " us=" + us[r]);
        }
        if (reps > 0) {
            long[] sorted = us.clone();
            java.util.Arrays.sort(sorted);
            System.out.println("TIME " + rowName + " median=" + sorted[reps / 2]);
            System.out.println("TIME " + rowName + " last=" + us[reps - 1]);
        }
        int i = which.equals("request") ? 0 : 1;
        if (expected[i] == 0) {
            expected[i] = first;
        } else {
            match &= expected[i] == first;
        }
        return match;
    }

    public static void main(String[] args) throws Exception {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 6;
        String only = args.length > 1 ? args[1] : null;
        for (String which : new String[] {"request", "blocking"}) {
            if (only != null && !only.equals(which)) {
                continue;
            }
            boolean v = run(which, true, reps);
            boolean p = run(which, false, reps);
            System.out.println("checksum " + which + " match=" + (v && p));
        }
    }
}
