//! Lightweight poller for local machine stats (CPU / memory / network).
//!
//! `sysinfo` is already a dependency for many Rust desktop apps; it gives us
//! cross-platform data with ~2% CPU overhead at 1-second cadence.

use std::time::Duration;

use sysinfo::{Disks, Networks, System};

use super::system_types::{SystemSampler, SystemSnapshot};

impl SystemSampler {
    pub fn new() -> Self {
        let mut sys = System::new_all();
        sys.refresh_all();
        let nets = Networks::new_with_refreshed_list();
        let (last_rx_total, last_tx_total) = net_totals(&nets);
        let disks = Disks::new_with_refreshed_list();
        Self {
            sys,
            nets,
            disks,
            last_rx_total,
            last_tx_total,
            last_instant: std::time::Instant::now(),
        }
    }

    /// Recommended poll interval for a UI sidebar.
    pub fn recommended_interval() -> Duration {
        Duration::from_millis(1000)
    }

    pub fn sample(&mut self) -> SystemSnapshot {
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();
        self.nets.refresh(true);

        let cpu_percent = self.sys.global_cpu_usage() / 100.0;

        let mem_total = self.sys.total_memory();
        let mem_used = self.sys.used_memory();
        let mem_percent = if mem_total > 0 {
            mem_used as f32 / mem_total as f32
        } else {
            0.0
        };

        let swap_total = self.sys.total_swap();
        let swap_used = self.sys.used_swap();
        let swap_percent = if swap_total > 0 {
            swap_used as f32 / swap_total as f32
        } else {
            0.0
        };

        // RX / TX bytes/sec from the delta across the physical iface list;
        // virtual adapters would double-count the same frames (see
        // `should_count_interface`).
        let (rx_total, tx_total) = net_totals(&self.nets);
        let now = std::time::Instant::now();
        let elapsed = now
            .duration_since(self.last_instant)
            .as_secs_f64()
            .max(0.001);
        let rx_delta = rx_total.saturating_sub(self.last_rx_total);
        let tx_delta = tx_total.saturating_sub(self.last_tx_total);
        self.last_rx_total = rx_total;
        self.last_tx_total = tx_total;
        self.last_instant = now;
        let net_rx_per_sec = (rx_delta as f64 / elapsed) as u64;
        let net_tx_per_sec = (tx_delta as f64 / elapsed) as u64;

        // Local filesystems (slow-changing, but cheap to refresh).
        self.disks.refresh(true);
        let disks: Vec<(String, u64, u64)> = self
            .disks
            .iter()
            .map(|d| {
                (
                    d.mount_point().to_string_lossy().to_string(),
                    d.available_space(),
                    d.total_space(),
                )
            })
            .filter(|(_, _, total)| *total > 0)
            .collect();

        SystemSnapshot {
            cpu_percent,
            mem_percent,
            swap_percent,
            mem_used_mib: mem_used / 1024 / 1024,
            mem_total_mib: mem_total / 1024 / 1024,
            swap_used_mib: swap_used / 1024 / 1024,
            swap_total_mib: swap_total / 1024 / 1024,
            net_bytes_per_sec: net_rx_per_sec + net_tx_per_sec,
            net_rx_per_sec,
            net_tx_per_sec,
            disks,
        }
    }
}

/// Which adapters count toward the aggregated local network rate.
///
/// A docker bridge, a WSL/Hyper-V `vEthernet` switch, a bond or the loopback
/// re-delivers frames that are already counted on the physical NIC, so summing
/// every adapter inflates the total (usually the receive side). Keep only the
/// physical adapters; if a machine carried traffic solely on a tunnel, the
/// local panel intentionally shows zero.
fn should_count_interface(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    // Loopback ("lo", "lo0", …) — but not Windows "Local Area Connection".
    if upper.starts_with("LO") && !upper.starts_with("LOCAL") {
        return false;
    }
    const EXCLUDED_PREFIXES: [&str; 19] = [
        "DOCKER",
        "VETH",
        "BR",
        "VIRBR",
        "BOND",
        "TAP",
        "TUN",
        "WG",
        "DUMMY",
        "CAN",
        "IFB",
        "AWDL",
        "UTUN",
        "ISATAP",
        "PPP",
        "VBOX",
        "MACVTAP",
        "MACVLAN",
        "TAILSCALE",
    ];
    if EXCLUDED_PREFIXES.iter().any(|p| upper.starts_with(p)) {
        return false;
    }
    // Brand prefixes stay ASCII even when the rest of the name is localized
    // (e.g. "vEthernet (默认交换机)").
    const EXCLUDED_SUBSTRINGS: [&str; 4] = ["VETHEREIN", "HYPER-V", "VMWARE", "WSL"];
    !EXCLUDED_SUBSTRINGS.iter().any(|s| upper.contains(s))
}

/// Sum the cumulative receive/transmit counters of the physical adapters only
/// (see [`should_count_interface`]).
///
/// An adapter that disappears and reappears (e.g. a Wi‑Fi reconnect) is
/// re‑baselined on the next sample, so its since‑boot counter isn't replayed;
/// at most one one‑second spike shows up in the graph.
fn net_totals(nets: &Networks) -> (u64, u64) {
    let mut rx = 0u64;
    let mut tx = 0u64;
    let mut counted = Vec::new();
    let mut skipped = Vec::new();
    for (name, data) in nets.iter() {
        if should_count_interface(name) {
            rx = rx.saturating_add(data.total_received());
            tx = tx.saturating_add(data.total_transmitted());
            counted.push(name.as_str());
        } else {
            skipped.push(name.as_str());
        }
    }
    tracing::debug!(
        counted = ?counted,
        skipped = ?skipped,
        "local net counters: physical adapters only"
    );
    (rx, tx)
}

/// Format a used/total memory pair (both in MiB) for the narrow sidebar.
/// Below 1 GiB it stays in megabytes (`512/2048M`); at or above, it switches to
/// gigabytes and drops the decimal for whole or large values to stay compact
/// (`1.5G/16G`, `120G/256G`).
pub fn format_mem(used_mib: u64, total_mib: u64) -> String {
    if total_mib < 1024 {
        return format!("{used_mib}/{total_mib}M");
    }
    // MiB → GiB, with a tidy width: integer when round or ≥100, else one decimal.
    fn gib(mib: u64) -> String {
        let g = mib as f64 / 1024.0;
        if g.fract() == 0.0 || g >= 100.0 {
            (g as u64).to_string()
        } else {
            format!("{g:.1}")
        }
    }
    format!("{}G/{}G", gib(used_mib), gib(total_mib))
}

/// Human-readable network throughput (e.g. `"1.2 MB/s"`).
pub fn format_bytes_per_sec(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B/s", "KB/s", "MB/s", "GB/s"];
    let mut value = bytes as f64;
    let mut idx = 0;
    while value >= 1024.0 && idx < UNITS.len() - 1 {
        value /= 1024.0;
        idx += 1;
    }
    if idx == 0 {
        format!("{} {}", bytes, UNITS[idx])
    } else {
        format!("{:.1} {}", value, UNITS[idx])
    }
}

#[cfg(test)]
mod interface_filter_tests {
    use super::*;

    #[test]
    fn physical_adapters_are_counted() {
        for name in [
            "eth0",
            "en0",
            "wlan0",
            "wlp2s0",
            "Ethernet",
            "WLAN",
            "以太网",
            "Wi-Fi",
            "enx001122334455",
            "Local Area Connection",
            "Local Area Connection* 2",
        ] {
            assert!(should_count_interface(name), "{name} should count");
        }
    }

    #[test]
    fn virtual_adapters_are_skipped() {
        for name in [
            "lo",
            "lo0",
            "docker0",
            "veth1a2b3c",
            "br0",
            "br-abc123",
            "virbr0",
            "bond0",
            "bonding_masters",
            "tap0",
            "tun0",
            "wg0",
            "dummy0",
            "can0",
            "ifb0",
            "bridge0",
            "awdl0",
            "utun4",
            "isatap.3.4.5.6",
            "ppp0",
            "vboxnet0",
            "macvtap0",
            "macvlan0",
            "tailscale0",
            "Tailscale",
            "vEthernet (WSL)",
            "vEthernet (Default Switch)",
            "vEthernet (默认交换机)",
            "VMware Network Adapter VMnet1",
            "Hyper-V 虚拟交换器 (Internal)",
            "Loopback Pseudo-Interface 1st Edition",
        ] {
            assert!(!should_count_interface(name), "{name} must be skipped");
        }
    }
}
