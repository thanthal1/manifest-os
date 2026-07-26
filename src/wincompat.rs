//! **Will this Windows app run under Wine?** — the compatibility oracle behind
//! the `windows` block (`docs/strata-design.md` §14, Phase 6a).
//!
//! Wine's failure modes are not random: they cluster around a handful of things
//! Wine structurally cannot do — load a Windows **kernel driver**, satisfy
//! **kernel-level anti-cheat**, run **Store (MSIX/APPX)** packages, talk to a
//! **hardware licence dongle**, or provide the deep GPU/driver stack that heavy
//! **CAD** wants. Those are knowable *before* you spend twenty minutes on an
//! install that was never going to work, which is the point of this module: tell
//! the user up front, and route the app to the tier that can actually run it.
//!
//! Two independent signals, both pure functions over data:
//!
//! 1. **Name/knowledge-base matching** — a curated table of app families with a
//!    known verdict (SolidWorks needs a VM; Notepad++ is fine).
//! 2. **Installer marker scanning** — strings in the actual installer binary
//!    (`EasyAntiCheat`, `vgk.sys`, `HASP`) that betray a blocker regardless of
//!    what the app calls itself.
//!
//! The oracle never blocks by itself: [`WindowsApp::force`] overrides it, and the
//! reasons are always printed so the verdict is auditable rather than magic.

/// How well an app is expected to run under Wine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    /// Known-good under Wine.
    Works,
    /// No red flags found, but unverified — most apps land here.
    Likely,
    /// Something suspicious (heavy .NET, DRM, GPU-hungry) — may need work.
    Risky,
    /// Structurally can't work under Wine; needs a real Windows VM.
    Blocked,
}

impl Verdict {
    pub fn label(&self) -> &'static str {
        match self {
            Verdict::Works => "works",
            Verdict::Likely => "likely works",
            Verdict::Risky => "risky",
            Verdict::Blocked => "won't work under Wine",
        }
    }
    /// The tier that can actually run this: `wine`, or a VM for blockers.
    pub fn tier(&self) -> &'static str {
        match self {
            Verdict::Blocked => "vm-rdp",
            _ => "wine",
        }
    }
}

/// The oracle's answer: a verdict, the reasons behind it, and any winetricks
/// verbs the app is known to need.
#[derive(Debug, Clone)]
pub struct Assessment {
    pub verdict: Verdict,
    pub reasons: Vec<String>,
    pub suggest_winetricks: Vec<String>,
}

impl Assessment {
    /// A one-line summary for the CLI/logs.
    pub fn summary(&self, name: &str) -> String {
        format!("{name}: {} — {}", self.verdict.label(), self.reasons.join("; "))
    }
}

/// (keyword, verdict, reason, winetricks) — matched case-insensitively against
/// the app name and installer filename. Order matters only for readability; the
/// worst verdict always wins.
type Rule = (&'static str, Verdict, &'static str, &'static [&'static str]);

/// The knowledge base. Deliberately small and honest: entries are things with a
/// well-known, stable Wine story, not a guess at every app in existence.
const RULES: &[Rule] = &[
    // ---- structurally blocked: kernel anti-cheat ----
    ("easyanticheat", Verdict::Blocked, "kernel-level anti-cheat (EasyAntiCheat) can't load under Wine", &[]),
    ("battleye", Verdict::Blocked, "kernel-level anti-cheat (BattlEye) can't load under Wine", &[]),
    ("vanguard", Verdict::Blocked, "Riot Vanguard is a kernel driver — impossible under Wine", &[]),
    ("valorant", Verdict::Blocked, "ships Riot Vanguard, a kernel anti-cheat driver", &[]),
    ("gameguard", Verdict::Blocked, "nProtect GameGuard is a kernel driver", &[]),
    ("xigncode", Verdict::Blocked, "XIGNCODE3 anti-cheat operates at kernel level", &[]),
    ("faceit", Verdict::Blocked, "FACEIT anti-cheat is a kernel driver", &[]),
    // ---- structurally blocked: CAD / engineering (the anchor use case) ----
    ("solidworks", Verdict::Blocked, "SolidWorks needs a real Windows driver stack — the VM+GPU path", &[]),
    ("autocad", Verdict::Blocked, "AutoCAD's installer and licensing don't work under Wine", &[]),
    ("inventor", Verdict::Blocked, "Autodesk Inventor requires a real Windows environment", &[]),
    ("revit", Verdict::Blocked, "Autodesk Revit requires a real Windows environment", &[]),
    ("fusion 360", Verdict::Risky, "Fusion 360 partly runs under Wine but the installer/updater fight it", &[]),
    ("catia", Verdict::Blocked, "CATIA requires a real Windows environment", &[]),
    ("altium", Verdict::Blocked, "Altium Designer requires a real Windows environment", &[]),
    // ---- structurally blocked: drivers, dongles, security software ----
    ("hasp", Verdict::Blocked, "HASP/Sentinel hardware licence dongle needs a Windows kernel driver", &[]),
    ("sentinel", Verdict::Blocked, "Sentinel licence dongle needs a Windows kernel driver", &[]),
    ("codemeter", Verdict::Blocked, "CodeMeter licensing needs a Windows kernel driver", &[]),
    ("antivirus", Verdict::Blocked, "antivirus hooks the Windows kernel — no Wine equivalent", &[]),
    ("virtualbox", Verdict::Blocked, "a hypervisor needs kernel drivers; use the Linux build", &[]),
    // ---- store packaging ----
    (".msix", Verdict::Blocked, "MSIX/Store packaging isn't supported by Wine", &[]),
    (".appx", Verdict::Blocked, "APPX/Store packaging isn't supported by Wine", &[]),
    // ---- risky: known to need work ----
    ("office 365", Verdict::Risky, "Microsoft 365's installer (Click-to-Run) rarely completes under Wine", &[]),
    ("microsoft office", Verdict::Risky, "recent Office versions are unreliable under Wine", &["corefonts"]),
    ("onedrive", Verdict::Risky, "OneDrive's sync engine is poorly supported", &[]),
    ("adobe", Verdict::Risky, "recent Adobe CC apps generally fail; older versions vary", &["corefonts", "atmlib"]),
    ("photoshop", Verdict::Risky, "only older Photoshop releases are reliable under Wine", &["corefonts", "atmlib"]),
    ("itunes", Verdict::Risky, "iTunes installs but device sync (USB) usually doesn't work", &[]),
    ("visual studio", Verdict::Risky, "full Visual Studio is not usable under Wine (VS Code is native)", &[]),
    // ---- known good ----
    ("notepad++", Verdict::Works, "well-supported under Wine", &[]),
    ("7-zip", Verdict::Works, "well-supported under Wine", &[]),
    ("winrar", Verdict::Works, "well-supported under Wine", &[]),
    ("irfanview", Verdict::Works, "well-supported under Wine", &[]),
    ("foobar2000", Verdict::Works, "well-supported under Wine", &[]),
    ("paint.net", Verdict::Likely, "runs with the .NET runtime installed", &["dotnet48"]),
    ("steam", Verdict::Likely, "Steam itself runs; individual games vary (and Proton is native)", &[]),
];

/// Binary markers found *inside* an installer that betray a blocker whatever the
/// app is called. Scanned as raw bytes so no decoding is needed.
const MARKERS: &[(&str, Verdict, &str)] = &[
    ("EasyAntiCheat", Verdict::Blocked, "installer bundles EasyAntiCheat (kernel anti-cheat)"),
    ("BEService", Verdict::Blocked, "installer bundles BattlEye (kernel anti-cheat)"),
    ("vgk.sys", Verdict::Blocked, "installer bundles Riot Vanguard (kernel driver)"),
    ("GameGuard", Verdict::Blocked, "installer bundles nProtect GameGuard (kernel driver)"),
    ("hasplms", Verdict::Blocked, "installer bundles a HASP/Sentinel dongle driver"),
    ("CodeMeter", Verdict::Blocked, "installer bundles CodeMeter licensing (kernel driver)"),
    (".sys", Verdict::Risky, "installer appears to ship a kernel driver (.sys)"),
    ("SecuROM", Verdict::Blocked, "SecuROM DRM uses a kernel driver"),
    ("StarForce", Verdict::Blocked, "StarForce DRM uses a kernel driver"),
];

/// Assess an app by **name** (and optionally its installer filename). Pure.
pub fn assess(name: &str, installer: Option<&str>) -> Assessment {
    let hay = format!("{} {}", name, installer.unwrap_or("")).to_lowercase();
    // The worst *matched* rule wins. Tracked as an Option so a known-good match
    // (`Works`, which sorts below `Likely`) isn't masked by the default.
    let mut matched: Option<Verdict> = None;
    let mut reasons = Vec::new();
    let mut tricks: Vec<String> = Vec::new();

    for (needle, v, why, verbs) in RULES {
        if hay.contains(needle) {
            matched = Some(matched.map_or(*v, |m: Verdict| m.max(*v)));
            reasons.push((*why).to_string());
            tricks.extend(verbs.iter().map(|s| (*s).to_string()));
        }
    }
    // An app we can't identify is the normal case — say so plainly rather than
    // implying knowledge we don't have.
    if reasons.is_empty() {
        reasons.push("no known blockers, but this app isn't in the compatibility list".into());
    }
    tricks.sort();
    tricks.dedup();
    Assessment { verdict: matched.unwrap_or(Verdict::Likely), reasons, suggest_winetricks: tricks }
}

/// Refine an assessment by scanning the installer's bytes for blocker markers.
/// Only reads the first `limit` bytes — installers are big and the interesting
/// strings live in the header/resources. Pure over the supplied bytes.
pub fn assess_bytes(base: &Assessment, bytes: &[u8]) -> Assessment {
    let mut out = base.clone();
    for (marker, v, why) in MARKERS {
        if find_ascii(bytes, marker.as_bytes()) {
            if *v > out.verdict {
                out.verdict = *v;
            }
            let why = (*why).to_string();
            if !out.reasons.contains(&why) {
                out.reasons.push(why);
            }
        }
    }
    // A real finding supersedes the "not in the list" placeholder.
    if out.reasons.len() > 1 {
        out.reasons.retain(|r| !r.starts_with("no known blockers"));
    }
    out
}

/// Case-sensitive substring search over raw bytes (installer strings are ASCII;
/// UTF-16 shows up too, so also try the naive wide form).
fn find_ascii(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || hay.len() < needle.len() {
        return false;
    }
    if hay.windows(needle.len()).any(|w| w == needle) {
        return true;
    }
    // UTF-16LE: each ASCII byte followed by 0x00.
    let wide: Vec<u8> = needle.iter().flat_map(|b| [*b, 0]).collect();
    hay.len() >= wide.len() && hay.windows(wide.len()).any(|w| w == wide)
}

/// Read the head of a local installer for [`assess_bytes`]. Returns `None` when
/// the path isn't readable (a URL, or not downloaded yet).
pub fn read_head(path: &str, limit: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; limit];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_anticheat_is_blocked() {
        let a = assess("Valorant", None);
        assert_eq!(a.verdict, Verdict::Blocked);
        assert!(a.reasons.iter().any(|r| r.contains("Vanguard")), "{a:?}");
        assert_eq!(a.verdict.tier(), "vm-rdp");
    }

    #[test]
    fn cad_is_blocked_and_routed_to_a_vm() {
        let a = assess("SolidWorks 2024", Some("sldworks_setup.exe"));
        assert_eq!(a.verdict, Verdict::Blocked);
        assert_eq!(a.verdict.tier(), "vm-rdp");
        assert!(a.reasons.iter().any(|r| r.contains("VM")), "{a:?}");
    }

    #[test]
    fn known_good_apps_pass_as_works() {
        assert_eq!(assess("Notepad++", None).verdict, Verdict::Works);
        assert_eq!(assess("7-Zip 23.01", Some("7z2301-x64.exe")).verdict, Verdict::Works);
    }

    #[test]
    fn unknown_apps_are_likely_and_say_so() {
        let a = assess("SomeInternalTool", Some("setup.exe"));
        assert_eq!(a.verdict, Verdict::Likely);
        assert!(a.reasons[0].contains("isn't in the compatibility list"), "{a:?}");
        assert_eq!(a.verdict.tier(), "wine");
    }

    #[test]
    fn store_packaging_is_blocked() {
        assert_eq!(assess("Some App", Some("app.msix")).verdict, Verdict::Blocked);
        assert_eq!(assess("Some App", Some("app.appx")).verdict, Verdict::Blocked);
    }

    #[test]
    fn winetricks_hints_come_through() {
        let a = assess("Paint.NET", None);
        assert!(a.suggest_winetricks.contains(&"dotnet48".to_string()), "{a:?}");
    }

    #[test]
    fn installer_markers_override_an_innocent_name() {
        // Name looks harmless; the bytes say kernel anti-cheat.
        let base = assess("Fun Game", Some("setup.exe"));
        assert_eq!(base.verdict, Verdict::Likely);
        let scanned = assess_bytes(&base, b"....EasyAntiCheat_Setup....");
        assert_eq!(scanned.verdict, Verdict::Blocked);
        assert!(scanned.reasons.iter().any(|r| r.contains("EasyAntiCheat")), "{scanned:?}");
        // The "unknown app" placeholder is dropped once we have a real finding.
        assert!(!scanned.reasons.iter().any(|r| r.starts_with("no known blockers")), "{scanned:?}");
    }

    #[test]
    fn markers_are_found_in_utf16_too() {
        let base = assess("Fun Game", None);
        // "vgk.sys" as UTF-16LE.
        let wide: Vec<u8> = b"vgk.sys".iter().flat_map(|b| [*b, 0]).collect();
        let scanned = assess_bytes(&base, &wide);
        assert_eq!(scanned.verdict, Verdict::Blocked, "{scanned:?}");
    }

    #[test]
    fn worst_verdict_wins_over_multiple_matches() {
        // Matches both "adobe" (Risky) and an anti-cheat marker via the name.
        let a = assess("Adobe Photoshop with EasyAntiCheat", None);
        assert_eq!(a.verdict, Verdict::Blocked, "{a:?}");
    }

    #[test]
    fn verdict_ordering_is_worst_last() {
        assert!(Verdict::Blocked > Verdict::Risky);
        assert!(Verdict::Risky > Verdict::Likely);
        assert!(Verdict::Likely > Verdict::Works);
    }
}
