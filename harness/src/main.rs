//! Harness command entry point.
use cs_harness::cli;
use std::process::ExitCode;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match cli::execute_from(std::env::args().skip(1), None).await {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("cs: {error}");
            ExitCode::from(2)
        }
    }
}
