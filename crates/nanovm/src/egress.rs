use std::{any::Any, collections::BTreeMap, fmt, path::PathBuf, sync::Arc};

use thiserror::Error;

use crate::{GuestCommand, Network, SharedDirectory, VmConfig};

const MOUNT_AND_EXEC: &str = concat!(
    "set -eu; ",
    "workdir=$1; shift; ",
    "while [ \"$1\" != -- ]; do ",
    "tag=$1; guest=$2; shift 2; ",
    "mkdir -p -- \"$guest\"; ",
    "mount -t virtiofs -o ro \"$tag\" \"$guest\"; ",
    "done; ",
    "shift; cd -- \"$workdir\"; exec \"$@\"",
);

/// VM-facing outbound-access configuration retained for one guest lifetime.
///
/// An application-specific provider can resolve MPP, secret, or capability
/// policy into this type without exposing that policy to `nanovm`. Values are
/// deliberately omitted from `Debug`: proxy URLs may contain short-lived
/// credentials.
#[derive(Clone)]
pub struct EgressLease {
    network: Network,
    guest_environment: BTreeMap<String, String>,
    guest_mounts: BTreeMap<String, EgressMount>,
    guards: Vec<Arc<dyn Any + Send + Sync>>,
}

/// One provider-owned host directory mounted read-only into the guest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EgressMount {
    pub tag: String,
    pub host_path: PathBuf,
    pub guest_path: PathBuf,
}

impl EgressLease {
    #[must_use]
    pub fn new(network: Network) -> Self {
        Self {
            network,
            guest_environment: BTreeMap::new(),
            guest_mounts: BTreeMap::new(),
            guards: Vec::new(),
        }
    }

    #[must_use]
    pub fn internet() -> Self {
        Self::new(Network::Internet)
    }

    #[must_use]
    pub fn disabled() -> Self {
        Self::new(Network::Disabled)
    }

    /// Adds one guest environment value.
    ///
    /// # Errors
    ///
    /// Returns an error when the name is empty or was already assigned a
    /// different value by another egress component.
    pub fn insert_environment(
        &mut self,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<(), EgressError> {
        let name = name.into();
        if !valid_environment_name(&name) {
            return Err(EgressError::InvalidEnvironmentName(name));
        }
        let value = value.into();
        if self
            .guest_environment
            .get(&name)
            .is_some_and(|current| current != &value)
        {
            return Err(EgressError::EnvironmentConflict(name));
        }
        self.guest_environment.insert(name, value);
        Ok(())
    }

    /// Adds one read-only provider mount.
    ///
    /// # Errors
    ///
    /// Returns an error when the tag or guest path collides with a different
    /// mount.
    pub fn insert_mount(&mut self, mount: EgressMount) -> Result<(), EgressError> {
        if mount.tag.is_empty() {
            return Err(EgressError::EmptyMountTag);
        }
        if self
            .guest_mounts
            .values()
            .any(|current| current.guest_path == mount.guest_path && current != &mount)
        {
            return Err(EgressError::GuestMountConflict(mount.guest_path));
        }
        if self
            .guest_mounts
            .get(&mount.tag)
            .is_some_and(|current| current != &mount)
        {
            return Err(EgressError::MountTagConflict(mount.tag));
        }
        self.guest_mounts.insert(mount.tag.clone(), mount);
        Ok(())
    }

    /// Retains provider state, such as a revocable proxy lease, until the guest
    /// is dropped.
    pub fn retain<T>(&mut self, guard: Arc<T>)
    where
        T: Any + Send + Sync,
    {
        self.guards.push(guard);
    }

    /// Combines independently provisioned egress fragments.
    ///
    /// Identical network, environment, and mount configuration is idempotent.
    /// Conflicting configuration fails closed.
    ///
    /// # Errors
    ///
    /// Returns an error when the fragments select different network modes or
    /// assign incompatible environment or mount values.
    pub fn merge(&mut self, other: Self) -> Result<(), EgressError> {
        if self.network != other.network {
            return Err(EgressError::NetworkConflict);
        }
        let mut merged = self.clone();
        for (name, value) in other.guest_environment {
            merged.insert_environment(name, value)?;
        }
        for mount in other.guest_mounts.into_values() {
            merged.insert_mount(mount)?;
        }
        merged.guards.extend(other.guards);
        *self = merged;
        Ok(())
    }

    /// Returns this lease with one independently provisioned layer applied.
    ///
    /// This is the fluent form of [`Self::merge`]. It lets an application
    /// assemble a VM route from concrete MPP, secret-gateway, and other
    /// provider leases without teaching the VM package about those policies.
    ///
    /// # Errors
    ///
    /// Returns an error when the new layer conflicts with an earlier layer.
    pub fn with_layer(mut self, layer: Self) -> Result<Self, EgressError> {
        self.merge(layer)?;
        Ok(self)
    }

    /// Applies this lease to one VM configuration and guest command.
    ///
    /// Provider directories are attached read-only and mounted before the
    /// requested program starts. Guest environment from the lease overrides
    /// the command's value for the same name. The lease itself must remain
    /// alive for at least as long as the resulting VM so its provider guards
    /// continue to protect revocable proxy and secret routes.
    #[must_use]
    pub fn configure(&self, mut vm: VmConfig, command: &GuestCommand) -> (VmConfig, GuestCommand) {
        vm = vm.network(self.network.clone());
        let mut configured =
            GuestCommand::new("/bin/sh").args(["-c", MOUNT_AND_EXEC, "nanovm-egress"]);
        configured = configured.arg(command.current_directory().as_os_str());
        for mount in self.guest_mounts() {
            vm = vm.shared_directory(SharedDirectory::read_only(&mount.tag, &mount.host_path));
            configured = configured.arg(&mount.tag).arg(mount.guest_path.as_os_str());
        }
        configured = configured.arg("--").arg(command.program().as_os_str());
        configured = configured.args(command.arguments().iter().cloned());
        for (name, value) in command.environment() {
            configured = configured.env(name, value);
        }
        for (name, value) in self.guest_environment() {
            configured = configured.env(name, value);
        }
        (vm, configured)
    }

    #[must_use]
    pub const fn network(&self) -> &Network {
        &self.network
    }

    #[must_use]
    pub fn guest_environment(&self) -> &BTreeMap<String, String> {
        &self.guest_environment
    }

    pub fn guest_mounts(&self) -> impl Iterator<Item = &EgressMount> {
        self.guest_mounts.values()
    }
}

impl fmt::Debug for EgressLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EgressLease")
            .field("network", &self.network)
            .field(
                "guest_environment_keys",
                &self.guest_environment.keys().collect::<Vec<_>>(),
            )
            .field(
                "guest_mounts",
                &self.guest_mounts.values().collect::<Vec<_>>(),
            )
            .field("guards", &self.guards.len())
            .finish()
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum EgressError {
    #[error("egress fragments require conflicting VM network modes")]
    NetworkConflict,
    #[error("guest environment name `{0}` is not a shell identifier")]
    InvalidEnvironmentName(String),
    #[error("guest environment `{0}` has conflicting egress values")]
    EnvironmentConflict(String),
    #[error("egress mount tag must not be empty")]
    EmptyMountTag,
    #[error("egress mount tag `{0}` has conflicting host paths")]
    MountTagConflict(String),
    #[error("guest egress mount path `{0}` has conflicting providers")]
    GuestMountConflict(PathBuf),
}

fn valid_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independently_provisioned_egress_fragments_compose() {
        let guard = Arc::new(());
        let mut secrets = EgressLease::internet();
        secrets
            .insert_environment("NANOCENTAUR_SECRET_BASE_URL", "https://secret-gateway/v1")
            .unwrap();
        secrets
            .insert_mount(EgressMount {
                tag: "secret-ca".to_owned(),
                host_path: PathBuf::from("/host/ca"),
                guest_path: PathBuf::from("/run/egress/ca"),
            })
            .unwrap();
        secrets.retain(Arc::clone(&guard));

        let mut mpp = EgressLease::internet();
        mpp.insert_environment(
            "HTTPS_PROXY",
            "http://mpp-lease:credential@host.internal:8080",
        )
        .unwrap();
        let egress = EgressLease::internet()
            .with_layer(mpp)
            .unwrap()
            .with_layer(secrets)
            .unwrap();

        assert_eq!(egress.guest_environment().len(), 2);
        assert_eq!(egress.guest_mounts().count(), 1);
        assert_eq!(Arc::strong_count(&guard), 2);
        let debug = format!("{egress:?}");
        assert!(!debug.contains("credential"));
        assert!(!debug.contains("secret-gateway"));
    }

    #[test]
    fn conflicting_provider_values_fail_closed() {
        let mut secrets = EgressLease::internet();
        secrets
            .insert_environment("HTTPS_PROXY", "http://secret-gateway")
            .unwrap();
        let mut mpp = EgressLease::internet();
        mpp.insert_environment("HTTPS_PROXY", "http://mpp-gateway")
            .unwrap();

        assert_eq!(
            secrets.merge(mpp),
            Err(EgressError::EnvironmentConflict("HTTPS_PROXY".to_owned()))
        );
    }

    #[test]
    fn lease_configures_network_mounts_and_proxy_environment() {
        let mut lease = EgressLease::internet();
        lease
            .insert_environment("HTTPS_PROXY", "http://host.internal:8080")
            .unwrap();
        lease
            .insert_mount(EgressMount {
                tag: "mpp-ca".to_owned(),
                host_path: PathBuf::from("/host/mpp"),
                guest_path: PathBuf::from("/run/egress/mpp"),
            })
            .unwrap();
        let command = GuestCommand::new("/usr/local/bin/nanocodex-vm-guest")
            .arg("/workspace")
            .env("HTTPS_PROXY", "http://untrusted")
            .current_dir("/workspace");

        let (vm, command) = lease.configure(
            VmConfig::ext4("/tmp/rootfs").network(Network::Disabled),
            &command,
        );

        assert_eq!(vm.network_value(), &Network::Internet);
        assert_eq!(
            vm.shared_directories(),
            &[SharedDirectory::read_only("mpp-ca", "/host/mpp")]
        );
        assert_eq!(command.program(), std::path::Path::new("/bin/sh"));
        assert_eq!(
            command
                .environment()
                .get(&std::ffi::OsString::from("HTTPS_PROXY")),
            Some(&std::ffi::OsString::from("http://host.internal:8080"))
        );
        assert!(
            command
                .arguments()
                .contains(&std::ffi::OsString::from("/run/egress/mpp"))
        );
    }
}
