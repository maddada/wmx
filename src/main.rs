#[cfg(windows)]
mod attachment;
#[cfg(windows)]
mod callbacks;
#[cfg(windows)]
mod cli;
#[cfg(windows)]
mod client;
#[cfg(windows)]
mod console_input;
#[cfg(windows)]
mod daemon;
#[cfg(windows)]
mod display;
#[cfg(windows)]
mod history;
#[cfg(windows)]
mod input;
#[cfg(windows)]
mod launch;
#[cfg(windows)]
mod process_owner;
#[cfg(windows)]
mod process_snapshot;
#[cfg(windows)]
mod protocol;
#[cfg(windows)]
mod title_events;

fn main() {
    #[cfg(windows)]
    if let Err(error) = cli::run() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
    #[cfg(not(windows))]
    {
        eprintln!("wmx requires Windows. Use zmx on macOS or Linux.");
        std::process::exit(1);
    }
}
