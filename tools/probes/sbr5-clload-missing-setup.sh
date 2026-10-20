cd /data/sbr5/clload; J=/data/jdkimages/jdk25-linux/jdk-25.0.4+7/bin; rm -rf ms; mkdir -p ms/s/ms ms/o ms/o2
printf "package ms; public class Gone { }\n" > ms/s/ms/Gone.java
printf "package ms; public interface GoneI { }\n" > ms/s/ms/GoneI.java
printf "package ms; public class Ok { }\n" > ms/s/ms/Ok.java
printf "package ms; public class Sub extends Gone { }\n" > ms/s/ms/Sub.java
printf "package ms; public abstract class Iface2 implements GoneI { }\n" > ms/s/ms/Iface2.java
$J/javac -d ms/o ms/s/ms/*.java; rm ms/o/ms/Gone.class ms/o/ms/GoneI.class
$J/jar --create --file ms/msjar.jar -C ms/o .
$J/javac -d ms Sbr5ClloadMissing.java
