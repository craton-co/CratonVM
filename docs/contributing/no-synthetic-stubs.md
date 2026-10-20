# No new synthetic stubs

Application-visible JDK behavior should come from real JDK bytecode or a
correct, tested VM bridge/intrinsic. A synthetic stub is acceptable only as a
temporary, explicit compatibility fallback with a tracked removal issue.

New registrations must set the accurate `NativeKind`. Reclassifying a
fabricated answer as `Bridge` to evade the ratchet is a correctness defect.
Include a direct contract test and, when replacing real bytecode, a HotSpot
differential test.
