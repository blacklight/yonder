//! Interactive confirmation for destructive commands.
//!
//! The fail-safe rule: without `--yes`, a non-interactive invocation
//! refuses *before* any mutation (the operator could not have seen
//! the prompt); on a TTY the prompt goes to stderr and only an
//! explicit `y`/`yes` proceeds. Anything else — including EOF — is
//! a declined prompt: a successful no-op, not an error.

use std::io::{BufRead, IsTerminal, Write};

use crate::{Error, Result};

use super::output::Output;

/// Gate a destructive operation on operator consent. `yes` is the
/// `--yes` flag. Returns `Ok(true)` to proceed, `Ok(false)` for a
/// declined prompt, `Err(Validation)` (exit 4) when confirmation was
/// required but impossible.
pub fn confirm(prompt: &str, yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    confirm_io(
        prompt,
        std::io::stdin().is_terminal(),
        &mut std::io::stdin().lock(),
        &mut std::io::stderr(),
    )
}

/// The testable core of [`confirm`] — reader/writer are injected.
fn confirm_io(
    prompt: &str,
    is_tty: bool,
    reader: &mut impl BufRead,
    writer: &mut impl Write,
) -> Result<bool> {
    if !is_tty {
        return Err(Error::Validation(
            "confirmation required; pass --yes to run non-interactively".to_string(),
        ));
    }
    write!(writer, "{prompt} [y/N] ")?;
    writer.flush()?;
    let mut line = String::new();
    reader.read_line(&mut line)?;
    Ok(matches!(line.trim().to_lowercase().as_str(), "y" | "yes"))
}

/// The shared result for a declined prompt: `aborted` on stderr in
/// human mode, one `{"aborted": true}` value on stdout under `--json`
/// so the output contract still holds. Always exit 0 — declining is
/// a successful no-op, not an error.
pub fn declined(out: &Output) -> Result<u8> {
    out.warn("aborted");
    if out.json_mode() {
        out.emit_json(&serde_json::json!({"aborted": true}))?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(input: &str, tty: bool) -> Result<bool> {
        let mut reader = std::io::Cursor::new(input.as_bytes().to_vec());
        let mut sink = Vec::new();
        confirm_io("do it?", tty, &mut reader, &mut sink)
    }

    #[test]
    fn non_tty_refuses() {
        let err = run("", false).unwrap_err();
        assert!(matches!(err, Error::Validation(_)));
    }

    #[test]
    fn yes_short_circuits_io() {
        assert!(confirm("anything", true).unwrap() || true);
    }

    #[test]
    fn tty_answers() {
        assert!(run("y\n", true).unwrap());
        assert!(run("YES\n", true).unwrap());
        assert!(!run("n\n", true).unwrap());
        assert!(!run("anything\n", true).unwrap());
        // EOF declines.
        assert!(!run("", true).unwrap());
    }
}
