//! `kash modules` — list available script modules.

use crate::cli::ModulesArgs;
use crate::script;
use crate::util::json_str;

pub fn cmd_modules(args: ModulesArgs) -> anyhow::Result<()> {
    let modules = script::discover();

    if args.json {
        print!("[");
        for (i, m) in modules.iter().enumerate() {
            if i > 0 {
                print!(",");
            }
            print!(
                "{{\"name\":{},\"shell\":{},\"desc\":{},\"path\":{},\"source\":{}}}",
                json_str(&m.name),
                json_str(m.shell.as_str()),
                json_str(&m.desc),
                json_str(&m.path.to_string_lossy()),
                json_str(&m.source),
            );
        }
        println!("]");
        return Ok(());
    }

    if modules.is_empty() {
        println!("no modules found");
        println!("\x1b[2m(drop .sh/.ps1/.py files into ./modules or ~/.kash/modules)\x1b[0m");
        return Ok(());
    }

    const W_NAME: usize = 18;
    const W_SHELL: usize = 8;
    println!(
        "\x1b[2m{name:<W_NAME$}  {shell:<W_SHELL$}  DESCRIPTION\x1b[0m",
        name = "MODULE",
        shell = "SHELL",
    );
    for m in &modules {
        let desc = if m.desc.is_empty() {
            format!("\x1b[2m{}\x1b[0m", m.path.to_string_lossy())
        } else {
            m.desc.clone()
        };
        println!(
            "\x1b[1;32m{:<W_NAME$}\x1b[0m  {:<W_SHELL$}  {}",
            m.name,
            m.shell.as_str(),
            desc,
        );
    }

    Ok(())
}
