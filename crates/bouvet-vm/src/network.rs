//! Host-side network setup for Firecracker TAP devices.

use crate::config::HostNetworkConfig;
use crate::error::{Result, VmError};
use tokio::process::Command;

/// Host network resources owned by a VM.
#[derive(Debug)]
pub(crate) struct HostNetworkLease {
    tap_name: String,
    config: HostNetworkConfig,
    tap_created: bool,
    host_input_rule_added: bool,
    spoofed_forward_rule_added: bool,
    blocked_forward_rules_added: usize,
    nat_rule_added: bool,
    forward_out_rule_added: bool,
    forward_in_rule_added: bool,
}

impl HostNetworkLease {
    /// Create a TAP device and optional host NAT/forwarding rules.
    pub(crate) async fn setup(tap_name: &str, config: &HostNetworkConfig) -> Result<Self> {
        let mut lease = Self {
            tap_name: tap_name.to_string(),
            config: config.clone(),
            tap_created: false,
            host_input_rule_added: false,
            spoofed_forward_rule_added: false,
            blocked_forward_rules_added: 0,
            nat_rule_added: false,
            forward_out_rule_added: false,
            forward_in_rule_added: false,
        };

        if let Err(err) = lease.configure().await {
            lease.cleanup().await;
            return Err(err);
        }

        Ok(lease)
    }

    async fn configure(&mut self) -> Result<()> {
        run(
            "ip",
            &[
                "tuntap",
                "add",
                "dev",
                self.tap_name.as_str(),
                "mode",
                "tap",
            ],
        )
        .await?;
        self.tap_created = true;

        let cidr = format!("{}/{}", self.config.host_ip, self.config.prefix_len);
        run(
            "ip",
            &["addr", "add", cidr.as_str(), "dev", self.tap_name.as_str()],
        )
        .await?;
        run("ip", &["link", "set", "dev", self.tap_name.as_str(), "up"]).await?;

        if self.config.enable_ip_forward {
            run("sysctl", &["-w", "net.ipv4.ip_forward=1"]).await?;
        }

        if self.config.enable_masquerade {
            self.add_iptables_rules().await?;
        }

        Ok(())
    }

    async fn add_iptables_rules(&mut self) -> Result<()> {
        let source = format!("{}/32", self.config.guest_ip);

        // Prevent TAP traffic from reaching services in the host/container namespace,
        // including the TAP gateway IP, regardless of source spoofing.
        run_owned(
            "iptables",
            &[
                "-I".to_string(),
                "INPUT".to_string(),
                "-i".to_string(),
                self.tap_name.clone(),
                "-j".to_string(),
                "DROP".to_string(),
            ],
        )
        .await?;
        self.host_input_rule_added = true;

        // Only the configured guest address may be forwarded from this TAP.
        run_owned(
            "iptables",
            &[
                "-I".to_string(),
                "FORWARD".to_string(),
                "-i".to_string(),
                self.tap_name.clone(),
                "!".to_string(),
                "-s".to_string(),
                source.clone(),
                "-j".to_string(),
                "DROP".to_string(),
            ],
        )
        .await?;
        self.spoofed_forward_rule_added = true;

        // Prevent internet-enabled guests from reaching other sandboxes, Docker/host
        // private networks, link-local metadata endpoints, and other non-public ranges.
        for blocked_cidr in &self.config.blocked_cidrs {
            run_owned(
                "iptables",
                &[
                    "-I".to_string(),
                    "FORWARD".to_string(),
                    "-i".to_string(),
                    self.tap_name.clone(),
                    "-s".to_string(),
                    source.clone(),
                    "-d".to_string(),
                    blocked_cidr.clone(),
                    "-j".to_string(),
                    "DROP".to_string(),
                ],
            )
            .await?;
            self.blocked_forward_rules_added += 1;
        }

        let mut nat_args = vec![
            "-t".to_string(),
            "nat".to_string(),
            "-A".to_string(),
            "POSTROUTING".to_string(),
            "-s".to_string(),
            source.clone(),
        ];
        if let Some(outbound_iface) = &self.config.outbound_iface {
            nat_args.push("-o".to_string());
            nat_args.push(outbound_iface.clone());
        }
        nat_args.extend(["-j".to_string(), "MASQUERADE".to_string()]);
        run_owned("iptables", &nat_args).await?;
        self.nat_rule_added = true;

        run_owned(
            "iptables",
            &[
                "-A".to_string(),
                "FORWARD".to_string(),
                "-i".to_string(),
                self.tap_name.clone(),
                "-s".to_string(),
                source.clone(),
                "-j".to_string(),
                "ACCEPT".to_string(),
            ],
        )
        .await?;
        self.forward_out_rule_added = true;

        run_owned(
            "iptables",
            &[
                "-A".to_string(),
                "FORWARD".to_string(),
                "-o".to_string(),
                self.tap_name.clone(),
                "-d".to_string(),
                source,
                "-m".to_string(),
                "conntrack".to_string(),
                "--ctstate".to_string(),
                "RELATED,ESTABLISHED".to_string(),
                "-j".to_string(),
                "ACCEPT".to_string(),
            ],
        )
        .await?;
        self.forward_in_rule_added = true;

        Ok(())
    }

    /// Best-effort cleanup of host network resources.
    pub(crate) async fn cleanup(&self) {
        if self.forward_in_rule_added {
            let source = format!("{}/32", self.config.guest_ip);
            run_cleanup(
                "iptables",
                &[
                    "-D",
                    "FORWARD",
                    "-o",
                    self.tap_name.as_str(),
                    "-d",
                    source.as_str(),
                    "-m",
                    "conntrack",
                    "--ctstate",
                    "RELATED,ESTABLISHED",
                    "-j",
                    "ACCEPT",
                ],
            )
            .await;
        }

        if self.forward_out_rule_added {
            let source = format!("{}/32", self.config.guest_ip);
            run_cleanup(
                "iptables",
                &[
                    "-D",
                    "FORWARD",
                    "-i",
                    self.tap_name.as_str(),
                    "-s",
                    source.as_str(),
                    "-j",
                    "ACCEPT",
                ],
            )
            .await;
        }

        if self.blocked_forward_rules_added > 0 {
            let source = format!("{}/32", self.config.guest_ip);
            for blocked_cidr in self
                .config
                .blocked_cidrs
                .iter()
                .take(self.blocked_forward_rules_added)
            {
                run_cleanup(
                    "iptables",
                    &[
                        "-D",
                        "FORWARD",
                        "-i",
                        self.tap_name.as_str(),
                        "-s",
                        source.as_str(),
                        "-d",
                        blocked_cidr.as_str(),
                        "-j",
                        "DROP",
                    ],
                )
                .await;
            }
        }

        if self.spoofed_forward_rule_added {
            let source = format!("{}/32", self.config.guest_ip);
            run_cleanup(
                "iptables",
                &[
                    "-D",
                    "FORWARD",
                    "-i",
                    self.tap_name.as_str(),
                    "!",
                    "-s",
                    source.as_str(),
                    "-j",
                    "DROP",
                ],
            )
            .await;
        }

        if self.host_input_rule_added {
            run_cleanup(
                "iptables",
                &["-D", "INPUT", "-i", self.tap_name.as_str(), "-j", "DROP"],
            )
            .await;
        }

        if self.nat_rule_added {
            let source = format!("{}/32", self.config.guest_ip);
            let mut nat_args = vec!["-t", "nat", "-D", "POSTROUTING", "-s", source.as_str()];
            if let Some(outbound_iface) = &self.config.outbound_iface {
                nat_args.push("-o");
                nat_args.push(outbound_iface.as_str());
            }
            nat_args.extend(["-j", "MASQUERADE"]);
            run_cleanup("iptables", &nat_args).await;
        }

        if self.tap_created {
            run_cleanup("ip", &["link", "delete", "dev", self.tap_name.as_str()]).await;
        }
    }
}

async fn run(program: &str, args: &[&str]) -> Result<()> {
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
    run_owned(program, &args).await
}

async fn run_owned(program: &str, args: &[String]) -> Result<()> {
    tracing::trace!(program, args = ?args, "Running host network command");
    let output = Command::new(program)
        .args(args)
        .output()
        .await
        .map_err(|e| {
            VmError::Config(format!("failed to run host network command {program}: {e}"))
        })?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    Err(VmError::Config(format!(
        "host network command failed: {} {} (status: {}, stdout: {}, stderr: {})",
        program,
        args.join(" "),
        output.status,
        stdout.trim(),
        stderr.trim()
    )))
}

async fn run_cleanup(program: &str, args: &[&str]) {
    tracing::trace!(program, args = ?args, "Running host network cleanup command");
    match Command::new(program).args(args).output().await {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!(
                program,
                args = ?args,
                status = %output.status,
                stderr = %stderr.trim(),
                "Host network cleanup command failed"
            );
        }
        Err(error) => {
            tracing::warn!(
                program,
                args = ?args,
                %error,
                "Failed to run host network cleanup command"
            );
        }
    }
}
