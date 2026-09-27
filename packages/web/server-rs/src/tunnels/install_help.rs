//! Port of `server/lib/tunnels/install-help.js`: provider/platform install
//! command metadata for missing tunnel dependencies.
//! （中文说明）维护各 tunnel provider（cloudflared/ngrok）在各平台上
//! 的安装指引：依赖名、安装命令（brew/winget/下载链接）与缺依赖时的
//! 提示文案，供可用性检查与 doctor 诊断接口直接返回给前端展示。

use serde_json::{Value, json};

use super::types::{TUNNEL_PROVIDER_CLOUDFLARE, TUNNEL_PROVIDER_NGROK};

/// 单个 provider 的静态安装元数据表项。
struct ProviderInstallInfo {
    /// 依赖二进制名（cloudflared/ngrok），用于提示文案开头。
    dependency: &'static str,
    /// 官方下载页地址（无包管理器命令的平台直接给出该链接）。
    install_url: &'static str,
    /// 三个平台的安装命令，顺序固定为 darwin、win32、linux。
    commands: [&'static str; 3], // [darwin, win32, linux]
}

/// 静态安装信息表：cloudflare 在前（索引 0，兼作未知 provider 的兜底
/// 项），ngrok 在后；运行时只读。
const PROVIDER_INSTALL_INFO: [(&str, ProviderInstallInfo); 2] = [
    (
        TUNNEL_PROVIDER_CLOUDFLARE,
        ProviderInstallInfo {
            dependency: "cloudflared",
            install_url: "https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/downloads/",
            commands: [
                "brew install cloudflared",
                "winget install --id Cloudflare.cloudflared",
                "Download cloudflared from https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/downloads/",
            ],
        },
    ),
    (
        TUNNEL_PROVIDER_NGROK,
        ProviderInstallInfo {
            dependency: "ngrok",
            install_url: "https://ngrok.com/download",
            commands: [
                "brew install ngrok",
                "winget install ngrok -s msstore",
                "Download ngrok from https://ngrok.com/download",
            ],
        },
    ),
];

/// 把平台标识归一到 darwin/win32/linux 之一；未知值（如 sunos、空串）
/// 一律按 linux 处理，与 JS 版兜底行为一致。
fn normalize_install_platform(platform: &str) -> &'static str {
    match platform {
        "darwin" => "darwin",
        "win32" => "win32",
        "linux" => "linux",
        _ => "linux",
    }
}

/// 拼接缺依赖提示：安装命令以 "Download " 开头时直接跟在句点后
/// （其本身已是完整指引句），否则加 "Install it with: " 前缀。
fn create_missing_dependency_message(dependency: &str, install_command: &str) -> String {
    if install_command.starts_with("Download ") {
        format!("{dependency} is not installed. {install_command}")
    } else {
        format!("{dependency} is not installed. Install it with: {install_command}")
    }
}

/// `getTunnelDependencyInstallInfo(provider, platform)`.
///
/// 查表返回 provider 在指定平台的安装信息：provider 未命中时退回
/// cloudflare 表项，平台未知时按 linux 处理；同时生成面向用户的
/// 消息文案。
pub fn get_tunnel_dependency_install_info(provider: &str, platform: &str) -> InstallInfo {
    let provider_info = &PROVIDER_INSTALL_INFO
        .iter()
        .find(|(id, _)| *id == provider)
        .map(|(_, info)| info)
        .unwrap_or(&PROVIDER_INSTALL_INFO[0].1);
    let normalized_platform = normalize_install_platform(platform);
    let install_command = match normalized_platform {
        "darwin" => provider_info.commands[0],
        "win32" => provider_info.commands[1],
        _ => provider_info.commands[2],
    };

    InstallInfo {
        dependency: provider_info.dependency.to_string(),
        install_command: install_command.to_string(),
        install_url: provider_info.install_url.to_string(),
        platform: normalized_platform.to_string(),
        message: create_missing_dependency_message(provider_info.dependency, install_command),
    }
}

/// 面向调用方（routes/doctor、前端 UI）的安装指引快照，字段与 JS 版
/// 返回对象一一对应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallInfo {
    /// 依赖二进制名，如 cloudflared、ngrok。
    pub dependency: String,
    /// 当前平台的安装命令（brew/winget 或下载指引句）。
    pub install_command: String,
    /// 官方下载页 URL。
    pub install_url: String,
    /// 归一化后的平台标识（darwin/win32/linux）。
    pub platform: String,
    /// 拼装好的"依赖未安装"完整提示文案。
    pub message: String,
}

/// 将 `InstallInfo` 序列化为 camelCase 键（installCommand/installUrl）
/// 的 JSON 对象，与 JS API 响应字段名保持一致。
impl From<&InstallInfo> for Value {
    /// 逐字段映射；键名是对外 API 契约，不可改动。
    fn from(info: &InstallInfo) -> Self {
        json!({
            "dependency": info.dependency,
            "installCommand": info.install_command,
            "installUrl": info.install_url,
            "platform": info.platform,
            "message": info.message,
        })
    }
}

/// 覆盖各平台安装命令选择、未知 provider/平台的兜底规则与消息文案格式。
#[cfg(test)]
mod tests {
    use super::*;

    /// 行为契约：win32 下 cloudflared 给出 winget 安装命令并出现在提示文案中。
    #[test]
    fn returns_windows_cloudflared_winget_guidance() {
        let info = get_tunnel_dependency_install_info(TUNNEL_PROVIDER_CLOUDFLARE, "win32");
        assert_eq!(info.dependency, "cloudflared");
        assert_eq!(
            info.install_command,
            "winget install --id Cloudflare.cloudflared"
        );
        assert!(info.message.contains("Cloudflare.cloudflared"));
    }

    /// 行为契约：win32 下 ngrok 给出 msstore 源的 winget 命令。
    #[test]
    fn returns_windows_ngrok_winget_guidance() {
        let info = get_tunnel_dependency_install_info(TUNNEL_PROVIDER_NGROK, "win32");
        assert_eq!(info.dependency, "ngrok");
        assert_eq!(info.install_command, "winget install ngrok -s msstore");
        assert!(info.message.contains("ngrok -s msstore"));
    }

    /// 行为契约：darwin 下提示使用 Homebrew 安装 cloudflared。
    #[test]
    fn keeps_macos_homebrew_guidance() {
        let info = get_tunnel_dependency_install_info(TUNNEL_PROVIDER_CLOUDFLARE, "darwin");
        assert_eq!(info.install_command, "brew install cloudflared");
        assert_eq!(
            info.message,
            "cloudflared is not installed. Install it with: brew install cloudflared"
        );
    }

    /// 行为契约：linux 下安装命令与下载 URL 均指向 Cloudflare 官方下载页。
    #[test]
    fn returns_current_linux_cloudflared_download_guidance() {
        let info = get_tunnel_dependency_install_info(TUNNEL_PROVIDER_CLOUDFLARE, "linux");
        let download_url = "https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/downloads/";
        assert_eq!(info.install_url, download_url);
        assert_eq!(
            info.install_command,
            format!("Download cloudflared from {download_url}")
        );
        assert!(info.message.contains(download_url));
    }

    /// 行为契约：未知平台归一为 linux 并给出下载指引。
    #[test]
    fn unknown_platform_falls_back_to_linux() {
        let info = get_tunnel_dependency_install_info(TUNNEL_PROVIDER_NGROK, "sunos");
        assert_eq!(info.platform, "linux");
        assert_eq!(
            info.install_command,
            "Download ngrok from https://ngrok.com/download"
        );
    }

    /// 行为契约：未知 provider 回退到 cloudflare 表项。
    #[test]
    fn unknown_provider_falls_back_to_cloudflare() {
        let info = get_tunnel_dependency_install_info("bogus", "darwin");
        assert_eq!(info.dependency, "cloudflared");
    }
}
