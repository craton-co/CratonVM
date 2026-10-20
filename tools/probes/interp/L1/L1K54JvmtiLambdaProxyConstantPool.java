// The i54-L1 retirement: the constant pool and code of the class the VM
// mints for a lambda, against HotSpot
// (docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-does-not-know-the-vms-lambda-proxy-classes-FIXED-20261007.md,
// "Next step (from wave 57)" item 3).
//
// For ten shapes -- a static method reference, a lambda capturing a
// `String` and a `long`, an unboxing reference, a bound receiver, a
// serializable reference (twice: an intersection cast and a serializable
// functional interface), a constructor reference, an interface method
// reference, a lambda capturing a `double` and a `long`, and an
// `IntBinaryOperator` -- the shim prints `GetConstantPool` of the lambda's
// class, decoded entry by entry (the hidden-class suffix masked), and
// `GetBytecodes` of each of its methods in hex.
//
// Needs the native shim tools/probes/interp/L1/L1K54JvmtiLambdaProxyConstantPool.c,
// loaded both as an agent and with `System.load`:
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1k54cp.so tools/probes/interp/L1/L1K54JvmtiLambdaProxyConstantPool.c
//   javac -d /tmp/l1k54cp tools/probes/interp/L1/L1K54JvmtiLambdaProxyConstantPool.java
//   java     -agentpath:/tmp/libl1k54cp.so -cp /tmp/l1k54cp L1K54JvmtiLambdaProxyConstantPool /tmp/libl1k54cp.so
//   cratonvm -agentpath:/tmp/libl1k54cp.so -cp /tmp/l1k54cp L1K54JvmtiLambdaProxyConstantPool /tmp/libl1k54cp.so
//
// The Rust port of the shim is tools/probes/interp/L1/rust-ports/L1K54JvmtiLambdaProxyConstantPool.
// Expected output (HotSpot 25.0.3): at the end of the file.
import java.io.Serializable;
import java.util.ArrayList;
import java.util.List;
import java.util.function.*;

public class L1K54JvmtiLambdaProxyConstantPool {
    static native String dump(Object lambda);

    interface SerRunnable extends Runnable, Serializable {}

    static int counter;

    static void hit() {
        counter++;
    }

    static String twice(int i) {
        return "" + i + i;
    }

    public static void main(String[] args) {
        System.load(args[0]);
        String text = "x";
        long number = 42L;
        double d = 1.5;
        Runnable plain = L1K54JvmtiLambdaProxyConstantPool::hit;
        System.out.println("== plain\n" + dump(plain));
        Supplier<String> capturing = () -> text + number;
        System.out.println("== capturing\n" + dump(capturing));
        Function<Integer, String> unboxing = L1K54JvmtiLambdaProxyConstantPool::twice;
        System.out.println("== unboxing\n" + dump(unboxing));
        StringBuilder sb = new StringBuilder("b");
        Runnable bound = sb::reverse;
        System.out.println("== bound\n" + dump(bound));
        Runnable ser = (Runnable & Serializable) L1K54JvmtiLambdaProxyConstantPool::hit;
        System.out.println("== serializable\n" + dump(ser));
        SerRunnable hostile = L1K54JvmtiLambdaProxyConstantPool::hit;
        System.out.println("== hostile\n" + dump(hostile));
        Supplier<List<String>> ctor = ArrayList::new;
        System.out.println("== ctor\n" + dump(ctor));
        ToIntFunction<List<String>> iface = List::size;
        System.out.println("== iface\n" + dump(iface));
        DoubleSupplier dbl = () -> d * number;
        System.out.println("== double\n" + dump(dbl));
        IntBinaryOperator ints = (a, b) -> a + b;
        System.out.println("== ints\n" + dump(ints));
    }
}

// Expected output (HotSpot 25.0.3, MEASURED with the Rust port, default flags; the hidden
// suffix masked). CratonVM before the i54-L1 retirement: `cp err=101` and every
// constant-pool operand of the code 0:
//
//   == plain
//   cp err=0 count=19 bytes=249
//   #1 Utf8 L1K54JvmtiLambdaProxyConstantPool$$Lambda
//   #2 Class #18
//   #3 Utf8 <init>
//   #4 Utf8 ()V
//   #5 Utf8 java/lang/Object
//   #6 Class #5
//   #7 NameAndType #3 #4
//   #8 Methodref #6 #7
//   #9 Utf8 run
//   #10 Utf8 L1K54JvmtiLambdaProxyConstantPool
//   #11 Class #10
//   #12 Utf8 hit
//   #13 NameAndType #12 #4
//   #14 Methodref #11 #13
//   #15 Utf8 java/lang/Runnable
//   #16 Class #15
//   #17 Utf8 Code
//   #18 Utf8 L1K54JvmtiLambdaProxyConstantPool$$Lambda+0x?
//   method <init>()V code=2ab70008b1
//   method run()V code=b8000eb1
//   == capturing
//   cp err=0 count=30 bytes=419
//   #1 Utf8 L1K54JvmtiLambdaProxyConstantPool$$Lambda
//   #2 Class #29
//   #3 Utf8 arg$1
//   #4 Utf8 Ljava/lang/String;
//   #5 Utf8 arg$2
//   #6 Utf8 J
//   #7 Utf8 <init>
//   #8 Utf8 (Ljava/lang/String;J)V
//   #9 Utf8 java/lang/Object
//   #10 Class #9
//   #11 Utf8 ()V
//   #12 NameAndType #7 #11
//   #13 Methodref #10 #12
//   #14 NameAndType #3 #4
//   #15 Fieldref #2 #14
//   #16 NameAndType #5 #6
//   #17 Fieldref #2 #16
//   #18 Utf8 get
//   #19 Utf8 ()Ljava/lang/Object;
//   #20 Utf8 L1K54JvmtiLambdaProxyConstantPool
//   #21 Class #20
//   #22 Utf8 lambda$main$0
//   #23 Utf8 (Ljava/lang/String;J)Ljava/lang/String;
//   #24 NameAndType #22 #23
//   #25 Methodref #21 #24
//   #26 Utf8 java/util/function/Supplier
//   #27 Class #26
//   #28 Utf8 Code
//   #29 Utf8 L1K54JvmtiLambdaProxyConstantPool$$Lambda+0x?
//   method <init>(Ljava/lang/String;J)V code=2ab7000d2a2bb5000f2a20b50011b1
//   method get()Ljava/lang/Object; code=2ab4000f2ab40011b80019b0
//   == unboxing
//   cp err=0 count=29 bytes=399
//   #1 Utf8 L1K54JvmtiLambdaProxyConstantPool$$Lambda
//   #2 Class #28
//   #3 Utf8 <init>
//   #4 Utf8 ()V
//   #5 Utf8 java/lang/Object
//   #6 Class #5
//   #7 NameAndType #3 #4
//   #8 Methodref #6 #7
//   #9 Utf8 apply
//   #10 Utf8 (Ljava/lang/Object;)Ljava/lang/Object;
//   #11 Utf8 java/lang/Integer
//   #12 Class #11
//   #13 Utf8 java/lang/Number
//   #14 Class #13
//   #15 Utf8 intValue
//   #16 Utf8 ()I
//   #17 NameAndType #15 #16
//   #18 Methodref #14 #17
//   #19 Utf8 L1K54JvmtiLambdaProxyConstantPool
//   #20 Class #19
//   #21 Utf8 twice
//   #22 Utf8 (I)Ljava/lang/String;
//   #23 NameAndType #21 #22
//   #24 Methodref #20 #23
//   #25 Utf8 java/util/function/Function
//   #26 Class #25
//   #27 Utf8 Code
//   #28 Utf8 L1K54JvmtiLambdaProxyConstantPool$$Lambda+0x?
//   method <init>()V code=2ab70008b1
//   method apply(Ljava/lang/Object;)Ljava/lang/Object; code=2bc0000cb60012b80018b0
//   == bound
//   cp err=0 count=25 bytes=350
//   #1 Utf8 L1K54JvmtiLambdaProxyConstantPool$$Lambda
//   #2 Class #24
//   #3 Utf8 arg$1
//   #4 Utf8 Ljava/lang/StringBuilder;
//   #5 Utf8 <init>
//   #6 Utf8 (Ljava/lang/StringBuilder;)V
//   #7 Utf8 java/lang/Object
//   #8 Class #7
//   #9 Utf8 ()V
//   #10 NameAndType #5 #9
//   #11 Methodref #8 #10
//   #12 NameAndType #3 #4
//   #13 Fieldref #2 #12
//   #14 Utf8 run
//   #15 Utf8 java/lang/StringBuilder
//   #16 Class #15
//   #17 Utf8 reverse
//   #18 Utf8 ()Ljava/lang/StringBuilder;
//   #19 NameAndType #17 #18
//   #20 Methodref #16 #19
//   #21 Utf8 java/lang/Runnable
//   #22 Class #21
//   #23 Utf8 Code
//   #24 Utf8 L1K54JvmtiLambdaProxyConstantPool$$Lambda+0x?
//   method <init>(Ljava/lang/StringBuilder;)V code=2ab7000b2a2bb5000db1
//   method run()V code=2ab4000db60014b1
//   == serializable
//   cp err=0 count=34 bytes=551
//   #1 Utf8 L1K54JvmtiLambdaProxyConstantPool$$Lambda
//   #2 Class #33
//   #3 Utf8 <init>
//   #4 Utf8 ()V
//   #5 Utf8 java/lang/Object
//   #6 Class #5
//   #7 NameAndType #3 #4
//   #8 Methodref #6 #7
//   #9 Utf8 run
//   #10 Utf8 L1K54JvmtiLambdaProxyConstantPool
//   #11 Class #10
//   #12 Utf8 hit
//   #13 NameAndType #12 #4
//   #14 Methodref #11 #13
//   #15 Utf8 writeReplace
//   #16 Utf8 ()Ljava/lang/Object;
//   #17 Utf8 java/lang/invoke/SerializedLambda
//   #18 Class #17
//   #19 Utf8 java/lang/Runnable
//   #20 String #19
//   #21 String #9
//   #22 String #4
//   #23 Integer 6
//   #24 String #10
//   #25 String #12
//   #26 Utf8 (Ljava/lang/Class;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;ILjava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;[Ljava/lang/Object;)V
//   #27 NameAndType #3 #26
//   #28 Methodref #18 #27
//   #29 Class #19
//   #30 Utf8 java/io/Serializable
//   #31 Class #30
//   #32 Utf8 Code
//   #33 Utf8 L1K54JvmtiLambdaProxyConstantPool$$Lambda+0x?
//   method <init>()V code=2ab70008b1
//   method run()V code=b8000eb1
//   method writeReplace()Ljava/lang/Object; code=bb001259120b1214121512161217121812191216121603bd0006b7001cb0
//   == hostile
//   cp err=0 count=32 bytes=552
//   #1 Utf8 L1K54JvmtiLambdaProxyConstantPool$$Lambda
//   #2 Class #31
//   #3 Utf8 <init>
//   #4 Utf8 ()V
//   #5 Utf8 java/lang/Object
//   #6 Class #5
//   #7 NameAndType #3 #4
//   #8 Methodref #6 #7
//   #9 Utf8 run
//   #10 Utf8 L1K54JvmtiLambdaProxyConstantPool
//   #11 Class #10
//   #12 Utf8 hit
//   #13 NameAndType #12 #4
//   #14 Methodref #11 #13
//   #15 Utf8 writeReplace
//   #16 Utf8 ()Ljava/lang/Object;
//   #17 Utf8 java/lang/invoke/SerializedLambda
//   #18 Class #17
//   #19 Utf8 L1K54JvmtiLambdaProxyConstantPool$SerRunnable
//   #20 String #19
//   #21 String #9
//   #22 String #4
//   #23 Integer 6
//   #24 String #10
//   #25 String #12
//   #26 Utf8 (Ljava/lang/Class;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;ILjava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;[Ljava/lang/Object;)V
//   #27 NameAndType #3 #26
//   #28 Methodref #18 #27
//   #29 Class #19
//   #30 Utf8 Code
//   #31 Utf8 L1K54JvmtiLambdaProxyConstantPool$$Lambda+0x?
//   method <init>()V code=2ab70008b1
//   method run()V code=b8000eb1
//   method writeReplace()Ljava/lang/Object; code=bb001259120b1214121512161217121812191216121603bd0006b7001cb0
//   == ctor
//   cp err=0 count=18 bytes=256
//   #1 Utf8 L1K54JvmtiLambdaProxyConstantPool$$Lambda
//   #2 Class #17
//   #3 Utf8 <init>
//   #4 Utf8 ()V
//   #5 Utf8 java/lang/Object
//   #6 Class #5
//   #7 NameAndType #3 #4
//   #8 Methodref #6 #7
//   #9 Utf8 get
//   #10 Utf8 ()Ljava/lang/Object;
//   #11 Utf8 java/util/ArrayList
//   #12 Class #11
//   #13 Methodref #12 #7
//   #14 Utf8 java/util/function/Supplier
//   #15 Class #14
//   #16 Utf8 Code
//   #17 Utf8 L1K54JvmtiLambdaProxyConstantPool$$Lambda+0x?
//   method <init>()V code=2ab70008b1
//   method get()Ljava/lang/Object; code=bb000c59b7000db0
//   == iface
//   cp err=0 count=21 bytes=282
//   #1 Utf8 L1K54JvmtiLambdaProxyConstantPool$$Lambda
//   #2 Class #20
//   #3 Utf8 <init>
//   #4 Utf8 ()V
//   #5 Utf8 java/lang/Object
//   #6 Class #5
//   #7 NameAndType #3 #4
//   #8 Methodref #6 #7
//   #9 Utf8 applyAsInt
//   #10 Utf8 (Ljava/lang/Object;)I
//   #11 Utf8 java/util/List
//   #12 Class #11
//   #13 Utf8 size
//   #14 Utf8 ()I
//   #15 NameAndType #13 #14
//   #16 InterfaceMethodref #12 #15
//   #17 Utf8 java/util/function/ToIntFunction
//   #18 Class #17
//   #19 Utf8 Code
//   #20 Utf8 L1K54JvmtiLambdaProxyConstantPool$$Lambda+0x?
//   method <init>()V code=2ab70008b1
//   method applyAsInt(Ljava/lang/Object;)I code=2bc0000cb900100100ac
//   == double
//   cp err=0 count=30 bytes=348
//   #1 Utf8 L1K54JvmtiLambdaProxyConstantPool$$Lambda
//   #2 Class #29
//   #3 Utf8 arg$1
//   #4 Utf8 D
//   #5 Utf8 arg$2
//   #6 Utf8 J
//   #7 Utf8 <init>
//   #8 Utf8 (DJ)V
//   #9 Utf8 java/lang/Object
//   #10 Class #9
//   #11 Utf8 ()V
//   #12 NameAndType #7 #11
//   #13 Methodref #10 #12
//   #14 NameAndType #3 #4
//   #15 Fieldref #2 #14
//   #16 NameAndType #5 #6
//   #17 Fieldref #2 #16
//   #18 Utf8 getAsDouble
//   #19 Utf8 ()D
//   #20 Utf8 L1K54JvmtiLambdaProxyConstantPool
//   #21 Class #20
//   #22 Utf8 lambda$main$1
//   #23 Utf8 (DJ)D
//   #24 NameAndType #22 #23
//   #25 Methodref #21 #24
//   #26 Utf8 java/util/function/DoubleSupplier
//   #27 Class #26
//   #28 Utf8 Code
//   #29 Utf8 L1K54JvmtiLambdaProxyConstantPool$$Lambda+0x?
//   method <init>(DJ)V code=2ab7000d2a27b5000f2a21b50011b1
//   method getAsDouble()D code=2ab4000f2ab40011b80019af
//   == ints
//   cp err=0 count=20 bytes=292
//   #1 Utf8 L1K54JvmtiLambdaProxyConstantPool$$Lambda
//   #2 Class #19
//   #3 Utf8 <init>
//   #4 Utf8 ()V
//   #5 Utf8 java/lang/Object
//   #6 Class #5
//   #7 NameAndType #3 #4
//   #8 Methodref #6 #7
//   #9 Utf8 applyAsInt
//   #10 Utf8 (II)I
//   #11 Utf8 L1K54JvmtiLambdaProxyConstantPool
//   #12 Class #11
//   #13 Utf8 lambda$main$2
//   #14 NameAndType #13 #10
//   #15 Methodref #12 #14
//   #16 Utf8 java/util/function/IntBinaryOperator
//   #17 Class #16
//   #18 Utf8 Code
//   #19 Utf8 L1K54JvmtiLambdaProxyConstantPool$$Lambda+0x?
//   method <init>()V code=2ab70008b1
//   method applyAsInt(II)I code=1b1cb8000fac
