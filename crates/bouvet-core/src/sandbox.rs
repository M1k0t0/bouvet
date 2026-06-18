//! Sandbox type - a running microVM with agent connection.

use crate::client::{AgentClient, ExecResult, FileEntry};
use crate::config::{InternetAccessConfig, SandboxConfig};
use crate::error::CoreError;
use chrono::{DateTime, Utc};
use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use tokio::sync::Mutex;
use uuid::Uuid;

/// Unique identifier for a sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SandboxId(Uuid);

impl SandboxId {
    /// Create a new random sandbox ID.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Get the underlying UUID.
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl Default for SandboxId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SandboxId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<Uuid> for SandboxId {
    fn from(uuid: Uuid) -> Self {
        Self(uuid)
    }
}

fn blocked_cidrs_for_internet(internet: &InternetAccessConfig) -> Vec<String> {
    let sandbox_cidr = format!(
        "{}.{}.0.0/16",
        internet.ipv4_prefix[0], internet.ipv4_prefix[1]
    );
    let mut cidrs = vec![
        sandbox_cidr,
        "0.0.0.0/8".into(),
        "10.0.0.0/8".into(),
        "100.64.0.0/10".into(),
        "127.0.0.0/8".into(),
        "169.254.0.0/16".into(),
        "172.16.0.0/12".into(),
        "192.168.0.0/16".into(),
        "198.18.0.0/15".into(),
        "224.0.0.0/4".into(),
        "240.0.0.0/4".into(),
    ];
    cidrs.extend(internet.blocked_cidrs.iter().cloned());
    cidrs.sort();
    cidrs.dedup();
    cidrs
}

const INTERNET_NETWORK_SLOT_COUNT: u16 = 16_384;

fn internet_network_allocations() -> &'static StdMutex<HashMap<[u8; 2], BTreeSet<u16>>> {
    static ALLOCATIONS: OnceLock<StdMutex<HashMap<[u8; 2], BTreeSet<u16>>>> = OnceLock::new();
    ALLOCATIONS.get_or_init(|| StdMutex::new(HashMap::new()))
}

struct InternetNetworkLease {
    prefix: [u8; 2],
    slot: u16,
    release_on_drop: bool,
}

impl InternetNetworkLease {
    fn allocate(internet: &InternetAccessConfig) -> Result<Self, CoreError> {
        let mut allocations = internet_network_allocations()
            .lock()
            .map_err(|_| CoreError::Connection("internet network allocator poisoned".into()))?;
        let allocated_slots = allocations.entry(internet.ipv4_prefix).or_default();
        let slot = (0..INTERNET_NETWORK_SLOT_COUNT)
            .find(|slot| !allocated_slots.contains(slot))
            .ok_or_else(|| {
                CoreError::Connection(format!(
                    "internet network pool exhausted for prefix {}.{}",
                    internet.ipv4_prefix[0], internet.ipv4_prefix[1]
                ))
            })?;
        allocated_slots.insert(slot);

        Ok(Self {
            prefix: internet.ipv4_prefix,
            slot,
            release_on_drop: true,
        })
    }

    fn slot(&self) -> u16 {
        self.slot
    }
}

impl Drop for InternetNetworkLease {
    fn drop(&mut self) {
        if !self.release_on_drop {
            return;
        }

        let Ok(mut allocations) = internet_network_allocations().lock() else {
            tracing::warn!("Failed to release internet network slot: allocator poisoned");
            return;
        };

        if let Some(allocated_slots) = allocations.get_mut(&self.prefix) {
            allocated_slots.remove(&self.slot);
            if allocated_slots.is_empty() {
                allocations.remove(&self.prefix);
            }
        }
    }
}

fn network_config_for_sandbox(
    id: SandboxId,
    network_lease: &InternetNetworkLease,
    internet: &InternetAccessConfig,
) -> Result<bouvet_vm::NetworkConfig, CoreError> {
    let slot = network_lease.slot();
    let third_octet = (slot / 64) as u8;
    let subnet_base = ((slot % 64) * 4) as u8;
    let host_ip = Ipv4Addr::new(
        internet.ipv4_prefix[0],
        internet.ipv4_prefix[1],
        third_octet,
        subnet_base + 1,
    );
    let guest_ip = Ipv4Addr::new(
        internet.ipv4_prefix[0],
        internet.ipv4_prefix[1],
        third_octet,
        subnet_base + 2,
    );
    let uuid = id.to_string();

    Ok(bouvet_vm::NetworkConfig {
        iface_id: "eth0".into(),
        host_dev_name: format!("bvt{}", &uuid[..8]),
        guest_mac: None,
        host_network: Some(bouvet_vm::HostNetworkConfig {
            host_ip,
            guest_ip,
            prefix_len: 30,
            guest_netmask: Ipv4Addr::new(255, 255, 255, 252),
            outbound_iface: internet.outbound_iface.clone(),
            blocked_cidrs: blocked_cidrs_for_internet(internet),
            enable_ip_forward: true,
            enable_masquerade: true,
        }),
    })
}

fn boot_args_with_network(network: &bouvet_vm::NetworkConfig) -> Result<String, CoreError> {
    let host_network = network.host_network.as_ref().ok_or_else(|| {
        CoreError::Connection("internet access requires host network config".into())
    })?;

    Ok(format!(
        "{} ip={}::{}:{}:bouvet:eth0:off",
        bouvet_vm::MachineConfig::default().boot_args,
        host_network.guest_ip,
        host_network.host_ip,
        host_network.guest_netmask
    ))
}

async fn configure_guest_dns(
    client: &mut AgentClient,
    internet: &InternetAccessConfig,
) -> Result<(), CoreError> {
    let mut resolv_conf = String::new();
    for dns_server in &internet.dns_servers {
        resolv_conf.push_str(&format!("nameserver {dns_server}\n"));
    }

    client.write_file("/etc/resolv.conf", &resolv_conf).await
}

/// Current state of a sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxState {
    /// Sandbox is being created (VM booting, agent connecting).
    Creating,
    /// Sandbox is ready for commands.
    Ready,
    /// Sandbox is destroyed.
    Destroyed,
}

impl fmt::Display for SandboxState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Creating => write!(f, "Creating"),
            Self::Ready => write!(f, "Ready"),
            Self::Destroyed => write!(f, "Destroyed"),
        }
    }
}

/// A running sandbox with VM and agent connection.
///
/// A sandbox represents a complete isolated execution environment consisting of:
/// - A Firecracker microVM
/// - A connected guest agent
///
/// Use the methods on this type to execute commands and work with files
/// in the isolated environment.
pub struct Sandbox {
    id: SandboxId,
    vm: bouvet_vm::VirtualMachine,
    client: Arc<Mutex<AgentClient>>,
    config: SandboxConfig,
    state: SandboxState,
    created_at: DateTime<Utc>,
    // Held for Drop so the per-sandbox network slot is released with the sandbox.
    #[allow(dead_code)]
    internet_network: Option<InternetNetworkLease>,
}

impl Sandbox {
    /// Create a new sandbox (called by SandboxManager).
    ///
    /// This will:
    /// 1. Create and boot a microVM
    /// 2. Wait for the guest agent to start
    /// 3. Connect to the agent via vsock
    /// 4. Verify the agent is responsive
    pub(crate) async fn create(config: SandboxConfig) -> Result<Self, CoreError> {
        let id = SandboxId::new();
        let start = std::time::Instant::now();
        tracing::info!(
            sandbox_id = %id,
            vcpus = config.vcpu_count,
            memory_mib = config.memory_mib,
            vsock_cid = config.vsock_cid,
            "Creating sandbox"
        );

        // Generate unique vsock config with per-VM UDS path
        let vsock_config =
            bouvet_vm::VsockConfig::for_vm(config.vsock_cid, &config.chroot_path, &id.to_string());
        tracing::debug!(
            sandbox_id = %id,
            uds_path = %vsock_config.uds_path.display(),
            "Generated vsock config"
        );

        // Ensure vsock directory exists
        if let Some(parent) = vsock_config.uds_path.parent() {
            tracing::trace!(sandbox_id = %id, path = %parent.display(), "Creating vsock directory");
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                tracing::error!(sandbox_id = %id, error = %e, "Failed to create vsock directory");
                CoreError::Connection(format!("Failed to create vsock directory: {}", e))
            })?;
        }

        // 1. Build VM config with unique vsock path
        tracing::debug!(sandbox_id = %id, "Building VM configuration");
        let mut vm_builder = bouvet_vm::VmBuilder::new()
            .vcpus(config.vcpu_count)
            .memory_mib(config.memory_mib)
            .kernel(&config.kernel_path)
            .rootfs(&config.rootfs_path)
            .firecracker_path(&config.firecracker_path)
            .chroot_path(&config.chroot_path)
            .with_vsock_config(vsock_config);

        let mut internet_network = None;
        if let Some(internet) = &config.internet_access {
            let network_lease = InternetNetworkLease::allocate(internet)?;
            let network = network_config_for_sandbox(id, &network_lease, internet)?;
            let boot_args = boot_args_with_network(&network)?;
            let guest_ip = network
                .host_network
                .as_ref()
                .expect("host network")
                .guest_ip;
            tracing::debug!(
                sandbox_id = %id,
                network_slot = network_lease.slot(),
                tap = %network.host_dev_name,
                guest_ip = %guest_ip,
                "Enabling sandbox internet access"
            );
            vm_builder = vm_builder.boot_args(boot_args).with_network_config(network);
            internet_network = Some(network_lease);
        }

        let vm_config = vm_builder.build_config();

        // 2. Create and boot VM with the same ID as the sandbox
        tracing::debug!(sandbox_id = %id, "Creating and booting VM");
        let vm = match bouvet_vm::VirtualMachine::create_with_id(id.as_uuid(), vm_config).await {
            Ok(vm) => vm,
            Err(e) => {
                tracing::error!(sandbox_id = %id, error = %e, "VM creation failed");
                // Cleanup directory if VM creation fails
                let vsock_dir = config.chroot_path.join(id.to_string());
                let _ = tokio::fs::remove_dir_all(&vsock_dir).await;
                return Err(e.into());
            }
        };
        tracing::debug!(
            sandbox_id = %id,
            elapsed_ms = start.elapsed().as_millis() as u64,
            "VM created and started"
        );

        // 3. Get vsock path and connect to agent
        let vsock_path = match vm.vsock_uds_path() {
            Some(path) => path.clone(),
            None => {
                tracing::error!(sandbox_id = %id, "VM started without vsock path");
                let _ = vm.destroy().await;
                return Err(CoreError::Connection("vsock not configured".into()));
            }
        };

        tracing::debug!(sandbox_id = %id, path = %vsock_path.display(), "Connecting to agent");
        let mut client = match AgentClient::connect(&vsock_path).await {
            Ok(client) => client,
            Err(e) => {
                tracing::error!(sandbox_id = %id, error = %e, "Agent connection failed");
                let _ = vm.destroy().await;
                return Err(e);
            }
        };
        tracing::debug!(sandbox_id = %id, "Agent connected");

        // 4. Verify agent is responsive
        tracing::trace!(sandbox_id = %id, "Pinging agent");
        if let Err(e) = client.ping().await {
            tracing::error!(sandbox_id = %id, error = %e, "Agent ping failed");
            let _ = vm.destroy().await;
            return Err(e);
        }

        if let Some(internet) = &config.internet_access {
            tracing::debug!(sandbox_id = %id, "Configuring guest DNS");
            if let Err(e) = configure_guest_dns(&mut client, internet).await {
                tracing::error!(sandbox_id = %id, error = %e, "Guest DNS configuration failed");
                let _ = vm.destroy().await;
                return Err(e);
            }
        }

        tracing::info!(
            sandbox_id = %id,
            elapsed_ms = start.elapsed().as_millis() as u64,
            "Sandbox ready"
        );

        Ok(Self {
            id,
            vm,
            client: Arc::new(Mutex::new(client)),
            config,
            state: SandboxState::Ready,
            created_at: Utc::now(),
            internet_network,
        })
    }

    /// Get the sandbox ID.
    pub fn id(&self) -> SandboxId {
        self.id
    }

    /// Get the current state.
    pub fn state(&self) -> SandboxState {
        self.state
    }

    /// Get the creation timestamp.
    pub fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }

    /// Get the configuration used to create this sandbox.
    pub fn config(&self) -> &SandboxConfig {
        &self.config
    }

    /// Execute a shell command.
    ///
    /// # Arguments
    ///
    /// * `cmd` - Shell command to execute
    ///
    /// # Returns
    ///
    /// The execution result including exit code, stdout, and stderr.
    pub async fn execute(&self, cmd: &str) -> Result<ExecResult, CoreError> {
        tracing::debug!(sandbox_id = %self.id, cmd = %cmd, "Executing command");
        self.ensure_ready()?;
        let mut client = self.client.lock().await;
        let result = client.exec(cmd).await;
        if let Ok(ref r) = result {
            tracing::debug!(
                sandbox_id = %self.id,
                exit_code = r.exit_code,
                stdout_len = r.stdout.len(),
                stderr_len = r.stderr.len(),
                "Command completed"
            );
        }
        result
    }

    /// Execute code in a specific language.
    ///
    /// # Arguments
    ///
    /// * `lang` - Language identifier (python, python3, node, javascript, bash, sh)
    /// * `code` - Code to execute
    ///
    /// # Returns
    ///
    /// The execution result including exit code, stdout, and stderr.
    pub async fn execute_code(&self, lang: &str, code: &str) -> Result<ExecResult, CoreError> {
        tracing::debug!(sandbox_id = %self.id, lang = %lang, code_len = code.len(), "Executing code");
        self.ensure_ready()?;
        let mut client = self.client.lock().await;
        let result = client.exec_code(lang, code).await;
        if let Ok(ref r) = result {
            tracing::debug!(
                sandbox_id = %self.id,
                exit_code = r.exit_code,
                stdout_len = r.stdout.len(),
                stderr_len = r.stderr.len(),
                "Code execution completed"
            );
        }
        result
    }

    /// Read a file from the guest filesystem.
    ///
    /// # Arguments
    ///
    /// * `path` - Absolute path to the file
    ///
    /// # Returns
    ///
    /// The file contents as a string.
    pub async fn read_file(&self, path: &str) -> Result<String, CoreError> {
        tracing::debug!(sandbox_id = %self.id, path = %path, "Reading file");
        self.ensure_ready()?;
        let mut client = self.client.lock().await;
        let result = client.read_file(path).await;
        if let Ok(ref content) = result {
            tracing::trace!(sandbox_id = %self.id, size = content.len(), "File read");
        }
        result
    }

    /// Write a file to the guest filesystem.
    ///
    /// # Arguments
    ///
    /// * `path` - Absolute path to the file
    /// * `content` - Content to write
    pub async fn write_file(&self, path: &str, content: &str) -> Result<(), CoreError> {
        tracing::debug!(sandbox_id = %self.id, path = %path, content_len = content.len(), "Writing file");
        self.ensure_ready()?;
        let mut client = self.client.lock().await;
        client.write_file(path, content).await
    }

    /// List directory contents.
    ///
    /// # Arguments
    ///
    /// * `path` - Absolute path to the directory
    ///
    /// # Returns
    ///
    /// A list of file entries in the directory.
    pub async fn list_dir(&self, path: &str) -> Result<Vec<FileEntry>, CoreError> {
        tracing::debug!(sandbox_id = %self.id, path = %path, "Listing directory");
        self.ensure_ready()?;
        let mut client = self.client.lock().await;
        let result = client.list_dir(path).await;
        if let Ok(ref entries) = result {
            tracing::trace!(sandbox_id = %self.id, count = entries.len(), "Directory listed");
        }
        result
    }

    /// Check if the sandbox is healthy and responsive.
    ///
    /// This pings the agent to verify it's still running and responsive.
    /// Returns true if the agent responds, false otherwise.
    pub async fn is_healthy(&self) -> bool {
        if self.state != SandboxState::Ready {
            tracing::trace!(sandbox_id = %self.id, state = ?self.state, "Health check: not ready");
            return false;
        }
        let mut client = match self.client.try_lock() {
            Ok(c) => c,
            Err(_) => {
                tracing::trace!(sandbox_id = %self.id, "Health check: client busy, assuming healthy");
                return true; // Client busy = still working
            }
        };
        let healthy = client.ping().await.is_ok();
        tracing::trace!(sandbox_id = %self.id, healthy, "Health check completed");
        healthy
    }

    /// Destroy the sandbox.
    ///
    /// This stops the VM and releases all resources.
    pub async fn destroy(mut self) -> Result<(), CoreError> {
        let start = std::time::Instant::now();
        tracing::info!(sandbox_id = %self.id, "Destroying sandbox");
        self.state = SandboxState::Destroyed;

        tracing::debug!(sandbox_id = %self.id, "Stopping VM");
        self.vm.destroy().await?;

        // Clean up vsock directory
        let vsock_dir = self.config.chroot_path.join(self.id.to_string());
        tracing::debug!(sandbox_id = %self.id, path = %vsock_dir.display(), "Removing sandbox directory");
        if let Err(e) = tokio::fs::remove_dir_all(&vsock_dir).await {
            tracing::warn!(sandbox_id = %self.id, error = %e, "Failed to remove sandbox directory");
        }

        tracing::info!(
            sandbox_id = %self.id,
            elapsed_ms = start.elapsed().as_millis() as u64,
            "Sandbox destroyed"
        );
        Ok(())
    }

    /// Ensure the sandbox is in the Ready state.
    fn ensure_ready(&self) -> Result<(), CoreError> {
        if self.state != SandboxState::Ready {
            return Err(CoreError::InvalidState {
                expected: "Ready".into(),
                actual: format!("{:?}", self.state),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sandbox_id_display() {
        let id = SandboxId::new();
        let s = format!("{}", id);
        // UUID format: xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
        assert_eq!(s.len(), 36);
        assert!(s.contains('-'));
    }

    #[test]
    fn test_sandbox_state_display() {
        assert_eq!(format!("{}", SandboxState::Creating), "Creating");
        assert_eq!(format!("{}", SandboxState::Ready), "Ready");
        assert_eq!(format!("{}", SandboxState::Destroyed), "Destroyed");
    }

    #[test]
    fn test_sandbox_id_from_uuid() {
        let uuid = Uuid::new_v4();
        let id: SandboxId = uuid.into();
        assert_eq!(format!("{}", id), format!("{}", uuid));
    }

    #[test]
    fn test_network_config_for_sandbox_first_slot() {
        let uuid = Uuid::parse_str("12345678-1234-5678-1234-567812345678").unwrap();
        let id = SandboxId::from(uuid);
        let network_lease = InternetNetworkLease {
            prefix: [172, 30],
            slot: 0,
            release_on_drop: false,
        };
        let network =
            network_config_for_sandbox(id, &network_lease, &InternetAccessConfig::default())
                .unwrap();
        let host_network = network.host_network.expect("host network");

        assert_eq!(network.host_dev_name, "bvt12345678");
        assert_eq!(host_network.host_ip, Ipv4Addr::new(172, 30, 0, 1));
        assert_eq!(host_network.guest_ip, Ipv4Addr::new(172, 30, 0, 2));
        assert_eq!(host_network.prefix_len, 30);
        assert!(host_network
            .blocked_cidrs
            .iter()
            .any(|cidr| cidr == "172.30.0.0/16"));
        assert!(host_network
            .blocked_cidrs
            .iter()
            .any(|cidr| cidr == "10.0.0.0/8"));
        assert!(host_network
            .blocked_cidrs
            .iter()
            .any(|cidr| cidr == "169.254.0.0/16"));
    }

    #[test]
    fn test_network_config_for_sandbox_high_slot() {
        let id = SandboxId::new();
        let internet = InternetAccessConfig {
            ipv4_prefix: [10, 77],
            ..Default::default()
        };
        let network_lease = InternetNetworkLease {
            prefix: [10, 77],
            slot: 64,
            release_on_drop: false,
        };
        let network = network_config_for_sandbox(id, &network_lease, &internet).unwrap();
        let host_network = network.host_network.expect("host network");

        assert_eq!(host_network.host_ip, Ipv4Addr::new(10, 77, 1, 1));
        assert_eq!(host_network.guest_ip, Ipv4Addr::new(10, 77, 1, 2));
        assert!(host_network
            .blocked_cidrs
            .iter()
            .any(|cidr| cidr == "10.77.0.0/16"));
    }

    #[test]
    fn test_blocked_cidrs_are_deduplicated() {
        let internet = InternetAccessConfig {
            ipv4_prefix: [10, 0],
            ..Default::default()
        };
        let blocked = blocked_cidrs_for_internet(&internet);
        let ten_count = blocked
            .iter()
            .filter(|cidr| cidr.as_str() == "10.0.0.0/8")
            .count();

        assert_eq!(ten_count, 1);
        assert!(blocked.iter().any(|cidr| cidr == "10.0.0.0/16"));
    }

    #[test]
    fn test_internet_network_slots_are_reused() {
        let internet = InternetAccessConfig {
            ipv4_prefix: [198, 77],
            ..Default::default()
        };

        let first = InternetNetworkLease::allocate(&internet).unwrap();
        let second = InternetNetworkLease::allocate(&internet).unwrap();
        assert_eq!(first.slot(), 0);
        assert_eq!(second.slot(), 1);

        drop(first);

        let reused = InternetNetworkLease::allocate(&internet).unwrap();
        assert_eq!(reused.slot(), 0);
    }

    #[test]
    fn test_boot_args_with_network() {
        let id = SandboxId::new();
        let network_lease = InternetNetworkLease {
            prefix: [172, 30],
            slot: 0,
            release_on_drop: false,
        };
        let network =
            network_config_for_sandbox(id, &network_lease, &InternetAccessConfig::default())
                .unwrap();
        let boot_args = boot_args_with_network(&network).unwrap();

        assert!(boot_args.contains("console=ttyS0"));
        assert!(boot_args.contains("ip=172.30.0.2::172.30.0.1:255.255.255.252:bouvet:eth0:off"));
    }
}
