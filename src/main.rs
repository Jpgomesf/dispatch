use std::process::ExitCode;

use harness::cli;
use harness::paths::EnvPaths;
use harness::session::ClaudeCli;

#[tokio::main]
async fn main() -> ExitCode {
    let code = cli::run(
        std::env::args_os(),
        EnvPaths::from_process(),
        ClaudeCli::default,
    )
    .await;
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
