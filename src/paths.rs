//! rfscan's configuration directory: `~/.config/rfscan` (on Windows
//! `C:\Users\USER\.config\rfscan`), on other systems `$XDG_CONFIG_HOME/rfscan` when the variable
//! is set. It holds the channel, exclusion and activity files and any files added later.

use std::path::PathBuf;

const APP: &str = "rfscan";

/// The configuration directory, or `None` if the home directory cannot be determined.
pub fn config_dir() -> Option<PathBuf> {
    // XDG Base Directory: a relative XDG_CONFIG_HOME is invalid and is ignored.
    if !cfg!(windows)
        && let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    {
        return Some(xdg.join(APP));
    }
    directories::BaseDirs::new().map(|d| d.home_dir().join(".config").join(APP))
}
