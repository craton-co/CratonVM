# TestWsRemoteEndpointImplServerDeadlock close delay fixed

**Status:** FIXED. **Severity:** high. **HotSpot:** PASS.

## Summary

`org.apache.tomcat.websocket.server.TestWsRemoteEndpointImplServerDeadlock`
previously failed all four parameter combinations with a close delay of about
19 seconds. That delay matched Tomcat's 20 second blocking-send timeout and
looked like a near-deadlock in the WebSocket close path.

The final live root cause was not a selector stall or a stuck
`messagePartInProgress` semaphore. Once the startup/handshake blockers were
cleared, a focused state probe showed the server `WsSession` reached
`CLOSED` about 1.1 seconds after the test released the client latch. The JUnit
test polls the private `WsSession.state` field, which is an
`AtomicReference<WsSession.State>`, by calling `state.toString()`. CratonVM's
native `AtomicReference.toString()` returned `class@hash` for the referenced
object instead of the JDK behavior, `String.valueOf(get())`. For enum states
that meant the test saw `org.apache.tomcat.websocket.WsSession$State@...`
instead of `CLOSED`, so it waited until its 19 second polling limit.

## Fix

- `native-builtins/src/lib.rs`: `AtomicReference.toString()` now delegates to
  the contained value's own `toString()` via the shared string helper. This
  matches `String.valueOf(value)` and makes enum-backed references render as
  `OPEN`, `CLOSING`, `CLOSED`, etc.
- Added a focused Rust regression test:
  `atomic_reference_to_string_delegates_to_value`.

This branch also cleared two real-JDK synthetic-layout blockers that were
preventing a clean run of the WebSocket test:

- `java/io/StringReader` synthetic natives are dropped in real-JDK mode. The
  synthetic constructor wrote the old `content/pos/length` layout, leaving the
  real JDK 25 delegate field `r` null and causing `StringReader.mark()` to NPE
  during Tomcat startup.
- `java/util/concurrent/LinkedBlockingDeque` synthetic fallback natives are
  dropped in real-JDK mode. The fallback constructor wrote a fake queue layout,
  leaving real fields such as `lock`, `notEmpty`, and `notFull` null; Tomcat's
  WebSocket `WriteBuffer.clear()` then failed in `LinkedBlockingDeque.clear()`.

Earlier commits on this same branch had already fixed the prerequisite
`EnumSet` and `ScheduledThreadPoolExecutor` real-layout issues and the
monitor-leak-on-interrupt problem exposed by Tomcat executor contention.

## Verification

Focused Rust tests:

```bash
cargo test -p cratonvm-native-builtins atomic_reference_to_string_delegates_to_value -- --nocapture
cargo test -p cratonvm-native-api real_layout_mode_drops_enumset_native_surface -- --nocapture
```

Both pass.

Release build and unique binary:

```bash
cargo build --release -p cratonvm-cli
cp target/release/cratonvm target/release/cratonvm-tomcat-wsclose-final-20260710-0105
```

Original Tomcat JUnit class on the Azure Linux fixture:

```bash
cd /data/data/apps/tomcat
CP=$(cat .suite/cp-linux-fixed.txt)
wt=/data/data/cratonvm-worktrees/20260709-223225-tomcat-wsclose-enumset
basedir="$wt/diagnostics/tomcat_wsdeadlock_20260710/tomcat-test-basedir"
"$wt/target/release/cratonvm-tomcat-wsclose-final-20260710-0105"   --java-home /data/data/jdk25-real   -Dtomcat.test.basedir="$basedir"   -Dtomcat.test.temp=/data/data/apps/tomcat/output/tmp   -cp "$CP"   org.junit.runner.JUnitCore   org.apache.tomcat.websocket.server.TestWsRemoteEndpointImplServerDeadlock
```

The reduced Tomcat checkout on the host does not include
`conf/logging.properties`, so the verification used a minimal temporary
`$basedir/conf/logging.properties` under diagnostics. Without that fixture,
the test body passes but JUnit reports teardown failures from
`ClassLoaderLogManager.reset()` trying to open the missing config file.

Result with the temporary basedir:

```text
OK (4 tests)
```

Additional diagnostic result: a custom `WsCloseStateProbe` showed the server
state transitions from `OPEN` to `CLOSING` and then `CLOSED` at about 1.1s
after `clientReceiveLatch.countDown()`, confirming the 19s assertion was a
polling/rendering bug rather than an actual delayed close after this fix.
