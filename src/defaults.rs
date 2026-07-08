//! Default applications and MIME associations.
//!
//! Instead of running `xdg-settings`/`xdg-mime` inside a login session, write the
//! freedesktop `mimeapps.list` file directly for the manifest's primary user.

use crate::exec::Ctx;
use crate::manifest::Defaults;
use anyhow::Result;
use std::collections::BTreeMap;

pub fn apply(defaults: &Defaults, primary_user: Option<&str>, ctx: &Ctx) -> Result<()> {
    if defaults.is_empty() {
        return Ok(());
    }

    let content = mimeapps_list(defaults);
    match primary_user {
        Some(user) => {
            let path = format!("/home/{user}/.config/mimeapps.list");
            ctx.write_root(&path, &content)?;
            ctx.sudo(
                "chown",
                &[
                    "-R",
                    &format!("{user}:{user}"),
                    &format!("/home/{user}/.config"),
                ],
            )?;
        }
        None => {
            let home = if ctx.dry_run {
                "$HOME".to_string()
            } else {
                std::env::var("HOME").unwrap_or_else(|_| "/root".to_string())
            };
            ctx.write_user(&format!("{home}/.config/mimeapps.list"), &content)?;
        }
    }
    Ok(())
}

fn mimeapps_list(defaults: &Defaults) -> String {
    let mut entries = BTreeMap::new();
    if let Some(browser) = defaults.browser.as_ref().filter(|b| !b.trim().is_empty()) {
        for mime in [
            "text/html",
            "x-scheme-handler/http",
            "x-scheme-handler/https",
            "x-scheme-handler/about",
            "x-scheme-handler/unknown",
        ] {
            entries.insert(mime.to_string(), browser.clone());
        }
    }
    for (mime, app) in &defaults.mime {
        if !mime.trim().is_empty() && !app.trim().is_empty() {
            entries.insert(mime.clone(), app.clone());
        }
    }

    let mut out = String::from("# Managed by Manifest OS\n[Default Applications]\n");
    for (mime, app) in entries {
        out.push_str(&mime);
        out.push('=');
        out.push_str(&app);
        out.push_str(";\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_expands_to_standard_handlers() {
        let d = Defaults {
            browser: Some("firefox.desktop".into()),
            mime: BTreeMap::new(),
        };
        let out = mimeapps_list(&d);
        assert!(out.contains("text/html=firefox.desktop;\n"));
        assert!(out.contains("x-scheme-handler/http=firefox.desktop;\n"));
        assert!(out.contains("x-scheme-handler/https=firefox.desktop;\n"));
    }

    #[test]
    fn explicit_mime_overrides_browser_default() {
        let mut mime = BTreeMap::new();
        mime.insert("text/html".into(), "org.gnome.Epiphany.desktop".into());
        let d = Defaults {
            browser: Some("firefox.desktop".into()),
            mime,
        };
        let out = mimeapps_list(&d);
        assert!(out.contains("text/html=org.gnome.Epiphany.desktop;\n"));
        assert!(!out.contains("text/html=firefox.desktop;\n"));
    }
}
