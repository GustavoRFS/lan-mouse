# Windows capture crash correction

**Goal:** Fix the daemon crashes observed in the installed Windows build without changing tray or clipboard behavior.

**Evidence:** Nine preserved minidumps all contain the call to `PostThreadMessageW(...).unwrap()` at `input-capture/src/windows/event_thread.rs:79:90`, with HRESULT `0x800705a4` (invalid thread ID / no message queue). The constructor publishes the thread ID before its first USER/GDI call creates the message queue. Most WER failures occurred within 0.2–3 seconds of daemon startup. The matching installed executable and local release executable have SHA256 `511232CE028112746B22B0574F026E0369932ACF52CB16363DF02CF55064B8BD`.

**Approach:** Create the Win32 message queue before publishing readiness, as documented by Microsoft for PostThreadMessageW. Propagate posting errors through CaptureError; teardown must not panic when its worker already exited. Log panic location and backtrace through the existing logger so future unrelated crashes are diagnosable. Preserve the existing startup/tray policy.

**Constraints:** All compilation/tests run on ligma; Windows integration tests use Wine there. Work in an isolated checkout, preserve installed binary and dumps, and perform a Windows launch smoke check after installation.

## Task 1: Capture initialization and signaling

- [x] Add Windows regressions for immediate posting after constructor, posting to an exited thread, a transient full queue while stopping, and a closed event receiver.
- [x] Cross-compile and run against the original code; observe failure.
- [x] Initialize the message queue before readiness notification.
- [x] Return Windows errors from signal/create/destroy/release; propagate from WindowsInputCapture and log shutdown errors instead of unwrapping.
- [x] Run Windows regressions under Wine, then relevant workspace tests and lint.

## Task 2: Panic diagnostics and delivery

- [x] Add a regression asserting panic messages/location reach the configured logger.
- [x] Install a panic hook after logger initialization, including a forced backtrace and flush.
- [x] Document crash troubleshooting, build Windows release with the existing GTK import libraries on ligma, and retain symbols.
- [x] Review the complete diff; preserve and replace the installed executable, restart the task, and verify processes/logs.

## Review focus

Readiness must mean the queue exists; no startup sleeps. Exited threads must produce errors without aborting in Drop. Preserve normal release/create/destroy delivery. Panic diagnostics must work with stderr redirected to NUL. Keep builds/tests off omen and avoid modifying unrelated clipboard behavior.

## Execution notes

The user explicitly requested diagnosis and correction. Routine implementation and isolated checkout choices are covered by that authorization; no separate design approval is needed. The existing source checkout was clean at fa01e95.
