---
name: open-wake
description: Run or observe long-running local builds, tests, deployments, jobs, and other conditions without polling from Codex. Use when Codex would otherwise call sleep, repeatedly poll a process or log, or keep a turn open only to wait. Supports detached local jobs, durable external predicates, and optional progress checkpoints. Do not use for short foreground commands.
---

# Open Wake

Use `open-wake` to end the current turn while work continues locally. Prefer its
small detached supervisor for ordinary local commands; observe external systems
through their own durable authority.

## Workflow

1. For a small local Unix build, test, or script, launch and arm it together:

   ```console
   open-wake run \
     --label "release build" \
     --timeout 1h \
     --checkpoint-every 15m \
     -- cargo build --release
   ```

   Omit `--checkpoint-every` when only terminal completion or timeout matters;
   it creates a model turn on every checkpoint.
2. For CI, deployments, services, containers, remote jobs, or an existing
   supervisor, define a fast, read-only, idempotent predicate and use `arm`:

   ```console
   open-wake arm \
     --label "deployment" \
     --timeout 1h \
     --poll-every 10s \
     --check-timeout 5s \
     --checkpoint-every 15m \
     -- deployment-status --ready
   ```

   `--poll-every` only invokes the local predicate and never wakes the model.
   Use `--checkpoint-every` only when periodic model-visible progress is
   genuinely required. It must be at least 1m; prefer 5m or longer.

3. Confirm that `run` or `arm` succeeded, briefly tell the user what job,
   condition, deadline, and checkpoint interval were registered, and end the
   turn immediately. Do not call `status`, `sleep`, or another polling command
   in that turn.
4. On a progress checkpoint, get the full log path with `open-wake logs` and
   inspect only the needed slice with native tools such as `tail`, `sed`, or
   `rg`. Do not print the whole log into the conversation. If work is healthy,
   finish the turn normally; the same condition remains armed and the same job
   keeps running. There is no `continue` command.
5. On terminal continuation, inspect the recorded exit code, signal, timeout,
   or failure before deciding whether the original task succeeded.
6. If the user returns manually instead of a hook continuation, run
   `open-wake status --json` and inspect `watcher`. An active condition with
   `attempts: 0` after its deadline, or while its attached job is terminal or
   stale, means the Codex host never invoked the `Stop` hook; do not call that
   an end-to-end success. A `waiting` condition with `watcher.state: stale`
   means its hook was interrupted: report that, finish the turn normally, and
   let the next Stop invocation recover it. Run `open-wake doctor` for evidence;
   use `open-wake cancel` only when abandoning notifications, not as a prefix
   for every new `run`. If doctor reports hook setup or trust problems, ask the
   user to review `/hooks`, trust the exact command, and restart Codex before
   arming another condition.

`open-wake cancel` makes the condition terminal immediately and permits a new
condition in the same Codex session. It stops future wake-ups but does not
terminate a supervised command. Inspect the attached job before using `run`
again; `job_error` means its outcome is unknown and requires inspection through
the recorded job authority. Do not accidentally launch a duplicate while the
cancelled job is still running. If a checkpoint reveals incorrect work, cancel
notifications only if appropriate and stop the command through its actual
execution authority after verifying the target.

## Predicate contract

- Exit `0` must mean ready; every other exit status means not ready yet.
- Keep checks cheap because they may run many times.
- Print only concise result evidence. The final check output is bounded and may
  be included in the continuation prompt.
- Never mutate production state, retry deployment actions, or perform cleanup
  in a predicate.
- Never include credentials or secrets in predicate arguments or output.
- Prefer durable signals such as a supervisor status, exit-code file, sentinel
  file, or purpose-built status script over parsing volatile terminal output.

Use `open-wake status --json` to diagnose state. `open-wake logs [JOB_ID]`
prints only an absolute path. Only one condition may be active per Codex
session.

If setup is missing or stale, run `open-wake doctor`. Apply the exact
`open-wake setup --scope user|project` command it recommends, then review and
trust the installed hook with `/hooks`. Setup enables the hook but never grants
trust silently, so restart Codex after setup and verify both states in `/hooks`.
Treat stale-job warnings as uncertain: inspect the log and process evidence,
and never assume the child stopped.
