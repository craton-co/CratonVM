public class Bits {
  public static void main(String[] a){
    long[] tv = {0L,1L,2L,8L,1024L,0x8000000000000000L,-1L,0x100L};
    for(long v: tv) System.out.println("L.ntz("+v+")="+Long.numberOfTrailingZeros(v)+" L.nlz="+Long.numberOfLeadingZeros(v)+" L.bc="+Long.bitCount(v));
    int[] iv = {0,1,2,8,1024,Integer.MIN_VALUE,-1,256};
    for(int v: iv) System.out.println("I.ntz("+v+")="+Integer.numberOfTrailingZeros(v)+" I.nlz="+Integer.numberOfLeadingZeros(v)+" I.bc="+Integer.bitCount(v)+" I.highestOneBit="+Integer.highestOneBit(v));
    System.out.println("DONE");
  }
}
