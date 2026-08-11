use std::io::ErrorKind;
use std::process::ExitCode;

/// Exit codes are part of the documented contract:
/// `0` success (including "no results"), `1` runtime error, `2` usage error.
/// clap exits with 2 on its own before `run()` ever returns.
fn main() -> ExitCode {
    match webseek::run() {
        Ok(()) => ExitCode::SUCCESS,
        // A closed pipe is not a failure. `webseek fetch … | head` is the
        // documented way to use this tool; reporting EPIPE as an error made
        // every such pipeline look like it had failed.
        Err(e) if is_broken_pipe(&e) => ExitCode::SUCCESS,
        Err(e) => {
            // `{:#}` prints the message chain via Display — no Debug dump and
            // no backtrace in the user's face.
            eprintln!("webseek: error: {e:#}");
            ExitCode::from(1)
        }
    }
}

/// Did this error come from the reader at the other end of the pipe hanging up?
fn is_broken_pipe(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == ErrorKind::BrokenPipe)
    })
}
