# Warm GPU comparison — GpuWarm.heavy (96 int MADs/element)

Date: 2026-07-11T12:24:55Z

| N | CratonVM CPU | HotSpot CPU | CratonVM GPU | TornadoVM GPU | CV-GPU vs CV-CPU | CV-GPU vs HotSpot |
|---|---|---|---|---|---|---|
| 2^22 | 472ms | 2ms | 1ms | 5ms | 472× | 2.0× |
| 2^24 | 1955ms | 8ms | 11ms | 17ms | 178× | 0.7× |
| 2^26 | 7689ms | 25ms | 27ms | 51ms | 285× | 0.9× |

Samples (CratonVM/HotSpot must match; Tornado prints full checksum):
- n=4194304: cv-cpu=4984709887 cv-gpu=4984709887 hotspot=4984709887
- n=16777216: cv-cpu=1782358783 cv-gpu=1782358783 hotspot=1782358783
- n=67108864: cv-cpu=1857856255 cv-gpu=1857856255 hotspot=1857856255
