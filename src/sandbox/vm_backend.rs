//! Container operations for one authenticated Pod VM.
use super::{SandboxBackend, SandboxKind, StartOutcome, WorkloadSpec};
use crate::core::{Config, Error, Result};
use crate::vm::{
    owner::PodVmStore,
    workload::{Operation, Reply},
};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::{io, sync::Arc, time::Duration};

pub(crate) struct VmBackend {
    config: Arc<Config>,
    store: PodVmStore,
    pod: String,
}

impl VmBackend {
    pub fn new(config: Arc<Config>, pod: &str) -> Self {
        let store = PodVmStore::new(config.runtime.state_dir.join("vms"));
        Self {
            config,
            store,
            pod: pod.to_string(),
        }
    }

    fn staging_dir(&self, id: &str) -> Result<std::path::PathBuf> {
        if !uuid::Uuid::parse_str(id).is_ok_and(|u| !u.is_nil() && u.to_string() == id) {
            return Err(Error::Container("invalid VM container ID".into()));
        }
        Ok(self
            .config
            .runtime
            .state_dir
            .join("vms")
            .join(format!(".oci-{id}")))
    }

    pub fn discard_stage(&self, id: &str) -> Result<()> {
        let path = self.staging_dir(id)?;
        match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
            Ok(meta) if meta.is_dir() => {
                crate::storage::snapshot::check_unmounted(&path.canonicalize()?)?;
                Ok(std::fs::remove_dir_all(path)?)
            }
            Ok(_) => Err(Error::Storage("VM staging directory was replaced".into())),
        }
    }

    pub async fn call(&self, id: &str, op: Operation<'_>) -> Result<Reply> {
        let channel = self.store.workload_channel(&self.pod).await?;
        Ok(channel.call(id, op).await?)
    }

    fn validate(&self, spec: &WorkloadSpec) -> Result<()> {
        let unsupported = |text: &str| Error::Experimental(format!("Pod VM: {text}"));
        if self.config.security.seccomp
            || !self.config.security.seccomp_profile_path.is_empty()
            || self.config.security.enable_apparmor
            || self.config.security.enable_selinux
        {
            return Err(unsupported(
                "node security profiles cannot be applied by this guest",
            ));
        }
        if let Some(s) = &spec.security {
            if s.privileged
                || s.host_network
                || s.host_pid
                || s.host_ipc
                || s.share_pod_pid
                || !s.add_capabilities.is_empty()
                || !s.apparmor_profile.is_empty()
                || !matches!(s.seccomp_profile_path.as_str(), "" | "unconfined")
            {
                return Err(unsupported("unsupported container namespace, privilege, path restriction or security profile"));
            }
        }
        if let Some(r) = &spec.resources {
            if !r.cpuset_cpus.is_empty() || !r.cpuset_mems.is_empty() || !(-1000..=1000).contains(&r.oom_score_adj) {
                return Err(unsupported(
                    "unsupported container cpuset or OOM adjustment",
                ));
            }
        }
        Ok(())
    }
    pub async fn prepare_workload(&self, spec: &WorkloadSpec) -> Result<String> {
        self.validate(spec)?;
        // Staging stays outside the guest export and on its filesystem. The owner
        // publishes the complete tree atomically without replacing another bundle.
        use std::os::unix::fs::DirBuilderExt;
        let staged = self.staging_dir(&spec.id)?;
        std::fs::DirBuilder::new().mode(0o700).create(&staged)?;
        let rootfs = staged.join("rootfs");
        tokio::fs::create_dir(&rootfs).await?;
        let mut image =
            crate::storage::pull::materialize_rootfs(&self.config, &spec.image_ref, &rootfs)
                .await?;
        let root = pathrs::Root::open(&rootfs)?;
        let security = spec.security.as_ref();
        let (uid, gid) = crate::storage::user::resolve(&root, &image.user, security)?;
        let groups = crate::storage::user::groups(
            &root,
            uid,
            security.map_or(&[][..], |s| s.supplemental_groups.as_slice()),
            spec.strict_groups,
        )?;
        let argv = if spec.entrypoint.is_empty() {
            image.entrypoint.extend(
                if spec.args.is_empty() {
                    &image.cmd
                } else {
                    &spec.args
                }
                .iter()
                .cloned(),
            );
            image.entrypoint
        } else {
            [spec.entrypoint.as_slice(), spec.args.as_slice()].concat()
        };
        image.env.retain(|v| {
            v.split_once('=')
                .is_none_or(|(key, _)| !spec.env.contains_key(key))
        });
        image
            .env
            .extend(spec.env.iter().map(|(k, v)| format!("{k}={v}")));
        let cwd = if !spec.working_dir.is_empty() {
            spec.working_dir.as_str()
        } else if image.working_dir.is_empty() {
            "/"
        } else {
            &image.working_dir
        };
        let mut resources = json!({});
        if let Some(r) = &spec.resources {
            for (name, value) in [
                ("shares", r.cpu_shares),
                ("quota", r.cpu_quota),
                ("period", r.cpu_period),
            ] {
                if value != 0 {
                    resources["cpu"][name] = Value::from(value);
                }
            }
            if r.memory_limit_in_bytes != 0 {
                resources["memory"] = json!({"limit":r.memory_limit_in_bytes});
            }
        }
        let mut mounts = Vec::with_capacity(spec.volumes.len());
        for volume in &spec.volumes {
            if !matches!(volume.propagation.as_str(), "" | "rprivate") {
                return Err(Error::Experimental(
                    "VM host volumes require private propagation".into(),
                ));
            }
            let source = self
                .store
                .bind_volume(&self.pod, &spec.id, &volume.host_path, volume.readonly)
                .await?;
            mounts.push(
                json!({"type":"bind", "source":source, "destination":volume.container_path,
                "options":["bind", if volume.readonly {"ro"} else {"rw"}]}),
            );
        }
        let mut spec_json = json!({
            "ociVersion":"1.1.0", "mounts":mounts,
            "root":{"path":"rootfs", "readonly": self.config.security.read_only_root_filesystem || security.is_some_and(|s| s.readonly_rootfs)},
            "annotations":{"io.nautilus.stdin":spec.open_stdin.to_string(),"io.nautilus.stdinOnce":spec.stdin_once.to_string()},
            "process":{"terminal":spec.tty,"user":{"uid":uid,"gid":gid,"additionalGids":groups},
              "args":argv,"env":image.env,"cwd":cwd,"noNewPrivileges":true,"capabilities":{}},
            "linux":{"namespaces":[{"type":"mount"},{"type":"pid"}],"resources":resources}
        });
        if let Some(security) = security {
            if !security.masked_paths.is_empty() {
                spec_json["linux"]["maskedPaths"] = json!(security.masked_paths);
            }
            if !security.readonly_paths.is_empty() {
                spec_json["linux"]["readonlyPaths"] = json!(security.readonly_paths);
            }
        }
        if let Some(resources) = &spec.resources {
            if resources.oom_score_adj != 0 {
                spec_json["process"]["oomScoreAdj"] = json!(resources.oom_score_adj);
            }
        }
        tokio::fs::write(staged.join("config.json"), serde_json::to_vec(&spec_json)?).await?;
        self.store
            .publish_bundle(&self.pod, &spec.id, &staged)
            .await?;
        self.call(&spec.id, Operation::Create).await?;
        Ok(image.image_id)
    }
}

#[async_trait]
impl SandboxBackend for VmBackend {
    fn kind(&self) -> SandboxKind {
        SandboxKind::Vm
    }

    async fn prepare(&self, spec: &WorkloadSpec) -> Result<()> {
        self.prepare_workload(spec).await.map(drop)
    }

    async fn start(&self, spec: &WorkloadSpec) -> Result<StartOutcome> {
        self.call(&spec.id, Operation::Start).await?;
        Ok(StartOutcome::Guest)
    }

    async fn stop(&self, id: &str, grace: Duration) -> Result<Option<i32>> {
        let reply = self
            .call(
                id,
                Operation::Stop {
                    timeout_ms: milliseconds(grace)?,
                },
            )
            .await?;
        Ok(reply.exit_code)
    }

    async fn exec_sync(
        &self,
        id: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: Option<&str>,
        timeout: Option<Duration>,
    ) -> Result<(i32, Vec<u8>, Vec<u8>)> {
        if !env.is_empty() || cwd.is_some() {
            return Err(Error::Experimental(
                "VM exec environment and cwd overrides are unavailable".into(),
            ));
        }
        let reply = self
            .call(
                id,
                Operation::Exec {
                    argv: args,
                    timeout_ms: milliseconds(timeout.unwrap_or_default())?,
                },
            )
            .await?;
        Ok((
            reply
                .exit_code
                .ok_or_else(|| Error::Runtime("guest omitted exec exit code".into()))?,
            reply.stdout()?,
            reply.stderr()?,
        ))
    }

    async fn remove(&self, id: &str) -> Result<()> {
        match self.call(id, Operation::Remove).await {
            Err(Error::Io(e))
                if e.kind() == io::ErrorKind::NotFound
                    && self.store.status(&self.pod).await?.is_none() => {}
            other => {
                other?;
            }
        }
        self.store.remove_bundle(&self.pod, id).await?;
        self.discard_stage(id)?;
        Ok(())
    }
}

fn milliseconds(duration: Duration) -> Result<i64> {
    i64::try_from(duration.as_millis())
        .ok()
        .filter(|n| *n <= 300_000)
        .ok_or_else(|| Error::Experimental("guest timeout exceeds 300 seconds".into()))
}
