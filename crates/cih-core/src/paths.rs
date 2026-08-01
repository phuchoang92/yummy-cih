use std::path::{Path, PathBuf};

/// Platform-aware locations for CIH's user-scoped state.
///
/// `CIH_HOME` is the portable override on every platform. New Windows installs
/// use `%LOCALAPPDATA%\CIH`; Unix keeps the historical `$HOME/.cih` layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CihPaths {
    home: PathBuf,
}

impl CihPaths {
    /// Discover the CIH state root from the current process environment.
    pub fn discover() -> Option<Self> {
        Self::from_environment(
            std::env::var_os("CIH_HOME").map(PathBuf::from),
            std::env::var_os("LOCALAPPDATA").map(PathBuf::from),
            std::env::var_os("HOME").map(PathBuf::from),
            cfg!(windows),
        )
    }

    /// Construct paths below an explicit state root.
    pub fn from_home(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn graphs(&self) -> PathBuf {
        self.home.join("graphs")
    }

    pub fn logs(&self) -> PathBuf {
        self.home.join("logs")
    }

    pub fn cache(&self) -> PathBuf {
        self.home.join("cache")
    }

    #[cfg(windows)]
    pub fn legacy_windows_home() -> Option<PathBuf> {
        std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .map(|home| home.join(".cih"))
    }

    fn from_environment(
        override_home: Option<PathBuf>,
        local_app_data: Option<PathBuf>,
        unix_home: Option<PathBuf>,
        windows: bool,
    ) -> Option<Self> {
        let nonempty = |path: PathBuf| (!path.as_os_str().is_empty()).then_some(path);
        if let Some(home) = override_home.and_then(nonempty) {
            return Some(Self::from_home(home));
        }
        if windows {
            return local_app_data
                .and_then(nonempty)
                .map(|root| Self::from_home(root.join("CIH")));
        }
        unix_home
            .and_then(nonempty)
            .map(|root| Self::from_home(root.join(".cih")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_override_wins_on_every_platform() {
        let paths = CihPaths::from_environment(
            Some(PathBuf::from("custom")),
            Some(PathBuf::from("local")),
            Some(PathBuf::from("home")),
            true,
        )
        .unwrap();
        assert_eq!(paths.home(), Path::new("custom"));
    }

    #[test]
    fn windows_uses_local_app_data() {
        let paths = CihPaths::from_environment(
            None,
            Some(PathBuf::from(r"C:\Users\Ada\AppData\Local")),
            Some(PathBuf::from(r"C:\Users\Ada")),
            true,
        )
        .unwrap();
        assert_eq!(
            paths.home(),
            Path::new(r"C:\Users\Ada\AppData\Local").join("CIH")
        );
        assert_eq!(paths.graphs(), paths.home().join("graphs"));
    }

    #[test]
    fn unix_preserves_dot_cih_home() {
        let paths = CihPaths::from_environment(
            None,
            Some(PathBuf::from("ignored")),
            Some(PathBuf::from("/home/ada")),
            false,
        )
        .unwrap();
        assert_eq!(paths.home(), Path::new("/home/ada/.cih"));
    }

    #[test]
    fn missing_required_platform_root_is_none() {
        assert!(CihPaths::from_environment(None, None, Some("home".into()), true).is_none());
        assert!(CihPaths::from_environment(None, Some("local".into()), None, false).is_none());
    }
}
