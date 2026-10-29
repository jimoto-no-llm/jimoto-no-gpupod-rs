ARG WORKSPACE_IMAGE=gpupod/ubuntu:24.04
FROM ${WORKSPACE_IMAGE}
ARG TORCH_VERSION=2.14.1
ARG TORCH_INDEX=cpu
RUN apt-get update \
    && apt-get install -y --no-install-recommends python3 python3-venv libgomp1 \
    && rm -rf /var/lib/apt/lists/* \
    && python3 -m venv /opt/venv \
    && /opt/venv/bin/pip install --no-cache-dir "torch==${TORCH_VERSION}" \
       --index-url "https://download.pytorch.org/whl/${TORCH_INDEX}" \
    && chown -R pod:pod /opt/venv
ENV PATH="/opt/venv/bin:${PATH}"
RUN printf '%s\n' 'export PATH=/opt/venv/bin:$PATH' > /etc/profile.d/pytorch.sh
COPY helpers/torch_smoke.py /usr/local/bin/torch-smoke.py
COPY helpers/collective_probe.py /usr/local/bin/collective-probe.py
