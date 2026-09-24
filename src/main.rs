use std::process::ExitCode;

use harness::cli;
use harness::session::ClaudeCli;

#[tokio::main]
async fn main() -> ExitCode {
    let env_config = std::env::var(harness::paths::CONFIG_ENV).ok();
    let code = cli::run(std::env::args_os(), env_config, ClaudeCli::default).await;
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
