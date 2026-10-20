// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
public class I61LinkerPeerGc {
    static volatile boolean active;
    static volatile int collections;
    static volatile Throwable peerFailure;
    public static final class Payload {
        final long value;
        Payload(long value) { this.value=value; }
    }
    public static Payload target(Payload value) {
        active=true;
        long spins=0;
        while (collections<6 && peerFailure==null) {
            if (++spins==Long.MAX_VALUE) throw new AssertionError("peer GC timeout");
            if (value.value!=0x123456789L) throw new AssertionError("argument corrupted");
        }
        if (peerFailure!=null) throw new AssertionError(peerFailure);
        return value;
    }
    public static void main(String[] args) throws Throwable {
        MethodHandle mh=MethodHandles.lookup().findStatic(I61LinkerPeerGc.class,"target",
            MethodType.methodType(Payload.class,Payload.class));
        Payload value=new Payload(0x123456789L);
        collections=6;
        for (int i=0;i<20_000;i++) {
            if ((Payload)mh.invokeExact(value)!=value) throw new AssertionError("warmup identity");
        }
        collections=0; active=false;
        Thread collector=new Thread(() -> {
            try {
                while (!active) Thread.yield();
                for (int i=0;i<6;i++) { System.gc(); collections=i+1; }
            } catch (Throwable failure) { peerFailure=failure; }
        });
        collector.setDaemon(true); collector.start();
        Payload result=(Payload)mh.invokeExact(value);
        collector.join();
        if (result!=value || result.value!=0x123456789L || collections!=6)
            throw new AssertionError("linker result corrupted");
        System.out.println("peer collections=6 argument and return identity=true");
    }
}
