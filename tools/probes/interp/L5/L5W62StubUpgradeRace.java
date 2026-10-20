// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 62, lane L5 -- a synthetic stub upgraded in place
// WHILE another thread is initializing it must not end up "initialized"
// without its real `<clinit>`
// (`interpreter-L5-a-stub-upgrade-during-the-stubs-own-initialization-is-lost-FIXED-20261026.md`).
//
// A RACE: only `--compatible` can reach it (a constant-pool reference to a
// name under an enterprise stub prefix, `io/smallrye/`, that is absent from
// the class path resolves to a fabricated synthetic stub there; under
// `--jdk-only` and on HotSpot it is a `NoClassDefFoundError`). Per round,
// thread A runs `new <name>()` in a loop (CratonVM --compatible: it claims
// and initializes the stub) while thread B defines the REAL bytes of `<name>`
// in a fresh loader (CratonVM: the stub is upgraded IN PLACE, same
// `ClassId`); both start from one spin barrier. Then the main thread reads,
// reflectively, a static only the real `<clinit>` sets. Before wave 62 a claim
// of A that finalized after B's upgrade marked the REAL class initialized,
// and the read printed `null`. The first and the last round are printed on
// their own lines (cold-VM first call); the rounds between are counted.
//
// The race window (A's claim to its finalize) is narrow: the orchestrator
// runs this probe 20 times per mode and binary (`--compatible` default and
// `--nojit`) and diffs every run. The base can pass a run; a fix is shown
// only by a run count with no `null`, and by the unit tests named in the
// page (`vm_util::tests::a_claim_lost_to_a_stub_upgrade_*`).
//
// The bytes are this file's own `L5W62StubUpgradeRaceTargetClass` /
// `L5W62StubUpgradeRaceToucher000` with same-length names patched in.
//
// Run: javac -d out L5W62StubUpgradeRace.java
//      cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W62StubUpgradeRace
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   first=clinit-ran
//   rounds=150 clinit-ran=150
//   last=clinit-ran
//
// CratonVM `--compatible`, debug build, Windows (wave 62): the base printed
// `rounds=150 clinit-ran=149 first-other=null` in 1 of 5 runs; the fix printed
// HotSpot's lines in 15 of 15 runs (10 default, 5 `--nojit`), each run losing
// 1-7 claims to the upgrade (`CRATONVM_DBG_CLINIT_FAIL=1`: `[DBG_CLINIT_FAIL]
// io/smallrye/l5w62/RaceTargetNNN: init claim lost to a stub upgrade; retried`).

import java.io.InputStream;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.Method;
import java.nio.charset.StandardCharsets;

public class L5W62StubUpgradeRace {
    static final String TARGET = "L5W62StubUpgradeRaceTargetClass";
    static final String TOUCHER = "L5W62StubUpgradeRaceToucher000";

    static byte[] bytesOf(String name) throws Exception {
        try (InputStream in = L5W62StubUpgradeRace.class.getResourceAsStream(name + ".class")) {
            return in.readAllBytes();
        }
    }

    static void patch(byte[] b, String oldName, String newName) {
        byte[] from = oldName.getBytes(StandardCharsets.UTF_8);
        byte[] to = newName.getBytes(StandardCharsets.UTF_8);
        if (from.length != to.length) throw new IllegalStateException("length " + newName);
        outer:
        for (int i = 0; i + from.length <= b.length; i++) {
            for (int j = 0; j < from.length; j++) {
                if (b[i + j] != from[j]) continue outer;
            }
            System.arraycopy(to, 0, b, i, to.length);
        }
    }

    static final class Definer extends ClassLoader {
        Definer() { super(null); }
        Class<?> define(String name, byte[] b) { return defineClass(name, b, 0, b.length); }
    }

    static String read(Class<?> c) {
        try {
            java.lang.reflect.Field f = c.getDeclaredField("state");
            f.setAccessible(true);
            return String.valueOf(f.get(null));
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    static volatile boolean go;

    static String row(int n) throws Exception {
        String num = String.format("%03d", n);
        // 31 bytes, as TARGET.
        String internal = "io/smallrye/l5w62/RaceTarget" + num;
        byte[] toucher = bytesOf(TOUCHER);
        patch(toucher, TARGET, internal);
        patch(toucher, TOUCHER, "L5W62StubUpgradeRaceToucher" + num);
        Class<?> t = MethodHandles.lookup().defineClass(toucher);
        Method touch = t.getDeclaredMethod("touch");
        byte[] target = bytesOf(TARGET);
        patch(target, TARGET, internal);
        Class<?>[] real = new Class<?>[1];
        String[] defineError = new String[1];
        go = false;
        // A starts its `new` up to 15 ms after B starts its define, in 0.3 ms
        // steps over the rounds: on a debug CratonVM the define takes ~14 ms
        // and the stub's initialization a fraction of a millisecond, so only
        // some delays land the upgrade inside A's claim (measured, wave 62).
        long delayNanos = ((n * 7L) % 50L) * 300_000L;
        Thread a = new Thread(() -> {
            while (!go) Thread.onSpinWait();
            long until = System.nanoTime() + delayNanos;
            while (System.nanoTime() < until) Thread.onSpinWait();
            try {
                touch.invoke(null);
            } catch (Throwable ignored) {
            }
        });
        Thread b = new Thread(() -> {
            while (!go) Thread.onSpinWait();
            try {
                real[0] = new Definer().define(internal.replace('/', '.'), target);
            } catch (Throwable e) {
                defineError[0] = "define " + e.getClass().getName();
            }
        });
        a.start();
        b.start();
        go = true;
        a.join();
        b.join();
        if (defineError[0] != null) return defineError[0];
        return read(real[0]);
    }

    public static void main(String[] args) throws Exception {
        System.out.println("first=" + row(0));
        long rounds = 0;
        long ran = 0;
        String odd = null;
        for (int i = 1; i <= 150; i++) {
            String r = row(i);
            rounds++;
            if (r.equals("clinit-ran")) {
                ran++;
            } else if (odd == null) {
                odd = r;
            }
        }
        System.out.println("rounds=" + rounds + " clinit-ran=" + ran
                + (odd == null ? "" : " first-other=" + odd));
        System.out.println("last=" + row(151));
    }
}

class L5W62StubUpgradeRaceTargetClass {
    public static String state = "clinit-ran";
    public L5W62StubUpgradeRaceTargetClass() {}
}

class L5W62StubUpgradeRaceToucher000 {
    static void touch() {
        for (int i = 0; i < 64; i++) {
            try {
                new L5W62StubUpgradeRaceTargetClass();
            } catch (Throwable ignored) {
            }
        }
    }
}
