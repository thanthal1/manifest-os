# fix for screen readers
if grep -Fqa 'accessibility=' /proc/cmdline &> /dev/null; then
    setopt SINGLE_LINE_ZLE
fi

~/.automated_script.sh

# Manifest OS: launch the installer on the primary console.
#
#   * Default: the graphical installer (GTK4) inside a `cage` kiosk Wayland
#     session, software-rendered so it works without GPU accel (VMs without 3D
#     and weak hardware). If the compositor/GUI can't start, it falls back to the
#     text installer, then the simple welcome shell.
#   * `manifest.text` on the kernel cmdline (the "text installer (low memory)"
#     boot entry) skips the GUI entirely — the lightweight path.
if [[ $(tty) == "/dev/tty1" && -z "${MANIFEST_NO_WELCOME:-}" ]]; then
    if grep -qa 'manifest.text' /proc/cmdline; then
        manifest tui || manifest-welcome
    else
        export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/0}"
        mkdir -p "$XDG_RUNTIME_DIR" 2>/dev/null
        export WLR_RENDERER=pixman       # software render — no GPU required
        export WLR_NO_HARDWARE_CURSORS=1
        export GSK_RENDERER=cairo        # GTK software renderer
        export GDK_BACKEND=wayland
        # cage needs a seat manager. We run as root, so start seatd and use it
        # (libseat has no usable "builtin" backend in Arch's build).
        if ! pgrep -x seatd >/dev/null 2>&1; then
            seatd >/var/log/seatd.log 2>&1 &
            sleep 1
        fi
        export LIBSEAT_BACKEND=seatd
        if command -v cage >/dev/null && command -v manifest-gui >/dev/null \
           && cage -- manifest-gui; then
            :
        else
            manifest tui || manifest-welcome
        fi
    fi
fi
