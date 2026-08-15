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
     --check-every 15m \
     -- cargo build --release
   ```

   Omit `--check-every` when only terminal completion or timeout matters.
2. For CI, deployments, services, containers, remote jobs, or an existing
   supervisor, define a fast, read-only, idempotent predicate and use `arm`:

   ```console
   open-wake arm \
     --label "deployment" \
     --timeout 1h \
     --interval 10s \
     --check-timeout 5s \
     -- deployment-status --ready
   ```

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
   `open-wake status --json`. An active condition past its deadline with
   `attempts: 0` means the Codex host never invoked the `Stop` hook; do not call
   that an end-to-end success. Run `open-wake cancel` to prevent a later wake,
   then use `open-wake doctor` and ask the user to review `/hooks`, trust the
   exact command, and restart Codex if setup changed.

`open-wake cancel` stops future wake-ups but does not terminate a supervised
command. If a checkpoint reveals incorrect work, cancel notifications only if
appropriate and stop the command through its actual execution authority after
verifying the target.

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
trust the installed hook with `/hooks`. Treat stale-job warnings as uncertain:
inspect the log and process evidence, and never assume the child stopped.
