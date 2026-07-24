# ManifestOS strata — shell integration (PATH + command-not-found).
#
# NOTE: keep this in sync with `strata::cnf_handler_script` in src/strata.rs —
# the engine writes the same content to /etc/manifest-os/strata-cnf.sh on every
# install; this baked copy is what makes the *live* ISO session behave the same.
#
# Sourced by interactive bash/zsh. Does two things: puts exposed foreign-distro
# binaries (/strata/.bin) on PATH, and offers to add a stratum when an
# uninstalled package manager is typed.
case ":$PATH:" in
  *:/strata/.bin:*) ;;
  *) PATH="/strata/.bin:$PATH"; export PATH ;;
esac
__manifest_cnf() {
  cmd=$1
  case $cmd in
    apt|apt-get|apt-cache|dpkg|dpkg-query|add-apt-repository) distro=debian ;;
    dnf|dnf5|yum|rpm|rpm2cpio) distro=fedora ;;
    *) return 127 ;;
  esac
  printf '\n%s is not installed — it comes from %s.\n' "$cmd" "$distro" >&2
  if [ -t 0 ] && [ -t 2 ]; then
    printf 'Add a %s stratum and put %s on your PATH? [y/N] ' "$distro" "$cmd" >&2
    read -r __r
    case $__r in
      [yY]|[yY][eE][sS])
        sudo manifest strata add "$distro" --expose "$cmd" || return $?
        case ":$PATH:" in *:/strata/.bin:*) ;; *) PATH="/strata/.bin:$PATH"; export PATH ;; esac
        hash -r 2>/dev/null
        "$@"
        return $?
        ;;
    esac
  fi
  printf 'Add it with:  sudo manifest strata add %s --expose %s\n' "$distro" "$cmd" >&2
  return 127
}
command_not_found_handle() { __manifest_cnf "$@"; }
command_not_found_handler() { __manifest_cnf "$@"; }
