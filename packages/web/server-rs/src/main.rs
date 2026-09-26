//! Entry point: CLI dispatch (`packages/web/bin` port). The serve command
//! runs the server composition in-process; everything else is a CLI command.

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let code = ompchamber_server::cli::run().await;
    std::process::ExitCode::from(code as u8)
}
