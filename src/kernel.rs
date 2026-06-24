//! Kernel selection.
//!
//! Every install gets a kernel — without one the system won't boot. When a
//! manifest doesn't name one, we install stock Arch mainline (`linux`), the
//! same kernel `pacstrap base` would give you. Authors switch it out via
//! `system.kernel`.
//!
//! Each kernel also has a matching `*-headers` package. Headers are installed
//! alongside the kernel because out-of-tree/DKMS modules (nvidia-dkms,
//! virtualbox, broadcom-wl, ...) fail to build without the headers that match
//! the *running* kernel — forgetting them is one of the most common Arch
//! setup mistakes.
//!
//! Installing a kernel package triggers pacman's mkinitcpio hook automatically,
//! so the initramfs is (re)generated for us. Adding a *bootloader* entry is a
//! separate concern owned by the ISO's boot step, not the manifest.

use anyhow::Result;

pub struct Kernel {
    pub key: &'static str,
    pub package: &'static str,
    pub headers: &'static str,
    /// Whether selecting this kernel requires the CachyOS repo + signing key.
    pub needs_cachyos_repo: bool,
    pub display: &'static str,
    pub notes: &'static str,
}

/// The kernel installed when a manifest specifies none: stock Arch mainline.
pub const DEFAULT_KEY: &str = "linux";

const CATALOG: &[Kernel] = &[
    Kernel {
        key: "linux",
        package: "linux",
        headers: "linux-headers",
        needs_cachyos_repo: false,
        display: "Mainline",
        notes: "Arch's default kernel. Installed automatically when no kernel is set.",
    },
    Kernel {
        key: "linux-lts",
        package: "linux-lts",
        headers: "linux-lts-headers",
        needs_cachyos_repo: false,
        display: "LTS",
        notes: "Long-term-support kernel — older but very stable. Good fallback alongside another kernel.",
    },
    Kernel {
        key: "linux-zen",
        package: "linux-zen",
        headers: "linux-zen-headers",
        needs_cachyos_repo: false,
        display: "Zen",
        notes: "Mainline tuned for desktop interactivity and responsiveness.",
    },
    Kernel {
        key: "linux-hardened",
        package: "linux-hardened",
        headers: "linux-hardened-headers",
        needs_cachyos_repo: false,
        display: "Hardened",
        notes: "Security-hardened mainline with extra exploit mitigations.",
    },
    Kernel {
        key: "linux-cachyos",
        package: "linux-cachyos",
        headers: "linux-cachyos-headers",
        needs_cachyos_repo: true,
        display: "CachyOS",
        notes: "BORE/EEVDF scheduler, x86-64-v3/v4 optimized. Pulls the CachyOS repo + signing key.",
    },
];

/// Manifest-facing aliases for kernel keys (the spec uses the short `cachy`).
fn normalize(key: &str) -> &str {
    match key {
        "cachy" | "cachyos" => "linux-cachyos",
        other => other,
    }
}

/// Resolve the kernel a manifest asked for, defaulting to stock `linux`.
pub fn resolve(key: Option<&str>) -> Result<&'static Kernel> {
    let raw = key.unwrap_or(DEFAULT_KEY);
    let wanted = normalize(raw);
    CATALOG.iter().find(|k| k.key == wanted).ok_or_else(|| {
        let names: Vec<&str> = CATALOG.iter().map(|k| k.key).collect();
        anyhow::anyhow!(
            "unknown kernel `{raw}` (expected one of: {}, or the alias `cachy`)",
            names.join(", ")
        )
    })
}

/// Every supported kernel, for `manifest kernels`.
pub fn catalog() -> &'static [Kernel] {
    CATALOG
}
