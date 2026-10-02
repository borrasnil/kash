//! `kash run` — run a script module in a running session.
//!
//! Small scripts go inline as a single-line base64 wrapper; larger ones
//! upload to a remote temp file first. Either way the payload runs
//! verbatim on the target (never obfuscated) and output is framed by the
//! usual `exec` markers, so plain `print`/`echo` is the whole protocol.

use std::collections::HashMap;
use std::io::{IsTerminal, Write as _};

use anyhow::Context;

use crate::agent;
use crate::cli::{OutputFormat, RunArgs};
use crate::script::{self, ModuleShell};
use crate::util::{gen_temp_nonce, json_str};

pub async fn cmd_run(args: RunArgs) -> anyhow::Result<()> {
    // Session must exist before touching anything else.
    let sock = agent::socket_path(&args.session);
    if !std::path::Path::new(&sock).exists() {
        anyhow::bail!("session '{}' not found", args.session);
    }

    let module = script::find(&args.module).ok_or_else(|| {
        let available: Vec<String> = script::discover()
            .iter()
            .map(|m| m.name.clone())
            .collect();
        let hint = if available.is_empty() {
            "no modules found (drop .sh/.ps1/.py files into ./modules or ~/.kash/modules)".to_string()
        } else {
            format!("available: {}", available.join(", "))
        };
        anyhow::anyhow!("module '{}' not found — {hint}", args.module)
    })?;
    let body = std::fs::read_to_string(&module.path)
        .with_context(|| format!("cannot read module '{}'", module.path.display()))?;

    let vars = parse_set(&args.set)?;
    let rendered = script::render(&body, &vars)?;

    check_shell_compat(&module, &args.session)?;

    if !args.yes {
        confirm(&module, &args.session, &rendered)?;
    }

    let display = format!("run {}", module.name);
    if rendered.len() <= script::INLINE_LIMIT {
        let wrapper = inline_wrapper(&module, &rendered);
        let ipc = format!("{}{}\x00{}", agent::RUN_CMD_PREFIX, display, wrapper);
        return finish(ipc, &args).await;
    }

    // Upload tier: stage the rendered script remotely, run it, clean up.
    let nonce = gen_temp_nonce();
    let remote = script::remote_temp(module.shell, &nonce);
    let local = stage_local(&module.name, &nonce, &rendered)?;
    let upload_ipc = format!(
        "{}{}\x00{}",
        agent::UPLOAD_CMD_PREFIX,
        local.display(),
        remote
    );
    let (out, code) = agent::send_command(&args.session, &upload_ipc)
        .await
        .with_context(|| format!("failed to reach session '{}'", args.session))?;
    let _ = std::fs::remove_file(&local);
    if code != 0 {
        anyhow::bail!("module staging failed: {out}");
    }
    let launcher = match module.shell {
        ModuleShell::Windows => script::ps_file_launcher(&remote),
        _ => script::bash_file_launcher(&script::sh_quote(&remote)),
    };
    let ipc = format!("{}{}\x00{}", agent::RUN_CMD_PREFIX, display, launcher);
    finish(ipc, &args).await
}

async fn finish(ipc: String, args: &RunArgs) -> anyhow::Result<()> {
    let (output, exit_code) = agent::send_command(&args.session, &ipc)
        .await
        .with_context(|| format!("failed to reach session '{}'", args.session))?;
    match args.output_format() {
        OutputFormat::Text => {
            print!("{output}");
            let _ = std::io::stdout().flush();
            std::process::exit(exit_code);
        }
        OutputFormat::Json => {
            println!(
                "{{\"output\":{},\"exit_code\":{}}}",
                json_str(&output),
                exit_code,
            );
            std::process::exit(exit_code);
        }
    }
}

fn parse_set(items: &[String]) -> anyhow::Result<HashMap<String, String>> {
    let mut vars = HashMap::new();
    for item in items {
        let (k, v) = item.split_once('=').ok_or_else(|| {
            anyhow::anyhow!("--set expects KEY=VAL, got '{item}'")
        })?;
        if k.trim().is_empty() {
            anyhow::bail!("--set expects KEY=VAL, got '{item}'");
        }
        vars.insert(k.to_string(), v.to_string());
    }
    Ok(vars)
}

fn check_shell_compat(module: &script::Module, session: &str) -> anyhow::Result<()> {
    let info = agent::read_session_info(session);
    let session_shell = info.shell.to_lowercase();
    // Python delivery is POSIX-only in v1 (bash pipe / /tmp staging).
    let ext = module
        .path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();
    if ext == "py" && session_shell == "windows" {
        anyhow::bail!(
            "module '{}' is Python and needs a linux session (session '{session}' is windows)",
            module.name
        );
    }
    if module.shell == ModuleShell::Any {
        return Ok(());
    }
    let want = module.shell.as_str();
    if session_shell == "auto" || session_shell == "?" || session_shell.is_empty() {
        return Ok(());
    }
    if session_shell != want {
        anyhow::bail!(
            "module '{}' needs a {want} shell, session '{session}' is {session_shell}",
            module.name
        );
    }
    Ok(())
}

/// Show what would run and ask. Skipped for `--yes` and non-TTY stdin
/// (scripted use must not block on a prompt it cannot see).
fn confirm(module: &script::Module, session: &str, rendered: &str) -> anyhow::Result<()> {
    if !std::io::stdin().is_terminal() {
        return Ok(());
    }
    let tier = if rendered.len() <= script::INLINE_LIMIT {
        "inline"
    } else {
        "upload"
    };
    eprintln!(
        "Module  {}  \x1b[2m({} · {} bytes · {tier})\x1b[0m",
        module.name,
        module.shell.as_str(),
        rendered.len(),
    );
    eprintln!("Session {session}");
    eprintln!("\x1b[2m--- script preview ---\x1b[0m");
    let lines: Vec<&str> = rendered.lines().collect();
    for line in lines.iter().take(20) {
        eprintln!("  {line}");
    }
    if lines.len() > 20 {
        eprintln!("  \x1b[2m… ({} more lines)\x1b[0m", lines.len() - 20);
    }
    eprint!("Run? [y/N] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    let _ = std::io::stdin().read_line(&mut answer);
    match answer.trim().to_lowercase().as_str() {
        "y" | "yes" => Ok(()),
        _ => anyhow::bail!("aborted"),
    }
}

/// Pick the inline runner from the file extension (authoritative —
/// it also decided the default shell), with a shebang sniff as fallback.
fn inline_wrapper(module: &script::Module, rendered: &str) -> String {
    let ext = module
        .path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();
    if ext == "py"
        || rendered
            .lines()
            .next()
            .map(|l| l.starts_with("#!") && l.contains("python"))
            .unwrap_or(false)
    {
        return script::python_inline(rendered);
    }
    match module.shell {
        ModuleShell::Windows => script::ps_inline(rendered),
        _ => script::bash_inline(rendered),
    }
}

/// Write the rendered script to a local temp file for the upload tier.
fn stage_local(module: &str, nonce: &str, rendered: &str) -> anyhow::Result<std::path::PathBuf> {
    let path = std::env::temp_dir().join(format!(".kash-run-{module}-{nonce}"));
    std::fs::write(&path, rendered)
        .with_context(|| format!("cannot stage {}", path.display()))?;
    Ok(path)
}
