//! Configuration types for MicroVM instances.

use crate::error::{Result, VmError};
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

/// Configuration for creating a new MicroVM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineConfig {
    /// Number of virtual CPUs (1-32)
    pub vcpu_count: u8,
    /// Memory size in MiB (128-32768)
    pub memory_mib: u32,
    /// Path to kernel image
    pub kernel_path: PathBuf,
    /// Kernel boot arguments
    pub boot_args: String,
    /// Root filesystem drive
    pub root_drive: DriveConfig,
    /// Additional drives (optional)
    pub extra_drives: Vec<DriveConfig>,
    /// Network configuration (optional)
    pub network: Option<NetworkConfig>,
    /// vsock configuration for guest-host communication (optional)
    pub vsock: Option<VsockConfig>,
    /// Path to Firecracker binary
    pub firecracker_path: PathBuf,
    /// Working directory for VM sockets and state
    pub chroot_path: PathBuf,
}

impl Default for MachineConfig {
    fn default() -> Self {
        Self {
            vcpu_count: 2,
            memory_mib: 256,
            kernel_path: PathBuf::from("/var/lib/bouvet/kernel/vmlinux"),
            boot_args: "console=ttyS0 reboot=k panic=1 pci=off".into(),
            root_drive: DriveConfig::default(),
            extra_drives: Vec::new(),
            network: None,
            vsock: None,
            firecracker_path: PathBuf::from("/usr/local/bin/firecracker"),
            chroot_path: PathBuf::from("/tmp/bouvet"),
        }
    }
}

impl MachineConfig {
    /// Validate the configuration.
    ///
    /// # Errors
    /// Returns an error if any configuration value is invalid.
    pub fn validate(&self) -> Result<()> {
        // Validate vCPU count (Firecracker supports 1-32)
        if self.vcpu_count == 0 || self.vcpu_count > 32 {
            return Err(VmError::Config(format!(
                "vcpu_count must be 1-32, got {}",
                self.vcpu_count
            )));
        }

        // Validate memory (Firecracker: 128 MiB to 32 GiB)
        if self.memory_mib < 128 {
            return Err(VmError::Config(format!(
                "memory_mib must be at least 128, got {}",
                self.memory_mib
            )));
        }
        if self.memory_mib > 32768 {
            return Err(VmError::Config(format!(
                "memory_mib must be at most 32768 (32 GiB), got {}",
                self.memory_mib
            )));
        }

        // Validate vsock CID (must be > 2, as 0, 1, 2 are reserved)
        if let Some(vsock) = &self.vsock {
            if vsock.guest_cid <= 2 {
                return Err(VmError::Config(format!(
                    "vsock guest_cid must be > 2, got {}",
                    vsock.guest_cid
                )));
            }
        }

        // Validate drive IDs are unique
        let mut drive_ids = vec![self.root_drive.drive_id.clone()];
        for extra in &self.extra_drives {
            if drive_ids.contains(&extra.drive_id) {
                return Err(VmError::Config(format!(
                    "duplicate drive_id: {}",
                    extra.drive_id
                )));
            }
            drive_ids.push(extra.drive_id.clone());
        }

        if let Some(network) = &self.network {
            network.validate()?;
        }

        Ok(())
    }
}

/// Configuration for a block device (drive).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriveConfig {
    /// Unique drive identifier
    pub drive_id: String,
    /// Path to drive image on host
    pub path_on_host: PathBuf,
    /// Whether this is the root device
    pub is_root_device: bool,
    /// Read-only flag
    pub is_read_only: bool,
}

impl Default for DriveConfig {
    fn default() -> Self {
        Self {
            drive_id: "rootfs".into(),
            path_on_host: PathBuf::from("/var/lib/bouvet/images/debian.ext4"),
            is_root_device: true,
            is_read_only: false,
        }
    }
}

/// Network interface configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// Network interface ID
    pub iface_id: String,
    /// Host device name (tap device)
    pub host_dev_name: String,
    /// Guest MAC address (optional, auto-generated if None)
    pub guest_mac: Option<String>,
    /// Optional host-side TAP/NAT setup for internet access.
    #[serde(default)]
    pub host_network: Option<HostNetworkConfig>,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            iface_id: "eth0".into(),
            host_dev_name: "tap0".into(),
            guest_mac: None,
            host_network: None,
        }
    }
}

impl NetworkConfig {
    /// Validate the network configuration.
    pub fn validate(&self) -> Result<()> {
        if self.iface_id.is_empty() {
            return Err(VmError::Config("network iface_id must not be empty".into()));
        }

        if self.host_dev_name.is_empty() {
            return Err(VmError::Config(
                "network host_dev_name must not be empty".into(),
            ));
        }

        // Linux IFNAMSIZ is 16 bytes including the null terminator.
        if self.host_dev_name.len() > 15 {
            return Err(VmError::Config(format!(
                "network host_dev_name must be at most 15 bytes, got {}",
                self.host_dev_name
            )));
        }

        if let Some(host_network) = &self.host_network {
            host_network.validate()?;
        }

        Ok(())
    }
}

/// Host-side network setup for a Firecracker TAP interface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostNetworkConfig {
    /// IPv4 address assigned to the host TAP interface.
    pub host_ip: Ipv4Addr,
    /// IPv4 address assigned to the guest interface.
    pub guest_ip: Ipv4Addr,
    /// IPv4 prefix length assigned to the TAP interface.
    pub prefix_len: u8,
    /// Netmask passed to the guest via kernel boot arguments.
    pub guest_netmask: Ipv4Addr,
    /// Optional outbound host interface used for MASQUERADE rules.
    pub outbound_iface: Option<String>,
    /// Destination CIDRs that internet-enabled guests cannot reach.
    #[serde(default)]
    pub blocked_cidrs: Vec<String>,
    /// Whether to enable IPv4 forwarding with sysctl.
    pub enable_ip_forward: bool,
    /// Whether to install per-guest iptables NAT/forwarding rules.
    pub enable_masquerade: bool,
}

impl Default for HostNetworkConfig {
    fn default() -> Self {
        Self {
            host_ip: Ipv4Addr::new(172, 30, 0, 1),
            guest_ip: Ipv4Addr::new(172, 30, 0, 2),
            prefix_len: 30,
            guest_netmask: Ipv4Addr::new(255, 255, 255, 252),
            outbound_iface: None,
            blocked_cidrs: vec![
                "0.0.0.0/8".into(),
                "10.0.0.0/8".into(),
                "100.64.0.0/10".into(),
                "127.0.0.0/8".into(),
                "169.254.0.0/16".into(),
                "172.16.0.0/12".into(),
                "172.30.0.0/16".into(),
                "192.168.0.0/16".into(),
                "198.18.0.0/15".into(),
                "224.0.0.0/4".into(),
                "240.0.0.0/4".into(),
            ],
            enable_ip_forward: true,
            enable_masquerade: true,
        }
    }
}

impl HostNetworkConfig {
    /// Validate host-side network configuration.
    pub fn validate(&self) -> Result<()> {
        if self.prefix_len == 0 || self.prefix_len > 32 {
            return Err(VmError::Config(format!(
                "host network prefix_len must be 1-32, got {}",
                self.prefix_len
            )));
        }

        if self.host_ip == self.guest_ip {
            return Err(VmError::Config(
                "host network host_ip and guest_ip must differ".into(),
            ));
        }

        if let Some(iface) = &self.outbound_iface {
            if iface.is_empty() {
                return Err(VmError::Config(
                    "host network outbound_iface must not be empty".into(),
                ));
            }
        }

        for cidr in &self.blocked_cidrs {
            if cidr.is_empty() {
                return Err(VmError::Config(
                    "host network blocked CIDR must not be empty".into(),
                ));
            }
        }

        Ok(())
    }
}

/// vsock configuration for guest-host communication.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VsockConfig {
    /// Guest CID (Context ID), must be > 2
    pub guest_cid: u32,
    /// Path to vsock Unix Domain Socket on host
    pub uds_path: PathBuf,
}

impl Default for VsockConfig {
    fn default() -> Self {
        Self {
            guest_cid: 3,
            uds_path: PathBuf::from("/tmp/bouvet-vsock.sock"),
        }
    }
}

impl VsockConfig {
    /// Create a vsock config for a specific VM.
    ///
    /// This generates a unique UDS path based on the VM ID.
    ///
    /// # Arguments
    /// * `cid` - Guest CID (must be > 2)
    /// * `chroot_path` - Base chroot path for VMs
    /// * `vm_id` - Unique VM identifier
    pub fn for_vm(cid: u32, chroot_path: &Path, vm_id: &str) -> Self {
        Self {
            guest_cid: cid,
            uds_path: chroot_path.join(vm_id).join("v.sock"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_vcpu() {
        let config = MachineConfig {
            vcpu_count: 0,
            ..Default::default()
        };
        assert!(config.validate().is_err());

        let config = MachineConfig {
            vcpu_count: 33,
            ..Default::default()
        };
        assert!(config.validate().is_err());

        let config = MachineConfig {
            vcpu_count: 4,
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_memory() {
        let config = MachineConfig {
            memory_mib: 64,
            ..Default::default()
        };
        assert!(config.validate().is_err());

        let config = MachineConfig {
            memory_mib: 128,
            ..Default::default()
        };
        assert!(config.validate().is_ok());

        // Test upper bound
        let config = MachineConfig {
            memory_mib: 32769,
            ..Default::default()
        };
        assert!(config.validate().is_err());

        let config = MachineConfig {
            memory_mib: 32768,
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_vsock_cid() {
        let config = MachineConfig {
            vsock: Some(VsockConfig {
                guest_cid: 2,
                uds_path: PathBuf::from("/tmp/test.sock"),
            }),
            ..Default::default()
        };
        assert!(config.validate().is_err());

        let config = MachineConfig {
            vsock: Some(VsockConfig {
                guest_cid: 3,
                uds_path: PathBuf::from("/tmp/test.sock"),
            }),
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_network_name() {
        let config = MachineConfig {
            network: Some(NetworkConfig {
                host_dev_name: "tap-name-that-is-too-long".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(config.validate().is_err());

        let config = MachineConfig {
            network: Some(NetworkConfig {
                host_dev_name: "bvt12345678".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_host_network() {
        let config = MachineConfig {
            network: Some(NetworkConfig {
                host_network: Some(HostNetworkConfig {
                    prefix_len: 0,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(config.validate().is_err());

        let config = MachineConfig {
            network: Some(NetworkConfig {
                host_network: Some(HostNetworkConfig {
                    guest_ip: HostNetworkConfig::default().host_ip,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(config.validate().is_err());

        let config = MachineConfig {
            network: Some(NetworkConfig {
                host_network: Some(HostNetworkConfig::default()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(config.validate().is_ok());

        let config = MachineConfig {
            network: Some(NetworkConfig {
                host_network: Some(HostNetworkConfig {
                    blocked_cidrs: vec!["".into()],
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_duplicate_drive_ids() {
        let mut config = MachineConfig::default();
        config.extra_drives.push(DriveConfig {
            drive_id: "rootfs".into(), // Same as root drive!
            path_on_host: PathBuf::from("/tmp/extra.ext4"),
            is_root_device: false,
            is_read_only: true,
        });
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_vsock_for_vm() {
        let config = VsockConfig::for_vm(5, &PathBuf::from("/tmp/bouvet"), "vm-123");
        assert_eq!(config.guest_cid, 5);
        assert_eq!(config.uds_path, PathBuf::from("/tmp/bouvet/vm-123/v.sock"));
    }
}
