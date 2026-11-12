use chrono::Utc;

#[cfg(target_os = "linux")]
pub const DEFAULT_TAPE_EXCLUDE_DIRS: &[&str] =
    &["/proc", "/sys", "/dev", "/run", "/nix/store", "/snap"];
#[cfg(target_os = "macos")]
pub const DEFAULT_TAPE_EXCLUDE_DIRS: &[&str] = &["/System"];
#[cfg(target_os = "windows")]
pub const DEFAULT_TAPE_EXCLUDE_DIRS: &[&str] = &[
    "C:\\Windows",
    "C:\\Program Files",
    "C:\\Program Files (x86)",
    "C:\\ProgramData",
];
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub const DEFAULT_TAPE_EXCLUDE_DIRS: &[&str] = &[];

pub fn format_time_suffix() -> String {
    Utc::now().format("%H%M%S").to_string()
}

pub fn graph_value(value: &impl std::fmt::Debug) -> String {
    format!("{value:?}")
        .chars()
        .enumerate()
        .flat_map(|(index, character)| {
            (character.is_uppercase() && index > 0)
                .then_some('_')
                .into_iter()
                .chain(character.to_lowercase())
        })
        .collect()
}

pub fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
