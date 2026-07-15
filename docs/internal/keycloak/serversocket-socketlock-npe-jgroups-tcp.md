# `ServerSocket` receive-buffer configuration before bind — fixed

Status: fixed and retired on 2026-07-15

## Root cause

The real-JDK build uses the RE2 `java.net.ServerSocket` constructor bridges.
Those bridges bypass the JDK constructor's field initialization and then leave
the public receive-buffer APIs to real JDK bytecode. Calling
`setReceiveBufferSize` before bind therefore entered `getImpl()` with no usable
synthetic implementation state, first exposing the null `socketLock` and then
the null `impl` invariant.

## Fix

All RE2 `ServerSocket` constructor overloads now initialize the real object's
lock fields. The RE2 surface also owns direct bridges for
`setReceiveBufferSize(int)` and `getReceiveBufferSize()`, including validation
of non-positive sizes. This keeps the legal pre-bind configuration path out of
the incompatible real `getImpl()` path while preserving the existing
side-table-backed listener implementation.

## Verification

- Added `test_classes/ServerSocketReceiveBufferProbe.java`.
- HotSpot and the uniquely built Azure CratonVM binary both passed the sequence
  `new ServerSocket()`, set/get receive-buffer size, explicit-address bind, and
  local-port assertion.
- The original Keycloak runner reached a generated 252-entry
  `model/infinispan` classpath, but its available compiled checkout lacks
  `org.keycloak.jgroups.protocol.JdbcPing2Test`; it therefore reports a
  harness `ClassNotFoundException` before any test code executes. This is not a
  residual of the retired VM defect.
