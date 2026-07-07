//! Declarative user creation.
//!
//! Replaces `useradd` / `chpasswd` / sudoers-editing hooks — which are both
//! tedious and too security-sensitive to leave as raw shell. Idempotent: an
//! existing user is left in place.

use crate::exec::Ctx;
use crate::manifest::UserSpec;
use anyhow::Result;

pub fn apply(users: &[UserSpec], ctx: &Ctx) -> Result<()> {
    for u in users {
        if ctx.check("id", &["--", &u.name]) {
            println!("  · user {} already exists", u.name);
        } else {
            create(u, ctx)?;
        }
        if u.sudo {
            grant_sudo(&u.name, ctx)?;
        }
        if let Some(pw) = &u.password {
            ctx.set_password(&u.name, pw)?;
        }
    }
    Ok(())
}

fn create(u: &UserSpec, ctx: &Ctx) -> Result<()> {
    println!("  · creating user {}", u.name);
    let groups = u.groups.join(",");
    let mut args: Vec<&str> = vec!["-m"];
    if !u.groups.is_empty() {
        args.push("-G");
        args.push(&groups);
    }
    if let Some(shell) = &u.shell {
        args.push("-s");
        args.push(shell);
    }
    args.push("--");
    args.push(&u.name);
    ctx.sudo("useradd", &args)
}

/// Drop a validated sudoers file granting the user sudo. Written to
/// /etc/sudoers.d (the safe, non-clobbering way) with the required 0440 mode.
fn grant_sudo(name: &str, ctx: &Ctx) -> Result<()> {
    println!("  · granting sudo to {name}");
    let path = format!("/etc/sudoers.d/10-{name}");
    // Stage under a dot-suffixed name first — sudo ignores files whose name
    // contains a '.', so a file that fails validation never becomes live
    // sudoers config (a broken live drop-in disables sudo system-wide).
    let staged = format!("{path}.tmp");
    ctx.write_root(&staged, &format!("{name} ALL=(ALL:ALL) ALL\n"))?;
    ctx.sudo("chmod", &["0440", &staged])?;
    if let Err(e) = ctx.sudo("visudo", &["-cf", &staged]) {
        let _ = ctx.sudo("rm", &["-f", &staged]);
        return Err(e);
    }
    ctx.sudo("mv", &[staged.as_str(), path.as_str()])
}
