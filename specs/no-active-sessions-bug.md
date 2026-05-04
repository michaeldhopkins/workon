# Spec: workon fails on a fresh machine with no zellij sessions

## Bug

On a machine that has never run zellij (or has had every session reaped),
running `workon` aborts immediately:

```
$ workon
Error: `zellij list-sessions --no-formatting` exited with exit status: 1: No active zellij sessions found.
```

This makes the *first* invocation of workon on any new machine impossible —
which is precisely the case we should handle most gracefully, since by
definition no prior session can exist for us to attach to.

## Root cause

`src/session.rs::session_exists` calls
`zellij list-sessions --no-formatting` to check whether a session named
`name` is already running. The result is matched as:

```rust
match Cmd::new("zellij").args(...).run() {
    Ok(output)               => /* parse stdout */,
    Err(e) if e.is_timeout() => /* recover wedged IPC */,
    Err(e)                   => Err(e.into()),   // <-- bug
}
```

Zellij signals "no sessions exist" by exiting **1** with the stderr line
`No active zellij sessions found.` (verified against zellij 0.43.1). That
falls into the catch-all `Err(e) => Err(e.into())` arm and is propagated
as a fatal `anyhow::Error`, even though semantically it is the same as
"yes I checked, our session does not exist" — i.e. `Ok(false)`.

The error is reachable via two distinct entry points but only one is
buggy:

| Caller                     | Behavior on `NonZeroExit("No active zellij sessions found.")` |
|----------------------------|---------------------------------------------------------------|
| `session_exists`           | Propagates the error → `workon` aborts. **Buggy.**            |
| `preflight_responsive`     | Already swallowed: only `is_timeout()` triggers recovery.     |

So the fix is scoped to `session_exists`.

## Fix

Recognize the no-sessions response as a successful negative answer.

Add a small classifier:

```rust
fn is_no_sessions_error(err: &RunError) -> bool {
    err.stderr()
        .is_some_and(|s| s.contains("No active zellij sessions"))
}
```

Match it ahead of the generic `Err(e)` arm in `session_exists`:

```rust
Err(ref e) if e.is_timeout()        => { recover_session(name)?; Ok(false) }
Err(ref e) if is_no_sessions_error(e) => Ok(false),
Err(e)                              => Err(e.into()),
```

### Why match on stderr substring rather than exit code

Exit code 1 alone is too generic — zellij returns 1 for many other
failures (missing config, invalid arguments, IPC error). Suppressing all
exit-1s would mask real problems. The stderr sentinel
`"No active zellij sessions"` is the canonical signal; matching on it
keeps the negative-response path narrow and lets every other failure
mode continue to surface as an error.

We deliberately match the **prefix** "No active zellij sessions" rather
than the full sentence so a punctuation tweak in a future zellij release
("...found" vs "...found.") doesn't silently re-break this path.

## Test plan

Unit test for the classifier (the part we can test in isolation):

- `is_no_sessions_error` returns `true` for `RunError::NonZeroExit` whose
  stderr contains the sentinel.
- Returns `false` for `NonZeroExit` with unrelated stderr.
- Returns `false` for `Spawn` and `Timeout` variants.

Integration coverage of `session_exists` end-to-end against a fresh
zellij would require fully sandboxing the user's `ZELLIJ_SOCKET_DIR`
*and* the resurrection cache directory, which leaks too much of zellij's
internal layout into our test setup. The classifier test plus manual
verification on a no-session machine is the practical bar.

## Manual verification

After the fix, on a machine with no live zellij sessions:

```
$ workon
# launches a fresh session; no error
```

If zellij prints a different stderr (e.g. real IPC failure), the error
still surfaces — only the specific "no sessions" string is suppressed.
