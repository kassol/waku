//! Shared application identity used by the daemon and desktop client.

#[cfg(debug_assertions)]
pub const APP_NAME: &str = "Waku Debug";
#[cfg(not(debug_assertions))]
pub const APP_NAME: &str = "Waku";

#[cfg(debug_assertions)]
pub const APP_ID: &str = "sh.waku.dev";
#[cfg(not(debug_assertions))]
pub const APP_ID: &str = "sh.waku";

#[cfg(debug_assertions)]
pub const DATA_DIRECTORY_NAME: &str = "Waku Debug";
#[cfg(not(debug_assertions))]
pub const DATA_DIRECTORY_NAME: &str = "Waku";

/// Waku-owned configuration and workspaces. Native harness directories stay shared.
pub fn configuration_directory() -> std::path::PathBuf {
    if cfg!(debug_assertions) {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("protocol crate is inside the workspace")
            .join("temp")
    } else {
        dirs::home_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(".waku")
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    #[test]
    fn debug_resources_stay_outside_installed_waku() {
        let root = super::configuration_directory();
        assert!(root.ends_with("temp"), "{}", root.display());
        assert_eq!(crate::projectless::home_directory(), None);
        assert_eq!(
            crate::DaemonSettings::default_path(),
            root.join("settings.json")
        );
        assert_eq!(
            crate::projectless::workspace_root(),
            Some(root.join("projects"))
        );
    }
}
