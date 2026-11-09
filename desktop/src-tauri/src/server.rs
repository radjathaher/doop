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
// On macOS, doop ▸ Server… edits server.json from a native sheet, like the
// iOS app's Settings ▸ Server, and restarts the shell on the new server.
//
// Only a whole HTTPS origin is accepted, by the same rule as the iOS app's
// ServerAddress: no credentials, query, fragment or path. An invalid value is
// reported on stderr and skipped, so a typo in the file falls back to the next
// source rather than leaving the window blank.
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

/// Accepts a whole HTTPS origin and returns it without a trailing slash.
fn normalize(raw: &str) -> Option<String> {
    let url = Url::parse(raw.trim()).ok()?;
    let whole_origin = url.scheme() == "https"
        && url.host_str().is_some_and(|host| !host.is_empty())
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && url.path() == "/";
    whole_origin.then(|| url.origin().ascii_serialization())
}

/// Saves the server for the next launch, replacing the file in one step so a
/// crash mid-write cannot leave half a file behind.
fn write_config_file(app: &AppHandle, origin: &str) -> std::io::Result<()> {
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::NotFound, err.to_string()))?;
    std::fs::create_dir_all(&dir)?;
    let staged = dir.join(format!("{CONFIG_FILE}.tmp"));
    let body = serde_json::to_string_pretty(&serde_json::json!({ "url": origin }))?;
    std::fs::write(&staged, body + "\n")?;
    std::fs::rename(&staged, dir.join(CONFIG_FILE))
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

/// doop ▸ Server…: the desktop counterpart of the iOS app's Settings ▸ Server.
/// It is a native sheet rather than a page, so no web content, from any
/// server, can reach the code that changes the server.
#[cfg(target_os = "macos")]
pub mod menu {
    use super::*;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSAlert, NSTextField};
    use objc2_foundation::{NSSize, NSString};
    use tauri::menu::MenuItem;

    pub const MENU_ID: &str = "server";
    const LABEL: &str = "Server…";
    const FOOTER: &str = "Doop Cloud or your own HTTPS server. Your account and canvases stay on that server.";
    const INVALID: &str = "Enter an HTTPS address, such as https://doop.example.com.";

    // NSAlert numbers its buttons from NSAlertFirstButtonReturn (1000) in the
    // order they are added.
    const SAVE: isize = 1000;
    const CANCEL: isize = 1001;

    pub fn item(app: &AppHandle) -> tauri::Result<MenuItem<crate::ShellRuntime>> {
        MenuItem::with_id(app, MENU_ID, LABEL, true, None::<&str>)
    }

    /// Menu events run on the UI thread, which the modal sheet requires.
    pub fn open(app: &AppHandle) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let current = origin().to_string();
        let mut value = current.clone();
        let mut note = FOOTER;
        loop {
            let (choice, typed) = ask(mtm, &value, note);
            match choice {
                SAVE => match normalize(&typed) {
                    Some(chosen) if chosen == current => return,
                    Some(chosen) => {
                        if let Err(err) = write_config_file(app, &chosen) {
                            eprintln!("doop: could not save {CONFIG_FILE}: {err}");
                            value = typed;
                            note = "Could not save the server. Check that the app can write to its settings folder.";
                            continue;
                        }
                        // The IPC grant and the in-window hosts are fixed for
                        // the life of the process, so the new server starts
                        // fresh, as it does on iOS.
                        app.request_restart();
                        return;
                    }
                    None => {
                        value = typed;
                        note = INVALID;
                    }
                },
                CANCEL => return,
                _ => {
                    value = OFFICIAL.to_string();
                    note = FOOTER;
                }
            }
        }
    }

    fn ask(mtm: MainThreadMarker, value: &str, note: &str) -> (isize, String) {
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str("Server"));
        alert.setInformativeText(&NSString::from_str(note));
        alert.addButtonWithTitle(&NSString::from_str("Save"));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));
        alert.addButtonWithTitle(&NSString::from_str("Use Doop Cloud"));
        let field = NSTextField::new(mtm);
        field.setFrameSize(NSSize::new(320.0, 24.0));
        field.setStringValue(&NSString::from_str(value));
        field.setPlaceholderString(Some(&NSString::from_str(OFFICIAL)));
        alert.setAccessoryView(Some(&field));
        alert.window().setInitialFirstResponder(Some(&field));
        let choice = alert.runModal();
        (choice, field.stringValue().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_accepts_a_whole_https_origin() {
        assert_eq!(normalize("https://doop.example.com").as_deref(), Some("https://doop.example.com"));
        assert_eq!(
            normalize(" https://Doop.Example.com:8443/ ").as_deref(),
            Some("https://doop.example.com:8443")
        );
    }

    /// The same rule as the iOS app's ServerAddress.
    #[test]
    fn normalize_rejects_anything_but_a_whole_https_origin() {
        for raw in [
            "http://doop.example.com",
            "http://localhost:4300",
            "doop.example.com",
            "file:///etc/passwd",
            "https://doop.example.com/auth",
            "https://doop.example.com/?x=1",
            "https://doop.example.com/#top",
            "https://user:secret@doop.example.com",
            "",
        ] {
            assert_eq!(normalize(raw), None, "{raw}");
        }
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
