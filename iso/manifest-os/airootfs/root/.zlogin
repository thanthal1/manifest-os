# fix for screen readers
if grep -Fqa 'accessibility=' /proc/cmdline &> /dev/null; then
    setopt SINGLE_LINE_ZLE
fi

~/.automated_script.sh

# Manifest OS: launch the installer welcome on the primary console.
if [[ $(tty) == "/dev/tty1" && -z "${MANIFEST_NO_WELCOME:-}" ]]; then
    manifest-welcome
fi
