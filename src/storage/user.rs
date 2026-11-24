//! Resolve OCI USER and CRI overrides against the image's account databases.
use crate::core::{Error, Result};
use crate::sandbox::ContainerSecurity;
use pathrs::{error::ErrorKind, flags::OpenFlags, Root};
use std::io::{BufRead, BufReader};

pub(crate) fn resolve(
    root: &Root,
    image_user: &str,
    security: Option<&ContainerSecurity>,
) -> Result<(u32, u32)> {
    let named_override = security.is_some_and(|s| !s.run_as_name.is_empty());
    let (user, group) = match security {
        Some(sec) if sec.run_as_uid.is_some() => (sec.run_as_uid.unwrap().to_string(), None),
        Some(sec) if !sec.run_as_name.is_empty() => (sec.run_as_name.clone(), None),
        _ => {
            let (user, group) = image_user.split_once(':').unwrap_or((image_user, ""));
            (user.to_owned(), (!group.is_empty()).then_some(group))
        }
    };
    // Empty USER means the OCI default. Numeric UID:GID also works in scratch
    // images that carry no passwd or group files.
    let gid_override = security.and_then(|s| s.run_as_gid);
    let uid = if named_override {
        None // CRI requires even numeric usernames to exist by name in passwd.
    } else if user.is_empty() {
        Some(0)
    } else {
        user.parse::<u32>().ok()
    };
    let needs_passwd =
        uid.is_none() || (!user.is_empty() && group.is_none() && gid_override.is_none());
    let account = if needs_passwd {
        lookup(root, "etc/passwd", |fields| {
            let id: u32 = fields.get(2)?.parse().ok()?;
            if uid == Some(id) || (uid.is_none() && fields.first()? == &user.as_str()) {
                Some((id, fields.get(3)?.parse::<u32>().ok()?))
            } else {
                None
            }
        })?
    } else {
        None
    };
    let uid = uid
        .or_else(|| account.map(|(id, _)| id))
        .ok_or_else(|| Error::Image(format!("unknown image user {user:?}")))?;
    let gid = match (gid_override, group) {
        (Some(id), _) => id,
        (None, None) => account.map_or(0, |(_, gid)| gid),
        (None, Some(group)) => match group.parse::<u32>() {
            Ok(id) => id,
            Err(_) => lookup(root, "etc/group", |fields| {
                if fields.first()? == &group {
                    fields.get(2)?.parse::<u32>().ok()
                } else {
                    None
                }
            })?
            .ok_or_else(|| Error::Image(format!("unknown image group {group:?}")))?,
        },
    };
    Ok((uid, gid))
}

/// Add the primary user's memberships from the image when CRI requests Merge.
pub(crate) fn groups(root: &Root, uid: u32, requested: &[u32], strict: bool) -> Result<Vec<u32>> {
    let mut groups = requested.to_vec();
    if !strict {
        if let Some(name) = lookup(root, "etc/passwd", |f| {
            (f.get(2)?.parse::<u32>().ok()? == uid).then(|| f[0].to_owned())
        })? {
            lookup::<()>(root, "etc/group", |f| {
                if f.get(3)?.split(',').any(|user| user == name) {
                    groups.push(f.get(2)?.parse::<u32>().ok()?);
                }
                None
            })?;
        }
    }
    groups.sort_unstable();
    groups.dedup();
    Ok(groups)
}

fn lookup<T>(
    root: &Root,
    path: &str,
    mut parse: impl FnMut(&[&str]) -> Option<T>,
) -> Result<Option<T>> {
    let file = match root.open_subpath(path, OpenFlags::O_RDONLY | OpenFlags::O_NONBLOCK) {
        Err(err) if err.kind() == ErrorKind::OsError(Some(libc::ENOENT)) => return Ok(None),
        result => result?,
    };
    if !file.metadata()?.is_file() {
        return Err(Error::Image(format!(
            "expected a regular account file at {path}"
        )));
    }
    for line in BufReader::new(file).lines() {
        let line = line?;
        if let Some(record) = parse(&line.split(':').collect::<Vec<_>>()) {
            return Ok(Some(record));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_identity_and_cri_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        assert_eq!(resolve(&root, "", None).unwrap(), (0, 0));
        assert_eq!(resolve(&root, "10001:10002", None).unwrap(), (10001, 10002));
        std::fs::create_dir(dir.path().join("etc")).unwrap();
        std::fs::write(
            dir.path().join("etc/passwd"),
            "worker:x:10001:101::/:/bin/sh\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("etc/group"), "jobs:x:202:\n").unwrap();
        assert_eq!(resolve(&root, "worker", None).unwrap(), (10001, 101));
        assert_eq!(resolve(&root, "10001", None).unwrap(), (10001, 101));
        assert_eq!(resolve(&root, "worker:jobs", None).unwrap(), (10001, 202));
        assert!(resolve(&root, "missing", None).is_err());
        assert!(resolve(&root, "worker:missing", None).is_err());

        let by_name = ContainerSecurity {
            run_as_name: "worker".into(),
            ..Default::default()
        };
        assert_eq!(
            resolve(&root, "missing", Some(&by_name)).unwrap(),
            (10001, 101)
        );
        let numeric_name = ContainerSecurity {
            run_as_name: "1234".into(),
            ..Default::default()
        };
        assert!(resolve(&root, "", Some(&numeric_name)).is_err());
        std::fs::write(
            dir.path().join("etc/passwd"),
            "1234:x:2000:3000::/:/bin/sh\n",
        )
        .unwrap();
        assert_eq!(
            resolve(&root, "", Some(&numeric_name)).unwrap(),
            (2000, 3000)
        );
        let by_id = ContainerSecurity {
            run_as_uid: Some(1234),
            run_as_gid: Some(5678),
            ..Default::default()
        };
        assert_eq!(
            resolve(&root, "worker:jobs", Some(&by_id)).unwrap(),
            (1234, 5678)
        );
    }
}
