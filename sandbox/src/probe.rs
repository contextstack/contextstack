//! `--probe`: what this host can enforce.

use serde_json::{Value, json};

/// What the Linux probe found beyond "enforceable": the bwrap binary, whether it can create an
/// unprivileged user namespace here, and Ubuntu's `AppArmor` restriction value.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Default)]
pub struct LinuxFacts {
    /// The bwrap binary, when installed.
    pub bwrap: Option<String>,
    /// `bwrap --unshare-user --ro-bind / / true` succeeded.
    pub userns: bool,
    /// `kernel.apparmor_restrict_unprivileged_userns`, when the knob exists.
    pub apparmor_restrict_unprivileged_userns: Option<i64>,
}

/// The probe document on Linux. `limits` stay false: the wrapper sets no rlimit (`RLIMIT_AS` could
/// be enforced here, but no policy field asks for one yet, and `RLIMIT_NPROC` counts per user, not
/// per call tree). Nested user namespaces are always refused inside (`--disable-userns`).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn linux_document(enforceable: bool, facts: &LinuxFacts) -> Value {
    json!({
        "backend": if enforceable { "bwrap" } else { "none" },
        "platform": "linux",
        "fs": enforceable,
        "net": {"none": enforceable, "gateway": enforceable, "all": enforceable},
        "limits": {"memory": false, "processes": false, "wall_clock": "caller"},
        "bwrap": facts.bwrap,
        "userns": facts.userns,
        "nested_userns": false,
        "apparmor_restrict_unprivileged_userns": facts.apparmor_restrict_unprivileged_userns,
    })
}

/// The probe document. `enforceable` is true when the Seatbelt backend is usable here.
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub fn document(enforceable: bool) -> Value {
    let backend = if enforceable { "seatbelt" } else { "none" };
    let platform = std::env::consts::OS;
    json!({
        "backend": backend,
        "platform": if platform == "macos" { "macos".to_owned() } else { platform.to_owned() },
        "fs": enforceable,
        "net": {"none": enforceable, "gateway": enforceable, "all": enforceable},
        "limits": {"memory": false, "processes": false, "wall_clock": "caller"},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforceable_probe_names_seatbelt_and_no_limits() {
        let value = document(true);
        assert_eq!(value["backend"], "seatbelt");
        assert_eq!(value["fs"], true);
        assert_eq!(value["net"]["gateway"], true);
        assert_eq!(value["limits"]["memory"], false);
        assert_eq!(value["limits"]["processes"], false);
        assert_eq!(value["limits"]["wall_clock"], "caller");
    }

    #[test]
    fn linux_probe_names_bwrap_and_what_the_host_allows() {
        let facts = LinuxFacts {
            bwrap: Some("/usr/bin/bwrap".to_owned()),
            userns: true,
            apparmor_restrict_unprivileged_userns: Some(1),
        };
        let value = linux_document(true, &facts);
        assert_eq!(value["backend"], "bwrap");
        assert_eq!(value["platform"], "linux");
        assert_eq!(value["userns"], true);
        assert_eq!(value["nested_userns"], false);
        assert_eq!(value["apparmor_restrict_unprivileged_userns"], 1);
        assert_eq!(value["limits"]["memory"], false);
        let blocked = linux_document(false, &LinuxFacts::default());
        assert_eq!(blocked["backend"], "none");
        assert_eq!(blocked["fs"], false);
        assert!(blocked["bwrap"].is_null());
        assert!(blocked["apparmor_restrict_unprivileged_userns"].is_null());
    }

    #[test]
    fn unenforceable_probe_keeps_the_shape_with_backend_none() {
        let value = document(false);
        assert_eq!(value["backend"], "none");
        assert_eq!(value["fs"], false);
        assert_eq!(value["net"]["none"], false);
        assert_eq!(value["limits"]["wall_clock"], "caller");
    }
}
