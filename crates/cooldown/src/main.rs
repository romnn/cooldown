//! The `cooldown` binary entry point: install error reporting, parse, run on a tokio runtime, and
//! exit with the policy taxonomy's code.

use cooldown::cli::{Cli, run};
use cooldown_core::interrupt;

/// The conventional status of a process stopped by `SIGINT` (128 + 2).
const INTERRUPTED_EXIT: u8 = 130;

/// How long a child process may take to exit after the termination request before it is killed.
const CHILD_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

fn main() -> std::process::ExitCode {
    let _ = color_eyre::install();
    let (cli, overrides) = Cli::parse_with_overrides();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: failed to start runtime: {e}");
            return std::process::ExitCode::from(4);
        }
    };

    let exit = runtime.block_on(async {
        // Installed before any work starts, so no signal can arrive before cooldown handles it.
        match Signals::install() {
            Some(signals) => drop(tokio::spawn(listen_for_interrupts(signals))),
            None => eprintln!(
                "cooldown: warning: cannot handle termination signals; an interrupted run may \
                 leave its in-progress changes behind"
            ),
        }
        run(cli, overrides).await
    });
    if interrupt::requested() {
        return std::process::ExitCode::from(INTERRUPTED_EXIT);
    }
    // Exit codes are the fixed 0..=4 taxonomy, so the conversion never saturates.
    std::process::ExitCode::from(u8::try_from(exit.code()).unwrap_or(1))
}

/// Turns the first termination signal into a cooperative [`interrupt::request`] and a second one
/// into an immediate exit.
///
/// Dying on the first signal would skip the rollback of whatever trial is mutating the project in
/// place, so the run is instead left to unwind through its error paths.
/// A child that ignores the termination request is killed after [`CHILD_GRACE`], since the unwind
/// waits for it.
/// The second signal is the escape hatch for a run that is slow to get there.
async fn listen_for_interrupts(mut signals: Signals) {
    while signals.recv().await {
        if interrupt::request() {
            eprintln!(
                "cooldown: interrupted; stopping child processes and rolling back in-progress \
                 changes (signal again to quit immediately)"
            );
            tokio::spawn(async {
                tokio::time::sleep(CHILD_GRACE).await;
                interrupt::kill_remaining();
            });
        } else {
            eprintln!("cooldown: quitting without finishing the rollback");
            std::process::exit(i32::from(INTERRUPTED_EXIT));
        }
    }
}

/// The termination signals cooldown handles: `SIGINT`, `SIGTERM`, and `SIGHUP`.
#[cfg(unix)]
struct Signals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl Signals {
    /// Installs the handlers, or `None` when the platform refuses one.
    fn install() -> Option<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Some(Signals {
            interrupt: signal(SignalKind::interrupt()).ok()?,
            terminate: signal(SignalKind::terminate()).ok()?,
            hangup: signal(SignalKind::hangup()).ok()?,
        })
    }

    /// Waits for the next signal; `false` once no more can arrive.
    async fn recv(&mut self) -> bool {
        tokio::select! {
            received = self.interrupt.recv() => received.is_some(),
            received = self.terminate.recv() => received.is_some(),
            received = self.hangup.recv() => received.is_some(),
        }
    }
}

/// The console interrupt, the one termination signal Windows delivers to a console process.
#[cfg(windows)]
struct Signals {
    ctrl_c: tokio::signal::windows::CtrlC,
}

#[cfg(windows)]
impl Signals {
    /// Installs the handler, or `None` when the platform refuses it.
    fn install() -> Option<Self> {
        Some(Signals {
            ctrl_c: tokio::signal::windows::ctrl_c().ok()?,
        })
    }

    /// Waits for the next console interrupt; `false` once no more can arrive.
    async fn recv(&mut self) -> bool {
        self.ctrl_c.recv().await.is_some()
    }
}
