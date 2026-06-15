import java.util.*;
import java.util.stream.*;
public class StreamProbe2 {
  static void t(String n, Runnable r){ try { r.run(); System.out.println("OK   "+n); } catch (Throwable e){ System.out.println("FAIL "+n+" -> "+e.getClass().getName()+": "+e.getMessage()); } }
  public static void main(String[] a){
    int[] arr = {9,10,11};
    t("of.findFirst",            () -> IntStream.of(3,1,2).findFirst().getAsInt());
    t("range.findFirst",         () -> IntStream.range(0,5).findFirst().getAsInt());
    t("range.filter.findFirst",  () -> IntStream.range(0,5).filter(i->i>1).findFirst().getAsInt());
    t("rangeClosed.map.findFirst",()-> IntStream.rangeClosed(1,3).map(i->i*2).findFirst().getAsInt());
    t("boxed.mapToInt.findFirst",() -> Stream.of("a","bb","ccc").mapToInt(String::length).findFirst().getAsInt());
    t("Arrays.stream.findFirst", () -> Arrays.stream(arr).findFirst().getAsInt());
    t("Arrays.stream.filter",    () -> Arrays.stream(arr).filter(i->i>9).findFirst().getAsInt());
    t("Stream.mapToInt.filter",  () -> Stream.of(1,2,3).mapToInt(Integer::intValue).filter(i->i>1).findFirst().getAsInt());
    t("IntStream.iterate",       () -> IntStream.iterate(1,i->i+1).limit(3).findFirst().getAsInt());
    System.out.println("PROBE2_DONE");
  }
}
