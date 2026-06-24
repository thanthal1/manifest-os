# Test harness: a real Arch + pacman + makepkg environment to exercise the
# install pipeline end-to-end. Throwaway — never ship this.
#
#   docker build -f docker/Dockerfile -t manifest-test .
#   docker run --rm manifest-test install examples/bootstrap.json --dry-run
#   docker run --rm manifest-test install examples/bootstrap.json
FROM archlinux:latest

# Base toolchain: sudo for privilege escalation, base-devel+git for paru,
# rust to build the binary, curl for the CachyOS repo path.
RUN pacman -Syu --noconfirm --needed sudo base-devel git rust curl tzdata

# Non-root user with passwordless sudo. makepkg/paru refuse to run as root, and
# the install flow assumes a normal user who can escalate via sudo.
RUN useradd -m -G wheel tester && \
    echo 'tester ALL=(ALL:ALL) NOPASSWD: ALL' > /etc/sudoers.d/00-tester

USER tester
WORKDIR /home/tester/manifest
COPY --chown=tester:tester . .

RUN cargo build --release

ENTRYPOINT ["./target/release/manifest"]
