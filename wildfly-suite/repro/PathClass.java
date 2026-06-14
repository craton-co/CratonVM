import java.nio.file.*;
public class PathClass { public static void main(String[] a){
  for(String x: a){
    Path p=Paths.get(x);
    System.out.println("IN=["+x+"]");
    System.out.println("  toAbsolutePath=["+p.toAbsolutePath()+"]");
    System.out.println("  normalize=["+p.normalize()+"]");
    System.out.println("  toAbs.norm=["+p.toAbsolutePath().normalize()+"]");
  }
}}
