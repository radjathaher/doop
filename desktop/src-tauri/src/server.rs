// Which doop server the shell wraps. The released installers talk to the
// hosted service, but the same binary can be pointed at a self-hosted
// instance at launch, without rebuilding. The first source that yields a
// valid origin wins:
//
//   1. the DOOP_APP_URL environment variable of the running process;
//   2. a `server.json` file holding `{"url": "https://doop.example.com"}` in
//      the app's config directory (macOS: ~/Library/Application Support/
//      design.doop.desktop, Windows: %APPDATA%\design.doop.desktop, Linux:
//      ~/.config/design.doop.desktop);
//   3. DOOP_APP_URL at compile time (`DOOP_APP_URL=… bun run build`);
//   4. https://doop.design.
//
// An invalid value is reported on stderr and skipped, so a typo in the file
// falls back to the next source rather than leaving the window blank. Only
// the origin is kept: a path in the value is ignored, because the shell
// always opens <origin>/auth.
//
// The bundled capability (capabilities/default.json) grants the shell's IPC
// to the official origins only. For any other server, `grant_ipc` adds the
// same permissions at runtime, scoped to that origin, so a self-hosted page
// gets window dragging, the opener, deep links and the local Claude bridge
// exactly as the hosted one does.

use std::sync::OnceLock;
use tauri::ipc::CapabilityBuilder;
use tauri::{AppHandle, Manager, Url};

/// The hosted service, which the bundled capability already covers.
pub const OFFICIAL: &str = "https://doop.design";
const OFFICIAL_WWW: &str = "https://www.doop.design";

const ENV_VAR: &str = "DOOP_APP_URL";
const CONFIG_FILE: &str = "server.json";

/// The shell's own commands and plugin permissions, as listed in
/// capabilities/default.json. A unit test keeps the two lists in step.
const PERMISSIONS: &[&str] = &[
    "core:default",
    "core:window:allow-start-dragging",
    "core:window:allow-close",
    "deep-link:default",
    "allow-claude-status",
    "allow-claude-connect",
    "allow-claude-login",
    "allow-claude-install",
    "allow-claude-choose-cli",
    "allow-claude-reset-cli",
    "allow-claude-run",
    "allow-claude-stop",
    "allow-claude-add-extension",
    "allow-codex-add-mcp",
    "allow-fonts-choose-local",
    "allow-fonts-read-local",
    "allow-view-menu-state",
    "allow-update-card-ready",
    "allow-update-restart",
];
const OPENER_PERMISSION: &str = "opener:allow-open-url";
const OPENER_ALLOW: &[&str] = &["https://**", "http://**"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Env,
    ConfigFile,
    Build,
    Default,
}

struct Server {
    /// Scheme, host and port only, with no trailing slash.
    origin: String,
    source: Source,
}

static SERVER: OnceLock<Server> = OnceLock::new();

/// Resolves the server once, before anything reads it. Called first in
/// `setup`, because the config directory needs the app handle.
pub fn init(app: &AppHandle) {
    let from_env = std::env::var(ENV_VAR).ok().map(|v| (v, Source::Env));
    let from_file = read_config_file(app).map(|v| (v, Source::ConfigFile));
    let from_build = option_env!("DOOP_APP_URL").map(|v| (v.to_string(), Source::Build));
    let resolved = [from_env, from_file, from_build]
        .into_iter()
        .flatten()
        .find_map(|(raw, source)| match normalize(&raw) {
            Some(origin) => Some(Server { origin, source }),
            None => {
                eprintln!("doop: ignoring server URL {raw:?} from {source:?}: expected an http(s) origin");
                None
            }
        })
        .unwrap_or_else(|| Server {
            origin: OFFICIAL.to_string(),
            source: Source::Default,
        });
    let _ = SERVER.set(resolved);
}

fn read_config_file(app: &AppHandle) -> Option<String> {
    let path = app.path().app_config_dir().ok()?.join(CONFIG_FILE);
    let text = std::fs::read_to_string(&path).ok()?;
    let parsed: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(err) => {
            eprintln!("doop: ignoring {}: {err}", path.display());
            return None;
        }
    };
    parsed.get("url")?.as_str().map(str::to_string)
}

/// Reduces a configured value to its origin, accepting https anywhere and
/// plain http only on loopback, where a local instance has no certificate.
fn normalize(raw: &str) -> Option<String> {
    let url = Url::parse(raw.trim()).ok()?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    match url.scheme() {
        "https" => {}
        "http" if loopback => {}
        _ => return None,
    }
    url.host_str()?;
    Some(url.origin().ascii_serialization())
}

fn server() -> &'static Server {
    SERVER.get_or_init(|| Server {
        origin: OFFICIAL.to_string(),
        source: Source::Default,
    })
}

/// The configured origin, used for in-shell navigation and IPC scope.
pub fn origin() -> &'static str {
    &server().origin
}

/// Where the window loads pages from. A debug build always targets the local
/// vite server; a second worktree runs its dev pair on other ports
/// (vite.config.ts) and DOOP_DEV_URL points a dev shell at it. Keep
/// tauri.conf.json's devUrl in step so IPC keeps treating the pages as local.
pub fn base_url() -> String {
    if cfg!(debug_assertions) {
        option_env!("DOOP_DEV_URL")
            .unwrap_or("http://localhost:4300")
            .trim_end_matches('/')
            .to_string()
    } else {
        origin().to_string()
    }
}

fn is_official(origin: &str) -> bool {
    origin == OFFICIAL || origin == OFFICIAL_WWW
}

/// Official updates install the official build. They keep a server chosen at
/// launch (the environment or server.json survive the update), but would
/// silently drop one baked in at compile time, so those builds never update.
pub fn updates_allowed() -> bool {
    let server = server();
    server.source != Source::Build || is_official(&server.origin)
}

/// Grants the shell's IPC to a self-hosted origin. The official origins are
/// already covered by capabilities/default.json, so they need nothing here.
pub fn grant_ipc(app: &AppHandle) -> tauri::Result<()> {
    let origin = origin();
    if is_official(origin) {
        return Ok(());
    }
    let allowed: Vec<serde_json::Value> = OPENER_ALLOW
        .iter()
        .map(|url| serde_json::json!({ "url": url }))
        .collect();
    let capability = PERMISSIONS
        .iter()
        .fold(
            CapabilityBuilder::new("shell-self-hosted")
                .window("main")
                .local(false)
                .remote(origin.to_string()),
            |capability, permission| capability.permission(*permission),
        )
        .permission_scoped(OPENER_PERMISSION, allowed, Vec::new());
    app.add_capability(capability)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_keeps_only_the_origin() {
        assert_eq!(
            normalize("https://doop.example.com/auth?x=1").as_deref(),
            Some("https://doop.example.com")
        );
        assert_eq!(
            normalize(" https://doop.example.com:8443/ ").as_deref(),
            Some("https://doop.example.com:8443")
        );
        assert_eq!(normalize("http://localhost:4300").as_deref(), Some("http://localhost:4300"));
    }

    #[test]
    fn normalize_rejects_insecure_and_malformed_values() {
        assert_eq!(normalize("http://doop.example.com"), None);
        assert_eq!(normalize("doop.example.com"), None);
        assert_eq!(normalize("file:///etc/passwd"), None);
        assert_eq!(normalize(""), None);
    }

    /// The runtime capability must grant exactly what the bundled one does,
    /// or a self-hosted page would silently lose (or gain) shell features.
    #[test]
    fn permissions_match_the_bundled_capability() {
        let bundled: serde_json::Value =
            serde_json::from_str(include_str!("../capabilities/default.json")).unwrap();
        let mut plain = Vec::new();
        let mut scoped = Vec::new();
        for entry in bundled["permissions"].as_array().unwrap() {
            match entry.as_str() {
                Some(id) => plain.push(id.to_string()),
                None => scoped.push(entry.clone()),
            }
        }
        let mut expected: Vec<String> = PERMISSIONS.iter().map(|p| p.to_string()).collect();
        plain.sort();
        expected.sort();
        assert_eq!(plain, expected);

        let opener = serde_json::json!({
            "identifier": OPENER_PERMISSION,
            "allow": OPENER_ALLOW.iter().map(|url| serde_json::json!({ "url": url })).collect::<Vec<_>>(),
        });
        assert_eq!(scoped, vec![opener]);

        let remote = bundled["remote"]["urls"].as_array().unwrap();
        assert!(remote.iter().any(|url| url == OFFICIAL));
        assert!(remote.iter().any(|url| url == OFFICIAL_WWW));
    }
}
