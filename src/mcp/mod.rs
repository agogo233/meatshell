//! Model Context Protocol feature.
//!
//! MCP transport, tools, resources, and protocol adapters belong in this module.

#[path = "impls/server.rs"]
mod server;
#[path = "impls/tools.rs"]
mod tools;

#[path = "impls/http.rs"]
mod http;
#[path = "impls/oauth.rs"]
mod oauth;

/// How an MCP client should start `meatshell mcp serve` on this machine:
/// the program and the arguments before `mcp serve`.
///
/// * Flatpak: the binary lives at `/app/bin` inside the sandbox, which the
///   host cannot run, so the client goes through `flatpak run <app id>`.
/// * AppImage: the `.AppImage` file itself (`current_exe` is a temporary
///   mount that disappears when the app quits).
/// * Everything else (Windows, macOS app bundle, .deb / AUR / tarball): the
///   running executable.
#[cfg_attr(feature = "headless", allow(dead_code))]
pub(crate) fn client_launch() -> (String, Vec<String>) {
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "meatshell".to_string());
    client_launch_from(env("FLATPAK_ID"), env("APPIMAGE"), exe)
}

fn client_launch_from(
    flatpak_id: Option<String>,
    appimage: Option<String>,
    exe: String,
) -> (String, Vec<String>) {
    if let Some(id) = flatpak_id {
        return ("flatpak".to_string(), vec!["run".to_string(), id]);
    }
    (appimage.unwrap_or(exe), Vec::new())
}

/// The `mcpServers` entry an MCP client needs to start `meatshell mcp serve`
/// over stdio, ready to paste into its config.
#[cfg_attr(feature = "headless", allow(dead_code))]
pub(crate) fn client_config_json(command: &str, leading_args: &[String]) -> String {
    let quote = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
    let args: Vec<String> = leading_args
        .iter()
        .map(|a| quote(a))
        .chain(["\"mcp\"".to_string(), "\"serve\"".to_string()])
        .collect();
    format!(
        "{{\n  \"meatshell\": {{\n    \"command\": {},\n    \"args\": [{}]\n  }}\n}}",
        quote(command),
        args.join(", ")
    )
}

#[cfg(test)]
mod client_config_tests {
    use super::{client_config_json, client_launch_from};

    fn parse(text: &str) -> serde_json::Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn config_is_valid_json_with_the_command_and_args() {
        let text = client_config_json(r"C:\Program Files\meatshell\meatshell.exe", &[]);
        let value = parse(&text);
        assert_eq!(
            value["meatshell"]["command"],
            r"C:\Program Files\meatshell\meatshell.exe"
        );
        assert_eq!(value["meatshell"]["args"], serde_json::json!(["mcp", "serve"]));
        // Backslashes are escaped in the text, so it pastes as valid JSON.
        assert!(text.contains(r#""C:\\Program Files\\meatshell\\meatshell.exe""#), "{text}");
    }

    #[test]
    fn unix_paths_and_quotes_survive() {
        let value = parse(&client_config_json("/opt/My \"Apps\"/meatshell", &[]));
        assert_eq!(value["meatshell"]["command"], "/opt/My \"Apps\"/meatshell");
    }

    #[test]
    fn launch_per_platform_package() {
        let exe = || "/usr/bin/meatshell".to_string();
        // .deb / AUR / tarball / macOS bundle / Windows: the executable.
        assert_eq!(client_launch_from(None, None, exe()), (exe(), vec![]));
        // AppImage: the image file, not the temporary mount.
        assert_eq!(
            client_launch_from(None, Some("/home/me/MeatShell.AppImage".into()), exe()),
            ("/home/me/MeatShell.AppImage".to_string(), vec![])
        );
        // Flatpak: through `flatpak run`.
        let (command, args) = client_launch_from(
            Some("io.github.yituorou.meatshell".into()),
            None,
            "/app/bin/meatshell".into(),
        );
        assert_eq!(command, "flatpak");
        let value = parse(&client_config_json(&command, &args));
        assert_eq!(
            value["meatshell"]["args"],
            serde_json::json!(["run", "io.github.yituorou.meatshell", "mcp", "serve"])
        );
    }
}

pub(crate) fn run(args: &[String]) -> anyhow::Result<()> {
    if let Some(index) = args.iter().position(|arg| arg == "--http-config") {
        anyhow::ensure!(index + 2 == args.len(), "--http-config requires one config path and must be last");
        anyhow::ensure!(args[3..index].iter().all(|arg| arg == "--allow-config-import"), "unknown MCP argument");
        http::run(&args[index + 1], args.iter().any(|arg| arg == "--allow-config-import"))
    } else {
        anyhow::ensure!(args[3..].iter().all(|arg| arg == "--allow-config-import"), "unknown MCP argument; use --http-config <path> for authenticated HTTP");
        server::run_stdio()
    }
}
