// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
import java.lang.invoke.*;
import java.nio.*;
public final class ByteBufferViewWritesProbe {
    static final Class<?>[] TYPES={short[].class,char[].class,int[].class,long[].class,float[].class,double[].class};
    static volatile long sink;
    static void write(VarHandle h, ByteBuffer b, int at, int kind, long bits) {
        switch(kind) {
            case 0: h.set(b,at,(short)bits); break;
            case 1: h.set(b,at,(char)bits); break;
            case 2: h.set(b,at,(int)bits); break;
            case 3: h.set(b,at,bits); break;
            case 4: h.set(b,at,Float.intBitsToFloat((int)bits)); break;
            case 5: h.set(b,at,Double.longBitsToDouble(bits)); break;
            default: throw new AssertionError(kind);
        }
    }
    static long decoded(ByteBuffer b,int at,int kind,boolean le) {
        int width=kind<2?2:kind==2||kind==4?4:8;
        long bits=0;
        for(int i=0;i<width;i++) bits|=(long)(b.get(at+i)&255)<<(8*(le?i:width-1-i));
        return narrowed(bits,kind);
    }
    static long narrowed(long bits,int kind) {
        return kind==0?(short)bits:kind==1?(char)bits:kind==2||kind==4?(int)bits:bits;
    }
    interface Action {void run() throws Throwable;}
    static String failure(Action a) {
        try {a.run();return "NONE";}
        catch(Throwable t) {return t.getClass().getName()+":"+t.getMessage();}
    }
    public static void main(String[] args) throws Exception {
        for(int carrier=0;carrier<6;carrier++) {
            ByteBuffer storage=(carrier&1)==0?ByteBuffer.allocate(64):ByteBuffer.allocateDirect(64);
            storage.position(7).limit(39);
            ByteBuffer data=storage.slice();
            if(carrier>=2&&carrier<4) {data.position(3).limit(31);data=data.slice();}
            if(carrier>=4) data=data.asReadOnlyBuffer();
            final ByteBuffer b=data;
            b.position(2);
            for(boolean le:new boolean[]{false,true}) for(int kind=0;kind<6;kind++) {
                final int k=kind;
                final VarHandle h=MethodHandles.byteBufferViewVarHandle(TYPES[k],le?ByteOrder.LITTLE_ENDIAN:ByteOrder.BIG_ENDIAN);
                int width=k<2?2:k==2||k==4?4:8;
                if(carrier<4) {
                    for(int n=0;n<20000;n++) {
                        int at=n%17;
                        long bits=n*0x9e3779b97f4a7c15L;
                        int before=at==0?0:b.get(at-1),after=b.get(at+width);
                        write(h,b,at,k,bits);
                        if(decoded(b,at,k,le)!=narrowed(bits,k)) throw new AssertionError("wire bits");
                        if((at>0&&before!=b.get(at-1))||after!=b.get(at+width)) throw new AssertionError("canary");
                        sink=bits;
                    }
                    for(long bits:new long[]{0x80000000L,0x7fc01234L,0x7f801234L,Long.MIN_VALUE,0x7ff8000012345678L,0x7ff0000012345678L}) {
                        write(h,b,1,k,bits);
                        if(decoded(b,1,k,le)!=narrowed(bits,k)) throw new AssertionError("sentinel bits");
                    }
                    System.gc();
                    write(h.withInvokeExactBehavior(),b,1,k,0x123456789abcdef0L);
                    if(decoded(b,1,k,le)!=narrowed(0x123456789abcdef0L,k)) throw new AssertionError("exact bits");
                } else System.out.println(carrier+"/"+k+"/"+le+"/readonly="+failure(()->write(h,b,0,k,7)));
                int saved=b.limit();b.limit(width-1);
                System.out.println(carrier+"/"+k+"/"+le+"/changed-limit="+failure(()->write(h,b,0,k,7)));
                b.limit(saved);
                System.out.println(carrier+"/"+k+"/"+le+"/negative="+failure(()->write(h,b,-1,k,7)));
                System.out.println(carrier+"/"+k+"/"+le+"/bounds="+failure(()->write(h,b,saved-width+1,k,7)));
                System.out.println(carrier+"/"+k+"/"+le+"/null="+failure(()->write(h,null,0,k,7)));
            }
            VarHandle h=MethodHandles.byteBufferViewVarHandle(int[].class,ByteOrder.BIG_ENDIAN);
            System.out.println("wrong-carrier="+failure(()->h.set(new int[8],0,7)));
            System.out.println("wrong-value="+failure(()->h.set(b,0,7L)));
            System.out.println("wrong-value-null="+failure(()->h.set((ByteBuffer)null,0,7L)));
            System.out.println("wrong-value-carrier="+failure(()->h.set(new int[8],0,7L)));
            System.out.println("wrong-value-bounds="+failure(()->h.set(b,-1,7L)));
            System.out.println("wrong-value-index-null="+failure(()->h.set(b,(Integer)null,7L)));
            System.out.println("wrong-value-volatile-null="+failure(()->h.setVolatile((ByteBuffer)null,0,7L)));
            System.out.println("wrong-value-cas-null="+failure(()->h.compareAndSet((ByteBuffer)null,0,7,8L)));
            System.out.println("wrong-value-exchange-null="+failure(()->h.compareAndExchange((ByteBuffer)null,0,7,8L)));
            System.out.println("wrong-value-getset-null="+failure(()->h.getAndSet((ByteBuffer)null,0,8L)));
            System.out.println("wrong-value-add-null="+failure(()->h.getAndAdd((ByteBuffer)null,0,8L)));
            System.out.println("wrong-value-bitwise-null="+failure(()->h.getAndBitwiseOr((ByteBuffer)null,0,8L)));
            System.out.println("boxed-index="+failure(()->h.set(b,Integer.valueOf(1),7)));
            final VarHandle exact=h.withInvokeExactBehavior();
            System.out.println("exact-object-value="+failure(()->exact.set(b,0,(Object)Integer.valueOf(7))));
        }
        System.out.println("BYTE-BUFFER-VIEW-WRITES-OK");
    }
}
