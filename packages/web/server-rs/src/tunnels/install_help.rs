//! Port of `server/lib/tunnels/install-help.js`: provider/platform install
//! command metadata for missing tunnel dependencies.

use serde_json::{Value, json};

use super::types::{TUNNEL_PROVIDER_CLOUDFLARE, TUNNEL_PROVIDER_NGROK};

struct ProviderInstallInfo {
    dependency: &'static str,
    install_url: &'static str,
    commands: [&'static str; 3], // [darwin, win32, linux]
}

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

fn normalize_install_platform(platform: &str) -> &'static str {
    match platform {
        "darwin" => "darwin",
        "win32" => "win32",
        "linux" => "linux",
        _ => "linux",
    }
}

fn create_missing_dependency_message(dependency: &str, install_command: &str) -> String {
    if install_command.starts_with("Download ") {
        format!("{dependency} is not installed. {install_command}")
    } else {
        format!("{dependency} is not installed. Install it with: {install_command}")
    }
}

/// `getTunnelDependencyInstallInfo(provider, platform)`.
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallInfo {
    pub dependency: String,
    pub install_command: String,
    pub install_url: String,
    pub platform: String,
    pub message: String,
}

impl From<&InstallInfo> for Value {
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn returns_windows_ngrok_winget_guidance() {
        let info = get_tunnel_dependency_install_info(TUNNEL_PROVIDER_NGROK, "win32");
        assert_eq!(info.dependency, "ngrok");
        assert_eq!(info.install_command, "winget install ngrok -s msstore");
        assert!(info.message.contains("ngrok -s msstore"));
    }

    #[test]
    fn keeps_macos_homebrew_guidance() {
        let info = get_tunnel_dependency_install_info(TUNNEL_PROVIDER_CLOUDFLARE, "darwin");
        assert_eq!(info.install_command, "brew install cloudflared");
        assert_eq!(
            info.message,
            "cloudflared is not installed. Install it with: brew install cloudflared"
        );
    }

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

    #[test]
    fn unknown_platform_falls_back_to_linux() {
        let info = get_tunnel_dependency_install_info(TUNNEL_PROVIDER_NGROK, "sunos");
        assert_eq!(info.platform, "linux");
        assert_eq!(
            info.install_command,
            "Download ngrok from https://ngrok.com/download"
        );
    }

    #[test]
    fn unknown_provider_falls_back_to_cloudflare() {
        let info = get_tunnel_dependency_install_info("bogus", "darwin");
        assert_eq!(info.dependency, "cloudflared");
    }
}
