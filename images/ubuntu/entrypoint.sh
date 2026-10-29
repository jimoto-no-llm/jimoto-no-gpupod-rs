#!/bin/sh
set -eu
: "${GPUPOD_SSH_PUBLIC_KEY:?A borrower SSH public key is required}"
mkdir -p /run/sshd /run/gpupod
printf '%s\n' "$GPUPOD_SSH_PUBLIC_KEY" > /run/gpupod/authorized_keys
chmod 0644 /run/gpupod/authorized_keys
chown pod:pod /workspace
ssh-keygen -A
exec /usr/sbin/sshd -D -e
