cd /data/sbr5/clload; J=/data/jdkimages/jdk25-linux/jdk-25.0.4+7/bin; rm -rf sp; mkdir -p sp/s1/com/split sp/s2/com/split sp/o1 sp/o2
printf "package com.split; public class A { }\n" > sp/s1/com/split/A.java
printf "package com.split; public class B { }\n" > sp/s2/com/split/B.java
$J/javac -d sp/o1 sp/s1/com/split/A.java; $J/javac -d sp/o2 sp/s2/com/split/B.java
printf "Manifest-Version: 1.0\nImplementation-Version: one\n" > sp/m1.txt; printf "Manifest-Version: 1.0\nImplementation-Version: two\n" > sp/m2.txt
$J/jar --create --file sp/s1.jar --manifest sp/m1.txt -C sp/o1 .; $J/jar --create --file sp/s2.jar --manifest sp/m2.txt -C sp/o2 .
$J/javac -d sp Sbr5ClloadSplit.java
