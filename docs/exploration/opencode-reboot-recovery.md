# Managed V2 reboot acceptance

Validated on October 20, 2026 in the dedicated Cua macOS VM
`local:space-cf9cc75f48`, not the host or the shared browser VM.

The installed OpenCode build was `2.0.32-local.8f3a0221f0`. Its managed service
already recovers durable execution claims after crashes and graceful shutdown.
It inserts a synthetic restart notice, not a user `continue` message.

The old agentview restart path admitted an additional `continue` after native
recovery had begun. Inspecting the real `session_inbox` proved that these inputs
were queued behind the recovered turns, even though the transcript initially
showed no extra user message. Registered V2 services now use native recovery
alone, including shell recovery. Legacy/unregistered behavior is unchanged.

Interactive startup also replaces a dead recorded managed service, including
when its registration survives a reboot. A stale registration for the recorded
PID is not adopted; after reboot, a matching private endpoint/secret also
handles registration and authority records persisted at different times.
Read-only JSON discovery does not call the startup recovery method.

## Reproduce

Use a disposable macOS VM. Copy the real binaries to
`~/reboot-recovery/bin/agentview` and `~/reboot-recovery/bin/opencode`, and copy
`scripts/vm-reboot-recovery.py` to `~/reboot-recovery/test.py`. The script refuses
to run outside a VirtualMac. It creates a fresh isolated profile and a local,
token-free OpenAI-compatible streaming provider; no provider credentials move.

1. `python3 ~/reboot-recovery/test.py prepare`
2. `python3 ~/reboot-recovery/test.py restart`
3. Reboot the VM, not the host.
4. `python3 ~/reboot-recovery/test.py recover`
5. `python3 ~/reboot-recovery/test.py replay`
6. Repeat reboot/recover/replay to check recovery of a previously resumed turn.
7. `python3 ~/reboot-recovery/test.py cleanup`, then stop the dedicated VM.

The harness launches the real interactive dashboard under a sized PTY and sends
no Enter or `continue`. The provider returns a completed response for one
session, streams text for another, and holds the response before its first
token for a third. A fourth session is deliberately interrupted before reboot.

Observed passes:

- Controlled restart retained the endpoint and recovered both active roots.
- Two complete VM reboots recovered both roots once each on dashboard startup.
- Completed and deliberately interrupted sessions stayed stopped.
- Reopening the dashboard with a deliberately restored stale restart journal
  consumed that journal without admitting a duplicate continuation.
- Both projected messages and the persisted `session_inbox` were checked;
  there were no injected user continuations, including queued ones.
- The test provider, dashboards and managed server were stopped, and the VM's
  stopped state was verified through Lume.

Additional checks: 21 supervisor unit tests, five explicitly enabled restart
fixtures, the real compiled V2 lifecycle test, formatting, and all-target Clippy
on the CI-pinned Rust 1.75.0 toolchain. The fake-server fixtures now refuse an
inherited live service registration before creating their supervisor.

Native recovery is at-least-once execution, not an exactly-once guarantee for
external tool side effects. This test does not send messages, make purchases,
or claim that an interrupted external action can safely be repeated blindly.
