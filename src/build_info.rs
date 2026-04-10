pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const GIT_COMMIT: &str = match option_env!("ZEROCLAW_GIT_COMMIT") {
    Some(commit) => commit,
    None => "unknown",
};

pub fn version_tag() -> String {
    format!("v{VERSION} ({})", short_commit())
}

pub fn long_version_tag() -> String {
    format!("v{VERSION} ({GIT_COMMIT})")
}

fn short_commit() -> &'static str {
    GIT_COMMIT.get(..7).unwrap_or(GIT_COMMIT)
}
