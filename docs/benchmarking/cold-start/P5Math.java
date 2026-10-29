public class P5Math { public static void main(String[] a) {
  System.out.println(Math.sin(1.0) + " " + Math.cos(2.0) + " " + Math.tan(0.5) + " " + Math.log(10) + " " + Math.exp(1.5) + " " + Math.pow(2.5, 3.1) + " " + Math.sqrt(2) + " " + Math.cbrt(27) + " " + Math.atan2(1,2) + " " + Math.hypot(3,4) + " " + Math.log10(1000) + " " + Math.sinh(1) + " " + Math.expm1(0.001) + " " + Math.log1p(0.001));
  System.out.println(StrictMath.sin(1.0) + " " + StrictMath.cos(2.0) + " " + StrictMath.log(10) + " " + StrictMath.pow(2.5,3.1) + " " + StrictMath.cbrt(10) + " " + StrictMath.IEEEremainder(10, 3));
  System.out.println(Math.floorMod(-7,3) + " " + Math.round(2.5) + " " + Math.abs(-3) + " " + Math.fma(1.5,2.0,0.25) + " " + Math.ulp(1.0) + " " + Math.nextUp(1.0f) + " " + Math.multiplyHigh(Long.MAX_VALUE, 3));
}}
