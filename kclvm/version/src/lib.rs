//! Copyright The KCL Authors. All rights reserved.

pub const VERSION: &str = include_str!("./../../../VERSION");
pub const CHECK_SUM: &str = "c020ab3eb4b9179219d6837a57f5d323";
pub const GIT_SHA: &str = env!("VERGEN_GIT_SHA");

/// Get kCL full version string with the format `{version}-{check_sum}`.
#[inline]
pub fn get_version_string() -> String {
    format!("{}-{}", VERSION, CHECK_SUM)
}

/// Get version info including version string, platform.
#[inline]
pub fn get_version_info() -> String {
    format!(
        "Version: {}\r\nPlatform: {}\r\nGitCommit: {}",
        get_version_string(),
        env!("VERGEN_RUSTC_HOST_TRIPLE"),
        env!("VERGEN_GIT_SHA")
    )
}
