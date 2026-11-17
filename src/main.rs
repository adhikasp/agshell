//! agshell CLI.

use std::io::{Read, Write};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};

use agshell::report::Report;
use agshell::session::{Options, Session};
use agshell::sys::become_subreaper;

/// A supervised execution shell for coding agents.
#[derive(Debug, Parser)]
#[command(name = "agshell", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Mode>,

    /// Bash script to supervise.
    #[arg(short = 'c', long = "command", value_name = "SCRIPT")]
    script_flag: Option<String>,

    /// Bash script to supervise (positional form).
    #[arg(value_name = "SCRIPT")]
    script: Vec<String>,

    /// Per-statement timeout, e.g. 500ms, 30s, 5m. 0 disables it.
    #[arg(long, default_value = "30s", value_parser = parse_duration, value_name = "DURATION")]
    timeout: Duration,

    /// Print a report instead of the JSON envelope.
    #[arg(long)]
    human: bool,

    /// Captured stdout limit in bytes.
    #[arg(long = "max-stdout", default_value_t = 65536, value_name = "BYTES")]
    max_stdout: usize,

    /// Stop the script at the first failure instead of continuing.
    #[arg(long)]
    strict: bool,

    /// Analyse the script without running it.
    #[arg(long)]
    check: bool,
}

#[derive(Debug, Subcommand)]
enum Mode {
    /// Serve requests on stdin, one JSON object per line.
    Serve {
        /// Per-statement timeout, e.g. 500ms, 30s, 5m. 0 disables it.
        #[arg(long, default_value = "30s", value_parser = parse_duration, value_name = "DURATION")]
        timeout: Duration,

        /// Captured stdout limit in bytes.
        #[arg(long = "max-stdout", default_value_t = 65536, value_name = "BYTES")]
        max_stdout: usize,

        /// Stop each script at the first failure instead of continuing.
        #[arg(long)]
        strict: bool,

    },
}

#[tokio::main]
async fn main() -> ExitCode {
    // Timeouts kill the process tree, and we need reparented grandchildren to
    // still be findable when we walk it.
    become_subreaper();

    let cli = Cli::parse();

    if let Some(Mode::Serve {
        timeout,
        max_stdout,
        strict,
    }) = cli.command
    {
        let opts = Options {
            timeout,
            max_stdout,
            strict,
        };
        return match agshell::serve::run(opts).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("agshell: serve: {e}");
                ExitCode::from(2)
            }
        };
    }

    let opts = Options {
        timeout: cli.timeout,
        max_stdout: cli.max_stdout,
        strict: cli.strict,
    };

    let session = match Session::new(opts).await {
        Ok(session) => session,
        Err(e) => {
            eprintln!("agshell: {e}");
            return ExitCode::from(2);
        }
    };

    let Some(script) = resolve_script(&cli) else {
        eprintln!(
            "usage: agshell [-c] '<bash script>' [--timeout 30s] [--human]\n       agshell serve"
        );
        return ExitCode::from(2);
    };

    if cli.check {
        let report = session.check(&script);
        print_json(&report);
        return ExitCode::SUCCESS;
    }

    drop(session);

    let mut session = match Session::new(opts).await {
        Ok(session) => session,
        Err(e) => {
            eprintln!("agshell: {e}");
            return ExitCode::from(2);
        }
    };

    let report = session.run(&script).await;
    let code = report.exit;

    if cli.human {
        print_human(&script, &report);
    } else {
        print_json(&report);
    }

    let _ = std::io::stdout().flush();
    ExitCode::from(u8::try_from(code.clamp(0, 255)).unwrap_or(1))
}

fn print_json<T: serde::Serialize>(value: &T) {
    match serde_json::to_string_pretty(value) {
        Ok(json) => println!("{json}"),
        Err(e) => eprintln!("agshell: failed to encode report: {e}"),
    }
}

fn resolve_script(cli: &Cli) -> Option<String> {
    if let Some(command) = &cli.script_flag {
        return Some(command.clone());
    }
    if !cli.script.is_empty() {
        return Some(cli.script.join(" "));
    }
    // The README always allowed piping a script in; make it true.
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).ok()?;
    if buf.trim().is_empty() {
        None
    } else {
        Some(buf)
    }
}

fn parse_duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("empty duration".to_string());
    }

    let split = text
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(text.len());
    let (value, unit) = text.split_at(split);
    let value: f64 = value.parse().map_err(|_| format!("bad duration: {text}"))?;

    let millis = match unit.trim() {
        "" | "s" => value * 1000.0,
        "ms" => value,
        "m" => value * 60_000.0,
        "h" => value * 3_600_000.0,
        other => return Err(format!("unknown time unit: {other}")),
    };

    if millis < 0.0 {
        return Err("negative duration".to_string());
    }
    Ok(Duration::from_millis(millis.round() as u64))
}

fn print_human(script: &str, report: &Report) {
    println!("$ {script}\n");

    for command in &report.commands {
        let state = if command.skipped {
            "skipped".to_string()
        } else if command.timed_out {
            "TIMEOUT".to_string()
        } else {
            format!("status={}", command.status)
        };

        let mut tag = String::new();
        if let (Some(pid), Some(stage)) = (command.pipeline, command.stage) {
            tag.push_str(&format!(" pipe{pid}.{stage}"));
        }
        if command.delegated {
            tag.push_str(" delegated");
        }
        if command.substitution_of.is_some() {
            tag.push_str(" substitution");
        }
        if let Some(parent) = command.inside {
            tag.push_str(&format!(" in #{parent}"));
        }
        if command.inferred {
            tag.push_str(" inferred");
        }

        println!(
            "  #{:<2} {:<34} {:<9} {:>5}ms{}",
            command.i,
            truncate(&command.text, 34),
            state,
            command.ms,
            tag
        );
        if !command.stderr.is_empty() {
            println!("       stderr: {}", truncate(command.stderr.trim(), 100));
        }
    }

    print!(
        "\nclass={} exit={} {}ms",
        report.class, report.exit, report.duration_ms
    );
    if !report.hidden_failures.is_empty() {
        print!(" hidden_failures={:?}", report.hidden_failures);
    }
    if report.stdout_truncated {
        print!(" stdout_truncated");
    }
    if report.inner_omitted > 0 {
        print!(" inner_omitted={}", report.inner_omitted);
    }
    if report.opaque_substitution {
        print!(" opaque_substitution");
    }
    if report.aborted {
        print!(" aborted_at={}", report.stopped_at.unwrap_or(0));
    }
    println!();

    let stdout = report.stdout.trim_end_matches('\n');
    if !stdout.is_empty() {
        println!("\n--- stdout ---\n{stdout}");
    }
    if !report.error.is_empty() {
        println!("\n--- error ---\n{}", report.error);
    }
}

fn truncate(text: &str, limit: usize) -> String {
    let flat = text.replace('\n', " ");
    if flat.chars().count() > limit {
        let head: String = flat.chars().take(limit).collect();
        format!("{head}…")
    } else {
        flat
    }
}
