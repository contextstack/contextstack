//! Additional grants for one invocation. The caller consumes the Director claim before launch.

use crate::{
    cli::Widening,
    failure::{Failure, unenforceable},
    policy::{Effective, HostPolicy, Net},
};
use std::path::{Path, PathBuf};

pub(crate) const DEFAULT_RESERVED_PORTS: [u16; 2] = [8780, 9200];

pub fn apply(
    effective: &mut Effective,
    grant: &Widening,
    host: &HostPolicy,
    tool: &Path,
    args: &[String],
) -> Result<Vec<PathBuf>, Failure> {
    let loopback = !grant.loopback_ports.is_empty();
    for port in &grant.loopback_ports {
        if *port == 0
            || DEFAULT_RESERVED_PORTS.contains(port)
            || grant.reserved_ports.contains(port)
        {
            return Err(unenforceable(format!(
                "loopback port {port} is reserved or invalid"
            )));
        }
    }
    if grant.network && loopback {
        return Err(unenforceable("conflicting action network widenings"));
    }
    if grant.network || loopback {
        let net = if loopback { Net::Loopback } else { Net::All };
        if !host.allow_net.contains(&net) && !host.allow_net.contains(&Net::All) {
            return Err(unenforceable(
                "host policy does not support a network widening",
            ));
        }
        effective.net = net;
        effective.loopback_ports.clone_from(&grant.loopback_ports);
        effective.loopback_ports.sort_unstable();
        effective.loopback_ports.dedup();
    }
    let mut deny_unlink = Vec::new();
    for path in &grant.write_paths {
        // Bind the existing exact path. Refuse a renamed or symlinked target rather than
        // widening to a different path or to the parent of a file that does not exist.
        if !path.is_absolute()
            || path.parent().is_none()
            || path.canonicalize().as_ref().ok() != Some(path)
        {
            return Err(unenforceable(format!(
                "action write target is not a canonical existing path: {}",
                path.display()
            )));
        }
        // The call's parent is the job scratch directory for action invocations.
        // Scratch files can be replaced or unlinked just like files in the workspace.
        if !grant.delete
            && !path.starts_with(&effective.workspace)
            && effective
                .tmp
                .parent()
                .is_none_or(|tmp| !path.starts_with(tmp))
        {
            if cfg!(target_os = "linux") && !path.is_file() && !creates_directory(tool, args, path)
            {
                return Err(unenforceable(
                    "on Linux, a writable directory bind also requires a delete grant",
                ));
            }
            deny_unlink.push(path.clone());
        }
        effective.read.push(path.clone());
        effective.write.push(path.clone());
    }
    Ok(deny_unlink)
}

// Linux cannot distinguish unlink from write on a bind. The standalone mkdir command only
// creates a direct child of the bound directory and does not remove or replace anything.
fn creates_directory(tool: &Path, args: &[String], parent: &Path) -> bool {
    let [separator, target] = args else {
        return false;
    };
    let trusted = [Path::new("/bin/mkdir"), Path::new("/usr/bin/mkdir")];
    separator == "--" && trusted.contains(&tool) && Path::new(target).parent() == Some(parent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widening_changes_only_the_selected_effective_policy() -> Result<(), String> {
        let temp = tempfile::tempdir().map_err(|e| e.to_string())?;
        let root = temp.path().canonicalize().map_err(|e| e.to_string())?;
        let workspace = root.join("work");
        let job_tmp = root.join("job-tmp");
        let call_tmp = job_tmp.join("call");
        let write = root.join("granted");
        std::fs::create_dir(&workspace).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(&call_tmp).map_err(|e| e.to_string())?;
        std::fs::write(&write, "").map_err(|e| e.to_string())?;
        let host: HostPolicy =
            serde_json::from_str(r#"{"version":1,"runtime_read":[],"allow_net":["none","all"]}"#)
                .map_err(|e| e.to_string())?;
        let baseline = || {
            crate::policy::intersect(
                &host,
                &crate::policy::Request::default(),
                &workspace,
                &call_tmp,
                &root,
                &root,
                &|_: &str| None,
            )
            .map(|(effective, _)| effective)
            .map_err(|e| e.message().to_owned())
        };
        let mut approved = baseline()?;
        let deny_unlink = apply(
            &mut approved,
            &Widening {
                network: true,
                loopback_ports: vec![],
                reserved_ports: vec![],
                delete: false,
                write_paths: vec![write.clone()],
            },
            &host,
            Path::new("/bin/echo"),
            &[],
        )
        .map_err(|e| e.message().to_owned())?;
        assert_eq!(approved.net, Net::All);
        assert!(approved.write.contains(&write));
        assert_eq!(deny_unlink, std::slice::from_ref(&write));
        for port in [0, 8780, 9200, 9001] {
            let error = apply(
                &mut baseline()?,
                &Widening {
                    network: false,
                    loopback_ports: vec![3000, port],
                    reserved_ports: vec![9001],
                    delete: false,
                    write_paths: vec![],
                },
                &host,
                Path::new("/bin/echo"),
                &[],
            )
            .err()
            .ok_or_else(|| format!("reserved or invalid localhost port {port} was granted"))?;
            assert!(error.message().contains(&format!("port {port}")));
        }
        let ordinary = baseline()?;
        let mut loopback = baseline()?;
        let scratch_unlinks = apply(
            &mut loopback,
            &Widening {
                network: false,
                loopback_ports: vec![3000],
                reserved_ports: vec![],
                delete: false,
                write_paths: vec![job_tmp.clone()],
            },
            &host,
            Path::new("/bin/echo"),
            &[],
        )
        .map_err(|e| e.message().to_owned())?;
        assert_eq!(scratch_unlinks, Vec::<PathBuf>::new());
        assert_eq!(loopback.net, Net::Loopback);
        let profile =
            crate::seatbelt::profile(&loopback, None).map_err(|e| e.message().to_owned())?;
        assert!(profile.contains(r#"(allow network-outbound (remote ip "localhost:3000"))"#));
        for denied in ["localhost:*", "localhost:8780", "localhost:9200"] {
            assert!(
                !profile
                    .lines()
                    .any(|line| line.contains("network-outbound") && line.contains(denied))
            );
        }
        assert!(loopback.write.contains(&job_tmp));
        assert_eq!(ordinary.net, Net::None);
        assert!(!ordinary.write.contains(&write));
        Ok(())
    }
}
