# shell-handler

Obfuscated command handler for reverse shells. Connect to any raw shell (nc, socat, ssh), type clean commands, and each one is automatically obfuscated before hitting the wire.

No listener, no C2, no agent. It's a replacement for the netcat half of a reverse shell — stdin goes through an obfuscation layer, stdout shows remote output with ANSI noise stripped.

## Features

| | Linux targets | Windows targets |
|---|---|---|
| Light obfuscation | `${}` injection, token noise | —* |
| Medium obfuscation | Temp files, base64, eval | —* |
| Heavy obfuscation | Double base64, /dev/shm | —* |
| ANSI stripping | Display-side only | Display-side only |

\* Windows backends exist as placeholders and will be implemented in a later release.

## Install

```text
cargo build --release
```

The binary lands at `target/release/shell-handler`.

## Usage

```text
shell-handler -t <target> -p <port> [-o <level>] [-s <shell>]
```

You need something listening on the other end first — a reverse shell from a target, a bind shell you're connecting to, etc. This tool connects out to it and wraps your input.

```text
# Light obfuscation (default)
shell-handler -t 10.0.0.5 -p 4444

# Medium — temp files and base64
shell-handler -t 10.0.0.5 -p 4444 -o medium

# Heavy — double encoding
shell-handler -t 10.0.0.5 -p 4444 -o heavy
```

Once connected, type normally. Every command gets obfuscated before it reaches the shell.

## Obfuscation levels

### light
Injects `${}` into random positions, duplicates tokens, wraps words in `$()`, appends trailing comments. The command still runs correctly but looks like garbage in logs and process listings.

### medium
Cycles between three techniques:
- Writes the command to `/tmp/.<random>`, sources it, deletes the file
- Base64-encodes the command and pipes through `base64 -d | sh`
- Wraps in `eval "..."`

### heavy
Cycles between:
- Double base64: encode the command, then encode the decode pipeline
- Writes base64 to `/dev/shm/.<random>`, decodes, executes, cleans up
- Same with a named bash function layer

## Options

```text
  -t, --target <TARGET>          Target IP address
  -p, --port <PORT>              Target port
  -o, --obfuscation <LEVEL>      Obfuscation level [light, medium, heavy]
                                 [default: light]
  -s, --shell <TYPE>             Shell type [auto, linux, windows]
                                 [default: auto]
  -h, --help                     Print help
  -V, --version                  Print version
```

## How it works

```text
You type:    ls -la
     │
     ▼
Handler obfuscates:  l${}s -l${}a #junk
     │
     ▼
Socket ──────────────────────────────────► Remote shell
     │                                    │
     ◄──────────────────────────────────── Echo + output
     │
     ▼
Display:     l${}s -l${}a #junk
             file1  file2
```

The protocol is raw TCP. No metadata, no encryption, no handshake beyond what the shell itself provides.

## Notes

- The handler connects out to the shell, it doesn't listen. Use `nc -lvnp <port>` on the target or the equivalent to initiate the reverse connection.
- Output is cleaned of ANSI escape sequences and carriage returns before display. Everything else passes through.
- Ctrl+C cleanly disconnects. A second Ctrl+C kills the process.
- Temp files created by the medium strategy are cleaned up (`rm -f`). If the connection drops mid-command, the temp file may persist on the target.

## Building from source

Requires Rust 1.90+ (edition 2024).

```text
git clone <url>
cd shell-handler
cargo build --release
```

## License

MIT
