import java.nio.file.*;
public class AbsCheck { public static void main(String[] a){
  for(String x: a){ Path p=Paths.get(x);
    System.out.println("["+x+"] abs=["+p.toAbsolutePath()+"] norm=["+p.toAbsolutePath().normalize()+"]"); }
}}
