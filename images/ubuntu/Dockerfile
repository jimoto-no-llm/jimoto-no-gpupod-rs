ARG UBUNTU_VERSION=24.04
FROM ubuntu:${UBUNTU_VERSION}
RUN apt-get update \
    && apt-get install -y --no-install-recommends openssh-server ca-certificates tini \
    && rm -rf /var/lib/apt/lists/* /etc/ssh/ssh_host_* \
    && useradd --create-home --uid 10001 --shell /bin/bash pod \
    && usermod --password '*' pod \
    && mkdir -p /workspace /run/sshd /run/gpupod \
    && printf '%s\n' 'PasswordAuthentication no' 'KbdInteractiveAuthentication no' \
       'PermitRootLogin no' 'AllowUsers pod' 'AuthorizedKeysFile /run/gpupod/authorized_keys' \
       > /etc/ssh/sshd_config.d/gpupod.conf
COPY images/ubuntu/entrypoint.sh /usr/local/bin/workspace-entrypoint
RUN chmod 0755 /usr/local/bin/workspace-entrypoint
WORKDIR /workspace
EXPOSE 22 8888 6006 8080
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/workspace-entrypoint"]
