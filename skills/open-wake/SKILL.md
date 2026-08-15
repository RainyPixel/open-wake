---
name: open-wake
description: Wait for long-running local builds, tests, deployments, jobs, or other observable conditions without polling from Codex. Use when Codex would otherwise call sleep, repeatedly poll a process or log, or keep a turn open only to wait. Do not use for short foreground commands or conditions that cannot be checked locally with a fast read-only predicate.
---

# Open Wake

Use `open-wake` to end the current turn and resume it once a local condition is
true or its deadline is reached.

## Workflow

1. Start the long-running work under an independent supervisor or other durable
   execution mechanism. `open-wake` observes work; it does not keep an ordinary
   foreground command alive.
2. Define a fast, read-only, idempotent predicate. Exit `0` must mean ready; any
   other exit status must mean not ready yet.
3. Arm one condition for the current Codex session:

   ```console
   open-wake arm \
     --label "release build" \
     --timeout 1h \
     --interval 10s \
     --check-timeout 5s \
     -- sh -c 'test -f target/release/app'
   ```

4. Confirm that `arm` succeeded, briefly tell the user what condition and
   deadline were registered, and end the turn immediately. Do not call
   `status`, `sleep`, or another polling command in that turn.
5. When the `open-wake` continuation arrives, inspect its success, timeout, or
   failure result and continue the original task. Read durable job logs only if
   they are needed for the next decision.

## Predicate contract

- Keep checks cheap because they may run many times.
- Print only concise result evidence. The final check output is bounded and may
  be included in the continuation prompt.
- Never mutate production state, retry deployment actions, or perform cleanup
  in a predicate.
- Never include credentials or secrets in predicate arguments or output.
- Prefer durable signals such as a supervisor status, exit-code file, sentinel
  file, or purpose-built status script over parsing volatile terminal output.

Use `open-wake status` to diagnose an armed condition and `open-wake cancel`
to release a waiting turn without continuing it. Only one condition may be
active per Codex session.

If setup is missing or stale, run `open-wake doctor`. Apply the exact
`open-wake setup --scope user|project` command it recommends, then review and
trust the installed hook with `/hooks`.
