use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use aven_core::{Diagnostic as AvenDiagnostic, Severity};
use clap::{Parser, Subcommand};
use codespan_reporting::diagnostic::{Diagnostic, Label, Severity as CodespanSeverity};
use codespan_reporting::files::SimpleFiles;
use codespan_reporting::term::termcolor::{ColorChoice, StandardStream};
use codespan_reporting::term::{Config, emit_to_write_style};

#[derive(Debug, Parser)]
#[command(name = "aven")]
#[command(about = "Aven language tooling")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Parse a file and report diagnostics.
    Check {
        /// Source file to check.
        path: PathBuf,
    },

    /// Format a source file.
    Fmt {
        /// Check formatting without writing changes.
        #[arg(long)]
        check: bool,

        /// Source file to format.
        path: PathBuf,
    },

    /// Start the language server on stdin/stdout.
    Lsp,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Check { path } => check(&path),
        Command::Fmt { check, path } => fmt(&path, check),
        Command::Lsp => {
            aven_lsp::run_stdio().await;
            Ok(())
        }
    }
}

fn check(path: &Path) -> Result<()> {
    let source =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let output = aven_parser::parse_module(&source);

    if !output.diagnostics.is_empty() {
        print_diagnostics(path, &source, &output.diagnostics)?;
    }

    if output.diagnostics.iter().any(AvenDiagnostic::is_error) {
        bail!("check failed");
    }

    println!(
        "{}: ok (parse checks only; semantic analysis is not implemented yet)",
        path.display()
    );
    Ok(())
}

fn fmt(path: &Path, check: bool) -> Result<()> {
    let source =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let formatted = aven_fmt::format_source(&source);

    if source == formatted {
        return Ok(());
    }

    if check {
        bail!("{} is not formatted", path.display());
    }

    fs::write(path, formatted).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn print_diagnostics(path: &Path, source: &str, diagnostics: &[AvenDiagnostic]) -> Result<()> {
    let mut files = SimpleFiles::new();
    let file_id = files.add(path.display().to_string(), source);
    let writer = StandardStream::stderr(ColorChoice::Auto);
    let config = Config::default();

    for diagnostic in diagnostics {
        let codespan_diagnostic = to_codespan_diagnostic(file_id, diagnostic);
        emit_to_write_style(&mut writer.lock(), &config, &files, &codespan_diagnostic)
            .context("failed to print diagnostic")?;
    }

    Ok(())
}

fn to_codespan_diagnostic(file_id: usize, diagnostic: &AvenDiagnostic) -> Diagnostic<usize> {
    let severity = match diagnostic.severity {
        Severity::Error => CodespanSeverity::Error,
        Severity::Warning => CodespanSeverity::Warning,
        Severity::Note => CodespanSeverity::Note,
    };

    let labels = diagnostic
        .labels
        .iter()
        .map(|label| {
            Label::primary(
                file_id,
                label.span.start..label.span.end.max(label.span.start + 1),
            )
            .with_message(label.message.clone())
        })
        .collect();

    let mut result = Diagnostic::new(severity)
        .with_message(diagnostic.message.clone())
        .with_labels(labels)
        .with_notes(diagnostic.notes.clone());

    if let Some(code) = &diagnostic.code {
        result = result.with_code(code);
    }

    result
}
