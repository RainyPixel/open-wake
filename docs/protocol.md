# Protocol and invariants

## Wake lifecycle

`arm` writes one versioned condition record keyed by `CODEX_THREAD_ID` and
returns. When Codex is about to stop, the configured `Stop` hook reads the hook
event from stdin. If the session has no active condition, it returns `{}` and
does not affect Codex.

For an active condition, the hook transitions it from `armed` to `waiting` and
acquires a private hook-owner lease. It runs the predicate until one terminal
state or a configured checkpoint is reached:

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

A `waiting` record is not by itself proof that a hook is alive. Its lease
contains the condition ID, unique owner ID, PID, and heartbeat
time. The owning hook refreshes the heartbeat while waiting. Liveness requires a
recent heartbeat and an existing owner PID; PID liveness is checked without
delivering a signal. The current writer refreshes every second and treats a
heartbeat older than five seconds as stale. If Codex interrupts the process, a
later Stop invocation may acquire the stale lease and continue the same
condition without resetting its ID, attempts, deadline, or checkpoint count.
Until a legacy version-1 record can be classified safely, a recent record
without a lease is treated as ownership-unknown.

`cancel` transitions an active condition directly to terminal `cancelled`. It
does not depend on another Stop event, so the same session can be re-armed even
when Codex never invoked the hook. Records written by older versions with the
transitional `cancel_requested` status are treated as terminal and normalized
to `cancelled` by an idempotent `cancel`.

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

Every read-validate-write transition is serialized by a per-session lock. Only
one fresh hook lease may drive a `waiting` generation. Hook updates require the
condition ID and owner ID they started with; predicate output is
condition- and owner-scoped, so overlapping predicates from a stale hook and
recovery hook cannot remove or overwrite each other's temporary output.

New condition records are version 2 and use `poll_every_ms` plus
`checkpoint_every_ms`. The reader also accepts version-1 `interval_ms` and
`check_every_ms` fields so an already active condition remains observable after
an upgrade. On takeover, a legacy checkpoint below one minute is raised to one
minute, or removed when the remaining overall timeout cannot accommodate it.
New writes do not preserve the old field names. A binary older than this state
format cannot read version-2 records, so downgrade after a v2 write requires
discarding or manually migrating that ephemeral condition.

For `run`, a persistent job directory contains `job.json`, a heartbeat,
`output.log`, and eventually an atomic `result.json`. The result records the
exact exit code or Unix signal. A recent heartbeat proves only that the
supervisor is observing the child; a stale heartbeat does not prove the child
stopped. `doctor` therefore reports stale jobs without killing or deleting
anything. The launcher reports success only after seeing the first heartbeat
or a terminal result from the supervisor.

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
deadline accounting includes predicate execution and `--poll-every` intervals.
Model-visible `--checkpoint-every` values must be at least one minute and
shorter than the timeout; values below five minutes produce a warning.

## Failure boundaries

- A missing or inactive condition is a no-op.
- A `waiting` condition with a fresh lease is ignored by another hook. A stale
  lease is recoverable by the next Stop invocation.
- Predicate exit statuses other than `0` mean "not ready" and are retried.
- Failure to start or observe a predicate wakes Codex with `failed` evidence.
- Failure to acknowledge a `run` supervisor cancels its newly armed condition
  by generation ID and retains the failed job record for inspection. A stale
  launcher cannot cancel a replacement condition.
- Overall timeout wakes Codex with the last bounded predicate result. If a
  supervised job has already reached a recorded terminal state, that result
  takes precedence over a late timeout check.
- Cancellation releases the session immediately without waking another agent
  turn. A predicate already in flight may finish before its hook process exits,
  but its result cannot overwrite the cancellation or a replacement condition.
- Cancellation stops notifications, not the supervised command.
- Callers must inspect an attached job before launching another supervised
  command after cancellation.
- Replacing the inactive condition record causes the older hook invocation to
  stop without acting on the new record.
- Predicate stdout and stderr share one private file; only the last 4 KiB can be
  copied into the continuation.
- Supervised command stdout and stderr share a persistent, unrestricted disk
  log. `logs` returns only its absolute path so callers can use native bounded
  readers and search tools.

## Trust boundary

Setup enables the hooks feature and the exact installed hook state, but it does
not grant trust silently. Codex users must review the discovered hook command
in `/hooks` and trust it if requested. Project hooks additionally depend on the
repository's `.codex` configuration being trusted by Codex.
