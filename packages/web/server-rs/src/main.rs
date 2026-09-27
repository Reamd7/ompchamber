//! Entry point: CLI dispatch (`packages/web/bin` port). The serve command
//! runs the server composition in-process; everything else is a CLI command.
//!
//! 中文说明：进程入口只做一件事 —— 调用 CLI 层分发子命令；`serve`
//! 在进程内组装并运行 axum 服务器，其余子命令均为一次性 CLI 命令。

/// 进程入口：执行 `ompchamber_server::cli::run` 完成子命令分发，
/// 并把其返回的退出码转换为进程退出状态。
#[tokio::main]
async fn main() -> std::process::ExitCode {
    let code = ompchamber_server::cli::run().await;
    std::process::ExitCode::from(code as u8)
}
