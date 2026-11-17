//! agshell — a supervised execution shell for coding agents.
//!
//! It parses a bash script, then runs it one element at a time so that the
//! outcome of every command and every pipeline stage survives as data. brush
//! is the evaluator: words, expansions, builtins, pipelines and redirections
//! are its job. agshell owns the topology.

mod report;
mod runner;

use std::io::{Read, Write};
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;

use report::Report;
use runner::{Options, Runner, become_subreaper};

/// A supervised execution shell for coding agents.
#[derive(Debug, Parser)]
#[command(name = "agshell", version, about, long_about = None)]
struct Cli {
    /// Bash script to supervise.
    #[arg(short = 'c', long = "command", value_name = "SCRIPT")]
    command: Option<String>,

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
}

#[tokio::main]
async fn main() -> ExitCode {
    // Timeouts kill the process tree, and we need reparented grandchildren
    // to still be findable when we walk it.
    become_subreaper();

    let cli = Cli::parse();
    let script = match resolve_script(&cli) {
        Some(s) => s,
        None => {
            eprintln!("usage: agshell [-c] '<bash script>' [--timeout 30s] [--human]");
            return ExitCode::from(2);
        }
    };

    let opts = Options {
        timeout: cli.timeout,
        max_stdout: cli.max_stdout,
    };

    let runner = match Runner::new(opts).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("agshell: {e}");
            return ExitCode::from(2);
        }
    };

    let report = runner.run(&script).await;
    let code = report.exit;

    if cli.human {
        print_human(&script, &report);
    } else {
        match serde_json::to_string_pretty(&report) {
            Ok(json) => println!("{json}"),
            Err(e) => eprintln!("agshell: failed to encode report: {e}"),
        }
    }

    let _ = std::io::stdout().flush();
    ExitCode::from(u8::try_from(code.clamp(0, 255)).unwrap_or(1))
}

fn resolve_script(cli: &Cli) -> Option<String> {
    if let Some(command) = &cli.command {
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
    if report.opaque_substitution {
        print!(" opaque_substitution");
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
