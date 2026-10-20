// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 123, lane L4 (`CRATONVM_VH_REAL_VARFORM`, default
// off, `--compatible` only; proposal
// docs/internal/proposals/interpreter/i57-L4-proposal-mint-varhandles-with-a-real-varform-20261021.md,
// stages 1 and 2): a field `VarHandle` the VM mints carries a real `VarForm`
// over an embedded `cratonvm/internal/VhForm*` class, and the JDK's own
// `isAccessModeSupported` / `toMethodHandle` / `getMethodHandleUncached` run
// over it. Every row is about what those bodies answer: the supported-mode
// mask of every access mode, the `type()` of `toMethodHandle(mode)` and what
// it does when invoked (including an unsupported mode's bound invoker), an
// exact handle's, the generic and exact `varHandleInvoker`, `revealDirect`,
// and a warm loop through one `toMethodHandle` result.
//
// Every row must be SAME with the switch on and off, in both modes, JIT and
// `--nojit`. `CRATONVM_DBG_MH_DISPATCH=1` with the switch on prints
// `[VH_REAL_VARFORM] ... VarForm over cratonvm/internal/VhForm...` per minted
// handle in `--compatible` (none under `--jdk-only`: the JDK's factories run
// there).
//
// Run: javac -d . L4W123RealVarForm.java
//      java -cp . L4W123RealVarForm                                   (HotSpot)
//      cratonvm --jdk-only|--compatible [--nojit] -cp . L4W123RealVarForm
//      CRATONVM_VH_REAL_VARFORM=1 cratonvm --compatible [--nojit] -cp . L4W123RealVarForm
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags; -Xint identical)
// mask i 1111111111111111111111111111111
// vh i VarHandle[varType=int, coord=[class L4W123RealVarForm$Box]] GET=(Box)int
// tmh i GET (Box)int -> Integer:7
// tmh i GET_VOLATILE (Box)int -> Integer:7
// tmh i SET (Box,int)void -> null
// tmh i GET_ACQUIRE (Box)int -> Integer:8
// tmh i COMPARE_AND_SET (Box,int,int)boolean -> Boolean:true
// tmh i COMPARE_AND_EXCHANGE (Box,int,int)int -> Integer:9
// tmh i GET_AND_SET (Box,int)int -> Integer:8
// tmh i WEAK_COMPARE_AND_SET_PLAIN (Box,int,int)boolean -> Boolean:false
// tmh i GET_AND_ADD (Box,int)int -> Integer:9
// tmh i GET_AND_BITWISE_OR (Box,int)int -> Integer:17
// tmh i GET_AND_BITWISE_XOR_RELEASE (Box,int)int -> Integer:25
// tmh i GET (Box)int -> Integer:17
// invoker i (VarHandle,Box)int -> Integer:17
// exact-invoker i (VarHandle,Box)int -> Integer:17
// exact-mask i 1111111111111111111111111111111 exact=true
// tmh i-exact GET (Box)int -> Integer:17
// reveal i java.lang.IllegalArgumentException: not a direct method handle
// mask j 1111111111111111111111111111111
// vh j VarHandle[varType=long, coord=[class L4W123RealVarForm$Box]] GET=(Box)long
// tmh j GET (Box)long -> Long:70
// tmh j GET_VOLATILE (Box)long -> Long:70
// tmh j SET (Box,long)void -> null
// tmh j GET_ACQUIRE (Box)long -> Long:80
// tmh j COMPARE_AND_SET (Box,long,long)boolean -> Boolean:true
// tmh j COMPARE_AND_EXCHANGE (Box,long,long)long -> Long:90
// tmh j GET_AND_SET (Box,long)long -> Long:80
// tmh j WEAK_COMPARE_AND_SET_PLAIN (Box,long,long)boolean -> Boolean:false
// tmh j GET_AND_ADD (Box,long)long -> Long:90
// tmh j GET_AND_BITWISE_OR (Box,long)long -> Long:170
// tmh j GET_AND_BITWISE_XOR_RELEASE (Box,long)long -> Long:250
// tmh j GET (Box)long -> Long:170
// invoker j (VarHandle,Box)long -> Long:170
// exact-invoker j (VarHandle,Box)long -> Long:170
// exact-mask j 1111111111111111111111111111111 exact=true
// tmh j-exact GET (Box)long -> Long:170
// reveal j java.lang.IllegalArgumentException: not a direct method handle
// mask z 1111111111111111111000111111111
// vh z VarHandle[varType=boolean, coord=[class L4W123RealVarForm$Box]] GET=(Box)boolean
// tmh z GET (Box)boolean -> Boolean:true
// tmh z GET_VOLATILE (Box)boolean -> Boolean:true
// tmh z SET (Box,boolean)void -> null
// tmh z GET_ACQUIRE (Box)boolean -> Boolean:false
// tmh z COMPARE_AND_SET (Box,boolean,boolean)boolean -> Boolean:true
// tmh z COMPARE_AND_EXCHANGE (Box,boolean,boolean)boolean -> Boolean:true
// tmh z GET_AND_SET (Box,boolean)boolean -> Boolean:false
// tmh z WEAK_COMPARE_AND_SET_PLAIN (Box,boolean,boolean)boolean -> Boolean:false
// tmh z GET_AND_ADD (Box,boolean)boolean -> threw java.lang.UnsupportedOperationException: getAndAdd
// tmh z GET_AND_BITWISE_OR (Box,boolean)boolean -> Boolean:true
// tmh z GET_AND_BITWISE_XOR_RELEASE (Box,boolean)boolean -> Boolean:true
// tmh z GET (Box)boolean -> Boolean:true
// invoker z (VarHandle,Box)boolean -> Boolean:true
// exact-invoker z (VarHandle,Box)boolean -> Boolean:true
// exact-mask z 1111111111111111111000111111111 exact=true
// tmh z-exact GET (Box)boolean -> Boolean:true
// reveal z java.lang.IllegalArgumentException: not a direct method handle
// mask b 1111111111111111111111111111111
// vh b VarHandle[varType=byte, coord=[class L4W123RealVarForm$Box]] GET=(Box)byte
// tmh b GET (Box)byte -> Byte:3
// tmh b GET_VOLATILE (Box)byte -> Byte:3
// tmh b SET (Box,byte)void -> null
// tmh b GET_ACQUIRE (Box)byte -> Byte:4
// tmh b COMPARE_AND_SET (Box,byte,byte)boolean -> Boolean:true
// tmh b COMPARE_AND_EXCHANGE (Box,byte,byte)byte -> Byte:6
// tmh b GET_AND_SET (Box,byte)byte -> Byte:4
// tmh b WEAK_COMPARE_AND_SET_PLAIN (Box,byte,byte)boolean -> Boolean:false
// tmh b GET_AND_ADD (Box,byte)byte -> Byte:6
// tmh b GET_AND_BITWISE_OR (Box,byte)byte -> Byte:10
// tmh b GET_AND_BITWISE_XOR_RELEASE (Box,byte)byte -> Byte:14
// tmh b GET (Box)byte -> Byte:10
// invoker b (VarHandle,Box)byte -> Byte:10
// exact-invoker b (VarHandle,Box)byte -> Byte:10
// exact-mask b 1111111111111111111111111111111 exact=true
// tmh b-exact GET (Box)byte -> Byte:10
// reveal b java.lang.IllegalArgumentException: not a direct method handle
// mask sh 1111111111111111111111111111111
// vh sh VarHandle[varType=short, coord=[class L4W123RealVarForm$Box]] GET=(Box)short
// tmh sh GET (Box)short -> Short:5
// tmh sh GET_VOLATILE (Box)short -> Short:5
// tmh sh SET (Box,short)void -> null
// tmh sh GET_ACQUIRE (Box)short -> Short:6
// tmh sh COMPARE_AND_SET (Box,short,short)boolean -> Boolean:true
// tmh sh COMPARE_AND_EXCHANGE (Box,short,short)short -> Short:12
// tmh sh GET_AND_SET (Box,short)short -> Short:6
// tmh sh WEAK_COMPARE_AND_SET_PLAIN (Box,short,short)boolean -> Boolean:false
// tmh sh GET_AND_ADD (Box,short)short -> Short:12
// tmh sh GET_AND_BITWISE_OR (Box,short)short -> Short:18
// tmh sh GET_AND_BITWISE_XOR_RELEASE (Box,short)short -> Short:22
// tmh sh GET (Box)short -> Short:16
// invoker sh (VarHandle,Box)short -> Short:16
// exact-invoker sh (VarHandle,Box)short -> Short:16
// exact-mask sh 1111111111111111111111111111111 exact=true
// tmh sh-exact GET (Box)short -> Short:16
// reveal sh java.lang.IllegalArgumentException: not a direct method handle
// mask c 1111111111111111111111111111111
// vh c VarHandle[varType=char, coord=[class L4W123RealVarForm$Box]] GET=(Box)char
// tmh c GET (Box)char -> 'q'
// tmh c GET_VOLATILE (Box)char -> 'q'
// tmh c SET (Box,char)void -> null
// tmh c GET_ACQUIRE (Box)char -> 'r'
// tmh c COMPARE_AND_SET (Box,char,char)boolean -> Boolean:true
// tmh c COMPARE_AND_EXCHANGE (Box,char,char)char -> 's'
// tmh c GET_AND_SET (Box,char)char -> 'r'
// tmh c WEAK_COMPARE_AND_SET_PLAIN (Box,char,char)boolean -> Boolean:false
// tmh c GET_AND_ADD (Box,char)char -> 's'
// tmh c GET_AND_BITWISE_OR (Box,char)char -> '?'
// tmh c GET_AND_BITWISE_XOR_RELEASE (Box,char)char -> '?'
// tmh c GET (Box)char -> '?'
// invoker c (VarHandle,Box)char -> '?'
// exact-invoker c (VarHandle,Box)char -> '?'
// exact-mask c 1111111111111111111111111111111 exact=true
// tmh c-exact GET (Box)char -> '?'
// reveal c java.lang.IllegalArgumentException: not a direct method handle
// mask f 1111111111111111111111000000000
// vh f VarHandle[varType=float, coord=[class L4W123RealVarForm$Box]] GET=(Box)float
// tmh f GET (Box)float -> Float:1.5
// tmh f GET_VOLATILE (Box)float -> Float:1.5
// tmh f SET (Box,float)void -> null
// tmh f GET_ACQUIRE (Box)float -> Float:2.5
// tmh f COMPARE_AND_SET (Box,float,float)boolean -> Boolean:true
// tmh f COMPARE_AND_EXCHANGE (Box,float,float)float -> Float:4.0
// tmh f GET_AND_SET (Box,float)float -> Float:2.5
// tmh f WEAK_COMPARE_AND_SET_PLAIN (Box,float,float)boolean -> Boolean:false
// tmh f GET_AND_ADD (Box,float)float -> Float:4.0
// tmh f GET_AND_BITWISE_OR (Box,float)float -> threw java.lang.UnsupportedOperationException: getAndBitwiseOr
// tmh f GET_AND_BITWISE_XOR_RELEASE (Box,float)float -> threw java.lang.UnsupportedOperationException: getAndBitwiseXorRelease
// tmh f GET (Box)float -> Float:6.5
// invoker f (VarHandle,Box)float -> Float:6.5
// exact-invoker f (VarHandle,Box)float -> Float:6.5
// exact-mask f 1111111111111111111111000000000 exact=true
// tmh f-exact GET (Box)float -> Float:6.5
// reveal f java.lang.IllegalArgumentException: not a direct method handle
// mask d 1111111111111111111111000000000
// vh d VarHandle[varType=double, coord=[class L4W123RealVarForm$Box]] GET=(Box)double
// tmh d GET (Box)double -> Double:2.5
// tmh d GET_VOLATILE (Box)double -> Double:2.5
// tmh d SET (Box,double)void -> null
// tmh d GET_ACQUIRE (Box)double -> Double:3.5
// tmh d COMPARE_AND_SET (Box,double,double)boolean -> Boolean:true
// tmh d COMPARE_AND_EXCHANGE (Box,double,double)double -> Double:7.0
// tmh d GET_AND_SET (Box,double)double -> Double:3.5
// tmh d WEAK_COMPARE_AND_SET_PLAIN (Box,double,double)boolean -> Boolean:false
// tmh d GET_AND_ADD (Box,double)double -> Double:7.0
// tmh d GET_AND_BITWISE_OR (Box,double)double -> threw java.lang.UnsupportedOperationException: getAndBitwiseOr
// tmh d GET_AND_BITWISE_XOR_RELEASE (Box,double)double -> threw java.lang.UnsupportedOperationException: getAndBitwiseXorRelease
// tmh d GET (Box)double -> Double:10.5
// invoker d (VarHandle,Box)double -> Double:10.5
// exact-invoker d (VarHandle,Box)double -> Double:10.5
// exact-mask d 1111111111111111111111000000000 exact=true
// tmh d-exact GET (Box)double -> Double:10.5
// reveal d java.lang.IllegalArgumentException: not a direct method handle
// mask s 1111111111111111111000000000000
// vh s VarHandle[varType=java.lang.String, coord=[class L4W123RealVarForm$Box]] GET=(Box)String
// tmh s GET (Box)String -> String:s0
// tmh s GET_VOLATILE (Box)String -> String:s0
// tmh s SET (Box,String)void -> null
// tmh s GET_ACQUIRE (Box)String -> String:s1
// tmh s COMPARE_AND_SET (Box,String,String)boolean -> Boolean:true
// tmh s COMPARE_AND_EXCHANGE (Box,String,String)String -> String:s2
// tmh s GET_AND_SET (Box,String)String -> String:s1
// tmh s WEAK_COMPARE_AND_SET_PLAIN (Box,String,String)boolean -> Boolean:false
// tmh s GET_AND_ADD (Box,String)String -> threw java.lang.UnsupportedOperationException: getAndAdd
// tmh s GET_AND_BITWISE_OR (Box,String)String -> threw java.lang.UnsupportedOperationException: getAndBitwiseOr
// tmh s GET_AND_BITWISE_XOR_RELEASE (Box,String)String -> threw java.lang.UnsupportedOperationException: getAndBitwiseXorRelease
// tmh s GET (Box)String -> String:s2
// invoker s (VarHandle,Box)String -> String:s2
// exact-invoker s (VarHandle,Box)String -> String:s2
// exact-mask s 1111111111111111111000000000000 exact=true
// tmh s-exact GET (Box)String -> String:s2
// reveal s java.lang.IllegalArgumentException: not a direct method handle
// mask fi 1010101000000000000000000000000
// vh fi VarHandle[varType=int, coord=[class L4W123RealVarForm$Box]] GET=(Box)int
// tmh fi GET (Box)int -> Integer:11
// tmh fi GET_VOLATILE (Box)int -> Integer:11
// tmh fi SET (Box,int)void -> threw java.lang.UnsupportedOperationException: set
// tmh fi GET_ACQUIRE (Box)int -> Integer:11
// tmh fi COMPARE_AND_SET (Box,int,int)boolean -> threw java.lang.UnsupportedOperationException: compareAndSet
// tmh fi COMPARE_AND_EXCHANGE (Box,int,int)int -> threw java.lang.UnsupportedOperationException: compareAndExchange
// tmh fi GET_AND_SET (Box,int)int -> threw java.lang.UnsupportedOperationException: getAndSet
// tmh fi WEAK_COMPARE_AND_SET_PLAIN (Box,int,int)boolean -> threw java.lang.UnsupportedOperationException: weakCompareAndSetPlain
// tmh fi GET_AND_ADD (Box,int)int -> threw java.lang.UnsupportedOperationException: getAndAdd
// tmh fi GET_AND_BITWISE_OR (Box,int)int -> threw java.lang.UnsupportedOperationException: getAndBitwiseOr
// tmh fi GET_AND_BITWISE_XOR_RELEASE (Box,int)int -> threw java.lang.UnsupportedOperationException: getAndBitwiseXorRelease
// tmh fi GET (Box)int -> Integer:11
// invoker fi (VarHandle,Box)int -> Integer:11
// exact-invoker fi (VarHandle,Box)int -> Integer:11
// exact-mask fi 1010101000000000000000000000000 exact=true
// tmh fi-exact GET (Box)int -> Integer:11
// reveal fi java.lang.IllegalArgumentException: not a direct method handle
// mask fs 1010101000000000000000000000000
// vh fs VarHandle[varType=java.lang.String, coord=[class L4W123RealVarForm$Box]] GET=(Box)String
// tmh fs GET (Box)String -> String:fixed
// tmh fs GET_VOLATILE (Box)String -> String:fixed
// tmh fs SET (Box,String)void -> threw java.lang.UnsupportedOperationException: set
// tmh fs GET_ACQUIRE (Box)String -> String:fixed
// tmh fs COMPARE_AND_SET (Box,String,String)boolean -> threw java.lang.UnsupportedOperationException: compareAndSet
// tmh fs COMPARE_AND_EXCHANGE (Box,String,String)String -> threw java.lang.UnsupportedOperationException: compareAndExchange
// tmh fs GET_AND_SET (Box,String)String -> threw java.lang.UnsupportedOperationException: getAndSet
// tmh fs WEAK_COMPARE_AND_SET_PLAIN (Box,String,String)boolean -> threw java.lang.UnsupportedOperationException: weakCompareAndSetPlain
// tmh fs GET_AND_ADD (Box,String)String -> threw java.lang.UnsupportedOperationException: getAndAdd
// tmh fs GET_AND_BITWISE_OR (Box,String)String -> threw java.lang.UnsupportedOperationException: getAndBitwiseOr
// tmh fs GET_AND_BITWISE_XOR_RELEASE (Box,String)String -> threw java.lang.UnsupportedOperationException: getAndBitwiseXorRelease
// tmh fs GET (Box)String -> String:fixed
// invoker fs (VarHandle,Box)String -> String:fixed
// exact-invoker fs (VarHandle,Box)String -> String:fixed
// exact-mask fs 1010101000000000000000000000000 exact=true
// tmh fs-exact GET (Box)String -> String:fixed
// reveal fs java.lang.IllegalArgumentException: not a direct method handle
// mask SI 1111111111111111111111111111111
// vh SI VarHandle[varType=int, coord=[]] GET=()int
// tmh SI GET ()int -> Integer:100
// tmh SI GET_VOLATILE ()int -> Integer:100
// tmh SI SET (int)void -> null
// tmh SI GET_ACQUIRE ()int -> Integer:101
// tmh SI COMPARE_AND_SET (int,int)boolean -> Boolean:true
// tmh SI COMPARE_AND_EXCHANGE (int,int)int -> Integer:102
// tmh SI GET_AND_SET (int)int -> Integer:101
// tmh SI WEAK_COMPARE_AND_SET_PLAIN (int,int)boolean -> Boolean:false
// tmh SI GET_AND_ADD (int)int -> Integer:102
// tmh SI GET_AND_BITWISE_OR (int)int -> Integer:203
// tmh SI GET_AND_BITWISE_XOR_RELEASE (int)int -> Integer:239
// tmh SI GET ()int -> Integer:138
// invoker SI (VarHandle)int -> Integer:138
// exact-invoker SI (VarHandle)int -> Integer:138
// exact-mask SI 1111111111111111111111111111111 exact=true
// tmh SI-exact GET ()int -> Integer:138
// reveal SI java.lang.IllegalArgumentException: not a direct method handle
// mask SO 1111111111111111111000000000000
// vh SO VarHandle[varType=java.lang.Object, coord=[]] GET=()Object
// tmh SO GET ()Object -> String:so0
// tmh SO GET_VOLATILE ()Object -> String:so0
// tmh SO SET (Object)void -> null
// tmh SO GET_ACQUIRE ()Object -> String:so1
// tmh SO COMPARE_AND_SET (Object,Object)boolean -> Boolean:true
// tmh SO COMPARE_AND_EXCHANGE (Object,Object)Object -> String:so2
// tmh SO GET_AND_SET (Object)Object -> String:so1
// tmh SO WEAK_COMPARE_AND_SET_PLAIN (Object,Object)boolean -> Boolean:false
// tmh SO GET_AND_ADD (Object)Object -> threw java.lang.UnsupportedOperationException: getAndAdd
// tmh SO GET_AND_BITWISE_OR (Object)Object -> threw java.lang.UnsupportedOperationException: getAndBitwiseOr
// tmh SO GET_AND_BITWISE_XOR_RELEASE (Object)Object -> threw java.lang.UnsupportedOperationException: getAndBitwiseXorRelease
// tmh SO GET ()Object -> String:so2
// invoker SO (VarHandle)Object -> String:so2
// exact-invoker SO (VarHandle)Object -> String:so2
// exact-mask SO 1111111111111111111000000000000 exact=true
// tmh SO-exact GET ()Object -> String:so2
// reveal SO java.lang.IllegalArgumentException: not a direct method handle
// mask SF 1010101000000000000000000000000
// vh SF VarHandle[varType=java.lang.String, coord=[]] GET=()String
// tmh SF GET ()String -> String:sf
// tmh SF GET_VOLATILE ()String -> String:sf
// tmh SF SET (String)void -> threw java.lang.UnsupportedOperationException: set
// tmh SF GET_ACQUIRE ()String -> String:sf
// tmh SF COMPARE_AND_SET (String,String)boolean -> threw java.lang.UnsupportedOperationException: compareAndSet
// tmh SF COMPARE_AND_EXCHANGE (String,String)String -> threw java.lang.UnsupportedOperationException: compareAndExchange
// tmh SF GET_AND_SET (String)String -> threw java.lang.UnsupportedOperationException: getAndSet
// tmh SF WEAK_COMPARE_AND_SET_PLAIN (String,String)boolean -> threw java.lang.UnsupportedOperationException: weakCompareAndSetPlain
// tmh SF GET_AND_ADD (String)String -> threw java.lang.UnsupportedOperationException: getAndAdd
// tmh SF GET_AND_BITWISE_OR (String)String -> threw java.lang.UnsupportedOperationException: getAndBitwiseOr
// tmh SF GET_AND_BITWISE_XOR_RELEASE (String)String -> threw java.lang.UnsupportedOperationException: getAndBitwiseXorRelease
// tmh SF GET ()String -> String:sf
// invoker SF (VarHandle)String -> String:sf
// exact-invoker SF (VarHandle)String -> String:sf
// exact-mask SF 1010101000000000000000000000000 exact=true
// tmh SF-exact GET ()String -> String:sf
// reveal SF java.lang.IllegalArgumentException: not a direct method handle
// mask SJ 1010101000000000000000000000000
// vh SJ VarHandle[varType=long, coord=[]] GET=()long
// tmh SJ GET ()long -> Long:9
// tmh SJ GET_VOLATILE ()long -> Long:9
// tmh SJ SET (long)void -> threw java.lang.UnsupportedOperationException: set
// tmh SJ GET_ACQUIRE ()long -> Long:9
// tmh SJ COMPARE_AND_SET (long,long)boolean -> threw java.lang.UnsupportedOperationException: compareAndSet
// tmh SJ COMPARE_AND_EXCHANGE (long,long)long -> threw java.lang.UnsupportedOperationException: compareAndExchange
// tmh SJ GET_AND_SET (long)long -> threw java.lang.UnsupportedOperationException: getAndSet
// tmh SJ WEAK_COMPARE_AND_SET_PLAIN (long,long)boolean -> threw java.lang.UnsupportedOperationException: weakCompareAndSetPlain
// tmh SJ GET_AND_ADD (long)long -> threw java.lang.UnsupportedOperationException: getAndAdd
// tmh SJ GET_AND_BITWISE_OR (long)long -> threw java.lang.UnsupportedOperationException: getAndBitwiseOr
// tmh SJ GET_AND_BITWISE_XOR_RELEASE (long)long -> threw java.lang.UnsupportedOperationException: getAndBitwiseXorRelease
// tmh SJ GET ()long -> Long:9
// invoker SJ (VarHandle)long -> Long:9
// exact-invoker SJ (VarHandle)long -> Long:9
// exact-mask SJ 1010101000000000000000000000000 exact=true
// tmh SJ-exact GET ()long -> Long:9
// reveal SJ java.lang.IllegalArgumentException: not a direct method handle
// mask unreflect-i 1111111111111111111111111111111
// vh unreflect-i VarHandle[varType=int, coord=[class L4W123RealVarForm$Box]] GET=(Box)int
// tmh unreflect-i GET (Box)int -> Integer:17
// tmh unreflect-i GET_VOLATILE (Box)int -> Integer:17
// tmh unreflect-i SET (Box,int)void -> null
// tmh unreflect-i GET_ACQUIRE (Box)int -> Integer:21
// tmh unreflect-i COMPARE_AND_SET (Box,int,int)boolean -> Boolean:true
// tmh unreflect-i COMPARE_AND_EXCHANGE (Box,int,int)int -> Integer:22
// tmh unreflect-i GET_AND_SET (Box,int)int -> Integer:21
// tmh unreflect-i WEAK_COMPARE_AND_SET_PLAIN (Box,int,int)boolean -> Boolean:false
// tmh unreflect-i GET_AND_ADD (Box,int)int -> Integer:22
// tmh unreflect-i GET_AND_BITWISE_OR (Box,int)int -> Integer:43
// tmh unreflect-i GET_AND_BITWISE_XOR_RELEASE (Box,int)int -> Integer:63
// tmh unreflect-i GET (Box)int -> Integer:42
// invoker unreflect-i (VarHandle,Box)int -> Integer:42
// exact-invoker unreflect-i (VarHandle,Box)int -> Integer:42
// exact-mask unreflect-i 1111111111111111111111111111111 exact=true
// tmh unreflect-i-exact GET (Box)int -> Integer:42
// reveal unreflect-i java.lang.IllegalArgumentException: not a direct method handle
// mask unreflect-fi 1010101000000000000000000000000
// vh unreflect-fi VarHandle[varType=int, coord=[class L4W123RealVarForm$Box]] GET=(Box)int
// tmh unreflect-fi GET (Box)int -> Integer:11
// tmh unreflect-fi GET_VOLATILE (Box)int -> Integer:11
// tmh unreflect-fi SET (Box,int)void -> threw java.lang.UnsupportedOperationException: set
// tmh unreflect-fi GET_ACQUIRE (Box)int -> Integer:11
// tmh unreflect-fi COMPARE_AND_SET (Box,int,int)boolean -> threw java.lang.UnsupportedOperationException: compareAndSet
// tmh unreflect-fi COMPARE_AND_EXCHANGE (Box,int,int)int -> threw java.lang.UnsupportedOperationException: compareAndExchange
// tmh unreflect-fi GET_AND_SET (Box,int)int -> threw java.lang.UnsupportedOperationException: getAndSet
// tmh unreflect-fi WEAK_COMPARE_AND_SET_PLAIN (Box,int,int)boolean -> threw java.lang.UnsupportedOperationException: weakCompareAndSetPlain
// tmh unreflect-fi GET_AND_ADD (Box,int)int -> threw java.lang.UnsupportedOperationException: getAndAdd
// tmh unreflect-fi GET_AND_BITWISE_OR (Box,int)int -> threw java.lang.UnsupportedOperationException: getAndBitwiseOr
// tmh unreflect-fi GET_AND_BITWISE_XOR_RELEASE (Box,int)int -> threw java.lang.UnsupportedOperationException: getAndBitwiseXorRelease
// tmh unreflect-fi GET (Box)int -> Integer:11
// invoker unreflect-fi (VarHandle,Box)int -> Integer:11
// exact-invoker unreflect-fi (VarHandle,Box)int -> Integer:11
// exact-mask unreflect-fi 1010101000000000000000000000000 exact=true
// tmh unreflect-fi-exact GET (Box)int -> Integer:11
// reveal unreflect-fi java.lang.IllegalArgumentException: not a direct method handle
// cce-holder java.lang.ClassCastException: Cannot cast java.lang.String to L4W123RealVarForm$Box
// cce-value java.lang.ClassCastException: class java.lang.String cannot be cast to class java.lang.Number (java.lang.String and java.lang.Number are in module java.base of loader 'bootstrap')
// npe-holder java.lang.NullPointerException
// null-mode java.lang.NullPointerException
// warm-sum 4519500 final 3007
// done
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.lang.invoke.VarHandle.AccessMode;

public class L4W123RealVarForm {

    static class Box {
        int i = 7;
        long j = 70L;
        boolean z = true;
        byte b = 3;
        short sh = 5;
        char c = 'q';
        float f = 1.5f;
        double d = 2.5;
        String s = "s0";
        final int fi = 11;
        final String fs = "fixed";
        static int SI = 100;
        static Object SO = "so0";
        static final String SF = "sf";
        static final long SJ = 9L;
    }

    static String show(Object o) {
        if (o == null) {
            return "null";
        }
        if (o instanceof Character ch) {
            return "'" + ch + "'";
        }
        return o.getClass().getSimpleName() + ":" + o;
    }

    static String err(Throwable t) {
        while (t.getCause() != null && t instanceof RuntimeException == false && t instanceof Error == false) {
            t = t.getCause();
        }
        return t.getClass().getName() + ": " + t.getMessage();
    }

    static String mask(VarHandle vh) {
        StringBuilder sb = new StringBuilder();
        for (AccessMode m : AccessMode.values()) {
            sb.append(vh.isAccessModeSupported(m) ? '1' : '0');
        }
        return sb.toString();
    }

    /** toMethodHandle(mode): its type, then the result of invoking it with args. */
    static void viaToMethodHandle(String name, VarHandle vh, AccessMode mode, Object... args) {
        MethodHandle mh;
        try {
            mh = vh.toMethodHandle(mode);
        } catch (Throwable t) {
            System.out.println("tmh " + name + " " + mode + " build " + err(t));
            return;
        }
        String result;
        try {
            result = show(mh.invokeWithArguments(args));
        } catch (Throwable t) {
            result = "threw " + err(t);
        }
        System.out.println("tmh " + name + " " + mode + " " + mh.type() + " -> " + result);
    }

    static void handle(String name, VarHandle vh, Object holder, Object v1, Object v2, boolean numeric, boolean bitwise) {
        System.out.println("mask " + name + " " + mask(vh));
        System.out.println("vh " + name + " " + vh + " GET=" + vh.accessModeType(AccessMode.GET));
        Object[] h = holder == null ? new Object[0] : new Object[] {holder};
        viaToMethodHandle(name, vh, AccessMode.GET, h);
        viaToMethodHandle(name, vh, AccessMode.GET_VOLATILE, h);
        viaToMethodHandle(name, vh, AccessMode.SET, cat(h, v1));
        viaToMethodHandle(name, vh, AccessMode.GET_ACQUIRE, h);
        viaToMethodHandle(name, vh, AccessMode.COMPARE_AND_SET, cat(h, v1, v2));
        viaToMethodHandle(name, vh, AccessMode.COMPARE_AND_EXCHANGE, cat(h, v2, v1));
        viaToMethodHandle(name, vh, AccessMode.GET_AND_SET, cat(h, v2));
        viaToMethodHandle(name, vh, AccessMode.WEAK_COMPARE_AND_SET_PLAIN, cat(h, v1, v1));
        viaToMethodHandle(name, vh, AccessMode.GET_AND_ADD, cat(h, v1));
        viaToMethodHandle(name, vh, AccessMode.GET_AND_BITWISE_OR, cat(h, v1));
        viaToMethodHandle(name, vh, AccessMode.GET_AND_BITWISE_XOR_RELEASE, cat(h, v1));
        viaToMethodHandle(name, vh, AccessMode.GET, h);
        // The generic and exact invokers of GET, called with the handle first.
        try {
            MethodType t = vh.accessModeType(AccessMode.GET);
            MethodHandle inv = MethodHandles.varHandleInvoker(AccessMode.GET, t);
            System.out.println("invoker " + name + " " + inv.type() + " -> " + show(inv.invokeWithArguments(cat(new Object[] {vh}, h))));
            MethodHandle exact = MethodHandles.varHandleExactInvoker(AccessMode.GET, t);
            System.out.println("exact-invoker " + name + " " + exact.type() + " -> " + show(exact.invokeWithArguments(cat(new Object[] {vh}, h))));
        } catch (Throwable t) {
            System.out.println("invoker " + name + " threw " + err(t));
        }
        // An exact copy: its own mask and toMethodHandle(GET).
        VarHandle ex = vh.withInvokeExactBehavior();
        System.out.println("exact-mask " + name + " " + mask(ex) + " exact=" + ex.hasInvokeExactBehavior());
        viaToMethodHandle(name + "-exact", ex, AccessMode.GET, h);
        // A toMethodHandle result is bound, not direct.
        try {
            MethodHandles.lookup().revealDirect(vh.toMethodHandle(AccessMode.GET));
            System.out.println("reveal " + name + " cracked");
        } catch (Throwable t) {
            System.out.println("reveal " + name + " " + err(t));
        }
    }

    static Object[] cat(Object[] a, Object... b) {
        Object[] r = new Object[a.length + b.length];
        System.arraycopy(a, 0, r, 0, a.length);
        System.arraycopy(b, 0, r, a.length, b.length);
        return r;
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        Box box = new Box();
        handle("i", l.findVarHandle(Box.class, "i", int.class), box, 8, 9, true, true);
        handle("j", l.findVarHandle(Box.class, "j", long.class), box, 80L, 90L, true, true);
        handle("z", l.findVarHandle(Box.class, "z", boolean.class), box, false, true, false, true);
        handle("b", l.findVarHandle(Box.class, "b", byte.class), box, (byte) 4, (byte) 6, true, true);
        handle("sh", l.findVarHandle(Box.class, "sh", short.class), box, (short) 6, (short) 12, true, true);
        handle("c", l.findVarHandle(Box.class, "c", char.class), box, 'r', 's', true, true);
        handle("f", l.findVarHandle(Box.class, "f", float.class), box, 2.5f, 4.0f, true, false);
        handle("d", l.findVarHandle(Box.class, "d", double.class), box, 3.5, 7.0, true, false);
        handle("s", l.findVarHandle(Box.class, "s", String.class), box, "s1", "s2", false, false);
        handle("fi", l.findVarHandle(Box.class, "fi", int.class), box, 12, 13, true, true);
        handle("fs", l.findVarHandle(Box.class, "fs", String.class), box, "x", "y", false, false);
        handle("SI", l.findStaticVarHandle(Box.class, "SI", int.class), null, 101, 102, true, true);
        handle("SO", l.findStaticVarHandle(Box.class, "SO", Object.class), null, "so1", "so2", false, false);
        handle("SF", l.findStaticVarHandle(Box.class, "SF", String.class), null, "a", "b", false, false);
        handle("SJ", l.findStaticVarHandle(Box.class, "SJ", long.class), null, 1L, 2L, true, true);
        handle("unreflect-i", l.unreflectVarHandle(Box.class.getDeclaredField("i")), box, 21, 22, true, true);
        handle("unreflect-fi", l.unreflectVarHandle(Box.class.getDeclaredField("fi")), box, 21, 22, true, true);

        // A mistyped holder and value through toMethodHandle(GET) / (SET).
        VarHandle vi = l.findVarHandle(Box.class, "i", int.class);
        try {
            vi.toMethodHandle(AccessMode.GET).invokeWithArguments("not a box");
            System.out.println("cce-holder none");
        } catch (Throwable t) {
            System.out.println("cce-holder " + err(t));
        }
        try {
            vi.toMethodHandle(AccessMode.SET).invokeWithArguments(box, "x");
            System.out.println("cce-value none");
        } catch (Throwable t) {
            System.out.println("cce-value " + err(t));
        }
        try {
            vi.toMethodHandle(AccessMode.GET).invokeWithArguments((Object) null);
            System.out.println("npe-holder none");
        } catch (Throwable t) {
            System.out.println("npe-holder " + t.getClass().getName());
        }
        try {
            vi.isAccessModeSupported(null);
            System.out.println("null-mode none");
        } catch (Throwable t) {
            System.out.println("null-mode " + t.getClass().getName());
        }

        // Warm: one toMethodHandle(GET_AND_ADD) result, called through invokeExact.
        MethodHandle add = vi.toMethodHandle(AccessMode.GET_AND_ADD);
        Box warm = new Box();
        long sum = 0;
        for (int k = 0; k < 3000; k++) {
            sum += (int) add.invokeExact(warm, 1);
        }
        System.out.println("warm-sum " + sum + " final " + warm.i);
        System.out.println("done");
    }
}
