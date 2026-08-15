# Protocol and invariants

## Wake lifecycle

`arm` writes one versioned condition record keyed by `CODEX_THREAD_ID` and
returns. When Codex is about to stop, the configured `Stop` hook reads the hook
event from stdin. If the session has no active condition, it returns `{}` and
does not affect Codex.

For an active condition, the hook transitions it from `armed` to `waiting` and
runs the predicate until one terminal state or a configured checkpoint is
reached:

| State | Meaning | Codex continuation |
| --- | --- | --- |
| `succeeded` | Predicate exited `0` before its deadline | Yes |
| `timed_out` | Overall condition deadline elapsed | Yes |
| `failed` | Predicate could not be started or observed | Yes |
| `cancelled` | User requested cancellation | No |

A recurring checkpoint produces a continuation but is not terminal. The
condition records the checkpoint, transitions back to `armed`, and keeps its
original deadline. When that agent turn stops, the next hook invocation resumes
checking the same predicate. For `run`, the detached command is never launched
again.

The continuation is the standard Stop-hook response:

```json
{
  "decision": "block",
  "reason": "open-wake: condition met ... Continue the task now."
}
```

The `block` decision prevents the current stop and creates one new agent turn.
A terminal record is inactive, so a later `Stop` event returns `{}` rather than
creating another continuation.

## Ownership

The condition authority is local durable state, not a terminal pane. Terminal
multiplexers are outside the protocol.

For `run`, a persistent job directory contains `job.json`, a heartbeat,
`output.log`, and eventually an atomic `result.json`. The result records the
exact exit code or Unix signal. A recent heartbeat proves only that the
supervisor is observing the child; a stale heartbeat does not prove the child
stopped. `doctor` therefore reports stale jobs without killing or deleting
anything.

Setup owns exactly one command handler identified by the status message
`open-wake: waiting for armed condition`. It may replace or remove that handler
but preserves other hook events, groups, handlers, and top-level JSON fields.

The embedded skill is installed under the reserved `open-wake` directory.
Setup updates its two known files. Uninstall deletes a known file only when its
bytes still match the embedded version.

## Timing

The installed synchronous Stop hook has a seven-day timeout. An armed condition
must be at least 60 seconds shorter, leaving time for hook startup and result
delivery. Each predicate also has a shorter independent check timeout. Overall
deadline accounting includes predicate execution and intervals.

## Failure boundaries

- A missing or inactive condition is a no-op.
- Predicate exit statuses other than `0` mean "not ready" and are retried.
- Failure to start or observe a predicate wakes Codex with `failed` evidence.
- Overall timeout wakes Codex with the last bounded predicate result. If a
  supervised job has already reached a recorded terminal state, that result
  takes precedence over a late timeout check.
- Cancellation releases the Stop hook without waking another agent turn.
- Cancellation stops notifications, not the supervised command.
- Replacing the active condition record causes the older hook invocation to
  stop without acting on the new record.
- Predicate stdout and stderr share one private file; only the last 4 KiB can be
  copied into the continuation.
- Supervised command stdout and stderr share a persistent, unrestricted disk
  log. `logs` returns only its absolute path so callers can use native bounded
  readers and search tools.

## Trust boundary

Installing configuration does not grant trust silently. Codex users must review
the discovered hook command in `/hooks`. Project hooks additionally depend on
the repository's `.codex` configuration being trusted by Codex.
