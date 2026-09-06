//! Shared application identity used by the daemon and desktop client.

pub const IS_STEWARD: bool = cfg!(feature = "steward");
pub const IS_ISOLATED: bool = cfg!(debug_assertions) || IS_STEWARD;

#[cfg(feature = "steward")]
pub const APP_NAME: &str = "Waku Steward";
#[cfg(feature = "steward")]
pub const APP_ID: &str = "sh.waku.steward";
#[cfg(feature = "steward")]
pub const DATA_DIRECTORY_NAME: &str = "Waku Steward";

#[cfg(all(debug_assertions, not(feature = "steward")))]
pub const APP_NAME: &str = "Waku Debug";
#[cfg(all(not(debug_assertions), not(feature = "steward")))]
pub const APP_NAME: &str = "Waku";

#[cfg(all(debug_assertions, not(feature = "steward")))]
pub const APP_ID: &str = "sh.waku.dev";
#[cfg(all(not(debug_assertions), not(feature = "steward")))]
pub const APP_ID: &str = "sh.waku";

#[cfg(all(debug_assertions, not(feature = "steward")))]
pub const DATA_DIRECTORY_NAME: &str = "Waku Debug";
#[cfg(all(not(debug_assertions), not(feature = "steward")))]
pub const DATA_DIRECTORY_NAME: &str = "Waku";

/// Waku-owned configuration and workspaces. Native harness directories stay shared.
pub fn configuration_directory() -> std::path::PathBuf {
    if IS_STEWARD {
        dirs::home_dir()
            .expect("Steward requires a home directory")
            .join(".waku-steward")
    } else if cfg!(debug_assertions) {
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

#[cfg(all(test, debug_assertions, not(feature = "steward")))]
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

#[cfg(all(test, feature = "steward"))]
mod steward_tests {
    #[test]
    fn daily_build_has_its_own_identity_and_resources() {
        use super::*;
        let root = configuration_directory();
        assert_eq!(APP_ID, "sh.waku.steward");
        assert_eq!(APP_NAME, "Waku Steward");
        assert_eq!(DATA_DIRECTORY_NAME, "Waku Steward");
        assert!(IS_ISOLATED);
        assert_eq!(root, dirs::home_dir().unwrap().join(".waku-steward"));
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
