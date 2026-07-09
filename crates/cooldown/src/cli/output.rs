use cooldown_core::CoreError;
use std::io::Write;

pub(in crate::cli) fn stdout(text: &str) -> Result<(), CoreError> {
    write_stdout(text.as_bytes())
}

pub(in crate::cli) fn stdout_line(text: &str) -> Result<(), CoreError> {
    let mut out = std::io::stdout().lock();
    if let Err(error) = out
        .write_all(text.as_bytes())
        .and_then(|()| out.write_all(b"\n"))
    {
        return handle_stdout_error(error);
    }
    Ok(())
}

fn write_stdout(bytes: &[u8]) -> Result<(), CoreError> {
    match std::io::stdout().lock().write_all(bytes) {
        Ok(()) => Ok(()),
        Err(error) => handle_stdout_error(error),
    }
}

fn handle_stdout_error(error: std::io::Error) -> Result<(), CoreError> {
    if error.kind() == std::io::ErrorKind::BrokenPipe {
        Ok(())
    } else {
        Err(error.into())
    }
}
