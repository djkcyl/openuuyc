//! Registration markers shared by the producer and account-device classifier.
pub(crate) const BOARD: &str = "OpenUUYC Virtual Device";
pub(crate) const CPU: &str = "Virtual CPU (8 Core)";
pub(crate) const VIDEO: &str = "Virtual Display Adapter";

pub(crate) fn matches<'a>(fields: impl IntoIterator<Item = (&'a str, &'a str)>) -> bool {
    let (mut board, mut cpu, mut video) = (false, false, false);
    for (key, value) in fields {
        match key.trim() {
            "主板" => {
                board = matches!(
                    value.trim(),
                    BOARD | "OpenUUYC Viewer" | "Virtual Baseboard"
                )
            }
            "处理器" => cpu = value.trim() == CPU,
            "显卡" => video = value.trim() == VIDEO,
            _ => {}
        }
    }
    // These detail labels are provided by the verified UU management endpoint.
    // Exact markers, not generic VM detection or a search of the device alias.
    // Account IDs, RAM size and OS version are not identity markers.
    board && cpu && video
}
