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

/// The CPU feature flags an x86-64-v3 kernel needs. `linux-cachyos` ships as a
/// **v3 build**, so a CPU missing any of these loads it and dies instantly with
/// no console output — a `quiet` boot then looks like it hung at "Loading
/// initial ramdisk", with no clue why. Intel fuses AVX2 off on plenty of
/// otherwise-modern Pentium/Celeron parts, so this is not a rare corner.
const V3_FLAGS: [&str; 4] = ["avx2", "bmi2", "fma", "movbe"];

/// Whether `flags` (a `/proc/cpuinfo` flags line) satisfies x86-64-v3.
fn flags_support_v3(flags: &str) -> bool {
    let have: Vec<&str> = flags.split_whitespace().collect();
    V3_FLAGS.iter().all(|f| have.contains(f))
}

/// Whether *this* CPU can run an x86-64-v3 build. Reads `/proc/cpuinfo`; on any
/// platform where that isn't readable (a non-Linux dev box) we assume it can,
/// so the check only ever *downgrades* on hard evidence — never on a guess.
pub fn cpu_supports_v3() -> bool {
    let Ok(info) = std::fs::read_to_string("/proc/cpuinfo") else {
        return true;
    };
    match info.lines().find(|l| l.starts_with("flags")) {
        Some(line) => flags_support_v3(line),
        None => true,
    }
}

/// Resolve the kernel, then refuse to hand back one this machine can't boot.
/// A manifest asking for `cachy` on a pre-v3 CPU silently falls back to stock
/// `linux` with a printed note — an install that boots beats an install that
/// matches the manifest exactly. Everything else passes through untouched.
pub fn resolve_bootable(key: Option<&str>) -> Result<&'static Kernel> {
    let k = resolve(key)?;
    if k.key == "linux-cachyos" && !cpu_supports_v3() {
        let fallback = resolve(Some(DEFAULT_KEY))?;
        println!(
            "  · note: this CPU lacks x86-64-v3 (needs {}), which `{}` requires —\n\
             \x20   falling back to `{}` so the system actually boots.",
            V3_FLAGS.join("/"),
            k.package,
            fallback.package
        );
        return Ok(fallback);
    }
    Ok(k)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real Haswell-era flags line (trimmed) — has everything v3 needs.
    const V3_CPU: &str = "flags : fpu vme de pse tsc msr sse sse2 ssse3 sse4_1 sse4_2 \
                          movbe popcnt aes avx f16c rdrand avx2 bmi1 bmi2 fma erms";
    // A Kaby Lake Celeron/Pentium: modern, but Intel fused AVX2/BMI2/FMA off.
    const NO_V3_CPU: &str = "flags : fpu vme de pse tsc msr sse sse2 ssse3 sse4_1 \
                             sse4_2 movbe popcnt aes rdrand erms";

    #[test]
    fn v3_detection_needs_every_flag() {
        assert!(flags_support_v3(V3_CPU));
        assert!(!flags_support_v3(NO_V3_CPU));
        // Missing just one flag is still not v3.
        assert!(!flags_support_v3("flags : avx2 bmi2 fma")); // no movbe
        assert!(!flags_support_v3(""));
    }

    #[test]
    fn v3_detection_is_not_fooled_by_substrings() {
        // "avx" must not satisfy "avx2", and "bmi1" must not satisfy "bmi2".
        assert!(!flags_support_v3("flags : avx bmi1 fma movbe"));
    }

    #[test]
    fn resolve_bootable_passes_through_non_cachy_kernels() {
        // These carry no CPU requirement, so they resolve unchanged whatever
        // the host CPU is (this test box included).
        for key in [None, Some("linux"), Some("linux-lts"), Some("linux-zen")] {
            let k = resolve_bootable(key).unwrap();
            assert_eq!(k.key, resolve(key).unwrap().key);
        }
    }

    #[test]
    fn cachy_alias_still_resolves() {
        assert_eq!(resolve(Some("cachy")).unwrap().key, "linux-cachyos");
        assert_eq!(resolve(Some("cachyos")).unwrap().key, "linux-cachyos");
    }
}
