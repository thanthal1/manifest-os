//! Installer UI translations.
//!
//! English is the baseline: a plain array compiled straight into the binary,
//! so it's available instantly with no decompression step. Every other
//! language is a gzip'd `key\tvalue` catalog under `i18n/` at the repo root,
//! embedded via `include_bytes!` — the compressed bytes sit in the binary for
//! every language, but nothing is decompressed or parsed until that language
//! is actually selected.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Read;

/// (code, native display name) — shown in the welcome page's language picker.
pub const LANGUAGES: &[(&str, &str)] = &[
    ("en", "English"),
    ("es", "Español"),
    ("fr", "Français"),
    ("de", "Deutsch"),
    ("pt", "Português"),
    ("it", "Italiano"),
    ("ru", "Русский"),
    ("zh", "中文"),
    ("ja", "日本語"),
    ("ar", "العربية"),
    ("hi", "हिन्दी"),
];

/// The English baseline: every translatable key, with its English text. Also
/// the fallback for any key missing from another language's catalog.
static EN: &[(&str, &str)] = &[
    ("app.title", "Install Manifest OS"),
    ("common.advanced", "Advanced"),
    ("common.back", "Back"),
    ("common.continue", "Continue"),
    ("common.cancel", "Cancel"),
    ("common.cancelling", "Cancelling…"),
    ("common.click_again_cancel", "Click again to cancel"),
    ("common.password_weak", "Weak"),
    ("common.password_okay", "Okay"),
    ("common.password_strong", "Strong"),
    ("welcome.title", "Welcome to Manifest OS"),
    ("welcome.subtitle", "We'll set up your computer in a few simple steps. It only takes a few minutes."),
    ("welcome.larger_text", "Larger text"),
    ("welcome.language_label", "Language"),
    ("welcome.get_started", "Get started"),
    ("network.title", "Internet connection"),
    ("network.subtitle", "Manifest OS downloads your software while it installs, so it needs to be online."),
    ("network.connected", "✓ You're connected."),
    ("network.not_connected_wifi", "Not connected. Pick a Wi-Fi network below, or plug in Ethernet."),
    ("network.not_connected_ethernet", "Not connected. Plug in an Ethernet cable — it connects automatically — then press Continue."),
    ("network.wifi_password_placeholder", "Wi-Fi password"),
    ("network.scan", "Scan for networks"),
    ("network.scanning", "Scanning…"),
    ("network.connect", "Connect"),
    ("network.connecting", "Connecting…"),
    ("network.static_ip_label", "Static IP (CIDR)"),
    ("network.static_ip_placeholder", "e.g. 192.168.1.50/24 — leave blank for DHCP"),
    ("network.gateway_label", "Gateway"),
    ("network.gateway_placeholder", "e.g. 192.168.1.1"),
    ("network.dns_label", "DNS servers"),
    ("network.dns_placeholder", "comma-separated, e.g. 1.1.1.1,8.8.8.8"),
    ("network.proxy_label", "HTTP(S) proxy"),
    ("network.proxy_placeholder", "e.g. http://10.0.0.1:3128"),
    ("network.vlan_id_label", "VLAN ID"),
    ("network.vlan_id_placeholder", "e.g. 100 — leave blank for none"),
    ("network.vlan_parent_label", "VLAN parent interface"),
    ("network.vlan_parent_placeholder", "e.g. eth0"),
    ("setup.title", "Choose your setup"),
    ("setup.subtitle", "Pick a ready-made style. Each one is a complete, declared system."),
    ("setup.custom_placeholder", "Or paste a link (https://…) or a file path"),
    ("survey.title", "A few questions"),
    ("survey.subtitle", "Your chosen setup asks for a couple of details."),
    ("disk.title", "Where should it go?"),
    ("disk.subtitle", "Choose the disk to install onto. Everything on it will be erased."),
    ("disk.filesystem_label", "Filesystem"),
    ("disk.encryption_label", "Encryption"),
    ("disk.encryption_off", "Off"),
    ("disk.encryption_full", "Full disk"),
    ("disk.encryption_home", "Home only"),
    ("disk.root_size_label", "Root size (GiB) — the rest becomes /home"),
    ("disk.encryption_passphrase_placeholder", "Encryption passphrase"),
    ("disk.lvm_label", "Use LVM (root on a resizable logical volume)"),
    ("disk.raid1_label", "Mirror root across a second disk (RAID1)"),
    ("disk.timezone_label", "Timezone"),
    ("disk.locale_label", "Locale"),
    ("disk.locale_manual_label", "Or type a locale manually"),
    ("disk.locale_manual_placeholder", "overrides the list above, e.g. en_NZ.UTF-8"),
    ("disk.keymap_label", "Keymap"),
    ("disk.keymap_placeholder", "console keymap, e.g. us"),
    ("disk.printing_label", "Set up printing (CUPS)"),
    ("disk.desktop_app_label", "Install the System Snapshots app (turn off for a server)"),
    ("disk.post_script_label", "Post-install script (path on this USB)"),
    ("disk.post_script_placeholder", "run in the new system after everything else"),
    ("disk.alongside_size_label", "Space for Manifest OS (GiB)"),
    ("disk.alongside_size_placeholder", "40"),
    ("disk.nvidia_label", "Install NVIDIA driver (proprietary)"),
    ("disk.swap_label", "Swap"),
    ("disk.swap_zram", "zram"),
    ("disk.swap_none", "none"),
    ("disk.swap_file", "file"),
    ("disk.swap_partition", "partition"),
    ("disk.swap_size_placeholder", "Size (GiB)"),
    ("account.title", "Create your account"),
    ("account.subtitle", "This is how you'll sign in."),
    ("account.name_placeholder", "Your name"),
    ("account.password_placeholder", "Choose a password"),
    ("account.username_placeholder", "Username"),
    ("account.hostname_placeholder", "Computer name (hostname)"),
    ("account.root_password_label", "Root password (optional)"),
    ("account.root_password_placeholder", "Leave blank to keep root locked"),
    ("account.autologin_label", "Log in automatically"),
    ("account.extra_users_label", "Additional accounts (one per line: username:password or username:password:sudo)"),
    ("review.title", "Ready to install"),
    ("review.subtitle", "Please review — this will erase the selected disk."),
    ("review.setup_label", "Setup:"),
    ("review.disk_label", "Disk:"),
    ("review.account_label", "Account:"),
    ("review.filesystem_label", "Filesystem:"),
    ("review.swap_label", "Swap:"),
    ("review.install_now", "Install now"),
    ("installing.title", "Installing Manifest OS"),
    ("installing.subtitle", "Sit back — this takes a few minutes. Don't turn off your computer."),
    ("done.title", "All done!"),
    ("done.restart", "Restart now"),
    ("done.uefi_message", "Manifest OS is installed. Press Restart — you can leave the USB plugged in; it will boot into your new system."),
    ("done.bios_message", "Manifest OS is installed. Remove the install USB (or eject the disc), then press Restart."),
    ("error.title", "Something went wrong"),
    ("error.subtitle", "The install didn't finish. You can go back and try again."),
    ("error.back_to_start", "Back to start"),
];

/// The gzip-compressed `key\tvalue`-per-line catalog for a non-English
/// language, embedded at compile time. `None` for English (it needs none) or
/// an unrecognized code.
fn compressed(code: &str) -> Option<&'static [u8]> {
    Some(match code {
        "es" => include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/i18n/es.tsv.gz")),
        "fr" => include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/i18n/fr.tsv.gz")),
        "de" => include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/i18n/de.tsv.gz")),
        "pt" => include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/i18n/pt.tsv.gz")),
        "it" => include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/i18n/it.tsv.gz")),
        "ru" => include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/i18n/ru.tsv.gz")),
        "zh" => include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/i18n/zh.tsv.gz")),
        "ja" => include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/i18n/ja.tsv.gz")),
        "ar" => include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/i18n/ar.tsv.gz")),
        "hi" => include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/i18n/hi.tsv.gz")),
        _ => return None,
    })
}

fn parse_catalog(tsv: &str) -> HashMap<String, String> {
    tsv.lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

thread_local! {
    static CURRENT: RefCell<String> = RefCell::new("en".to_string());
    // Decompressed catalogs, cached after first use. Empty until a non-English
    // language is actually selected.
    static CACHE: RefCell<HashMap<String, HashMap<String, String>>> = RefCell::new(HashMap::new());
}

/// Select the active language for `t()`. Decompresses and parses that
/// language's catalog on first use, then caches it; a no-op for "en" or an
/// unrecognized code (falls back to English either way).
pub fn set_language(code: &str) {
    CURRENT.with(|c| *c.borrow_mut() = code.to_string());
    if code == "en" || CACHE.with(|c| c.borrow().contains_key(code)) {
        return;
    }
    let Some(bytes) = compressed(code) else { return };
    let mut gz = flate2::read::GzDecoder::new(bytes);
    let mut tsv = String::new();
    if gz.read_to_string(&mut tsv).is_ok() {
        let catalog = parse_catalog(&tsv);
        CACHE.with(|c| {
            c.borrow_mut().insert(code.to_string(), catalog);
        });
    }
}

pub fn current_language() -> String {
    CURRENT.with(|c| c.borrow().clone())
}

/// Translate `key` into the active language, falling back to English (or the
/// key itself, if even English is somehow missing it — a visible bug beats a
/// blank label).
pub fn t(key: &str) -> String {
    let lang = current_language();
    if lang != "en" {
        if let Some(v) = CACHE.with(|c| c.borrow().get(&lang).and_then(|m| m.get(key).cloned())) {
            return v;
        }
    }
    EN.iter().find(|(k, _)| *k == key).map(|(_, v)| v.to_string()).unwrap_or_else(|| key.to_string())
}
