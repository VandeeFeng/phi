use std::path::PathBuf;

pub fn builtin_skills_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("SKILLS")
}

pub fn phi_config_dir() -> PathBuf {
    std::env::var_os("PHI_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".phi")
        })
}

pub fn phi_session_root() -> PathBuf {
    std::env::var_os("PHI_SESSION_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| phi_config_dir().join("projects"))
}

pub fn session_scope_dir(working_dir: &str) -> PathBuf {
    phi_session_root().join(encode_working_dir_key(working_dir))
}

pub fn encode_working_dir_key(working_dir: &str) -> String {
    let working_dir = working_dir.trim_start_matches(['/', '\\']);
    let encoded = working_dir.replace(['/', '\\', ':'], "-");
    format!("--{encoded}--")
}

pub fn expand_home_path(path: impl AsRef<str>) -> PathBuf {
    let path = path.as_ref();
    let Some(home) = dirs::home_dir() else {
        return PathBuf::from(path);
    };
    if matches!(path, "~" | "$HOME") {
        return home;
    }
    for prefix in ["~/", "$HOME/"] {
        if let Some(rest) = path.strip_prefix(prefix) {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}
