# fix for screen readers
if grep -Fqa 'accessibility=' /proc/cmdline &> /dev/null; then
    setopt SINGLE_LINE_ZLE
fi

~/.automated_script.sh

# Manifest OS: launch the guided installer on the primary console.
# Falls back to the simple welcome (then a shell) if the TUI exits.
if [[ $(tty) == "/dev/tty1" && -z "${MANIFEST_NO_WELCOME:-}" ]]; then
    manifest tui || manifest-welcome
fi
