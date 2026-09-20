FROM sha256:e25716b6504a8515f72bcdae7c853e2471719c771c147c83df474ef47efb1f46

USER root
COPY specula /opt/specula-native
COPY runtime /opt/native-runtime
ENV SPECULA_ROOT="/opt/specula-native" \
    PYTHONDONTWRITEBYTECODE="1" \
    TMPDIR="/work/scratch"
RUN /usr/bin/python3 -c "from pathlib import Path; r=Path('/sys/fs/cgroup'); assert (r/'memory.max').read_text().strip()==str(26*1024**3); assert (r/'memory.swap.max').read_text().strip()=='0'; q,p=map(int,(r/'cpu.max').read_text().split()); assert q==6*p; print('Build cgroup verified: 26 GiB, no swap, six CPUs')" \
    && mkdir -p /work/scratch /opt/specula-native/lib /opt/native-runtime/libexec \
    && cp /opt/specula/lib/*.jar /opt/specula-native/lib/ \
    && cp -a /opt/specula/tools/cfa/target /opt/specula-native/tools/cfa/target \
    && /opt/venv/bin/pip install --no-cache-dir -e /opt/specula-native \
       -r /opt/specula-native/tools/context_control/requirements.txt \
       -r /opt/specula-native/tools/spec_analyzer/requirements.txt \
       -r /opt/specula-native/tools/trace_debugger/requirements.txt \
    && for tool in trace_debugger spec_analyzer inv_checking_tool tlc_tools context_control; do \
         ln -s /opt/venv /opt/specula-native/tools/$tool/.venv; \
       done \
    && /opt/venv/bin/pip check \
    && /opt/venv/bin/pip freeze > /opt/native-runtime/python-packages.txt \
    && mv /usr/bin/git /opt/native-runtime/libexec/git \
    && rm /usr/bin/gh /usr/local/bin/copilot \
    && ln -s /opt/native-runtime/git_guard.py /usr/bin/git \
    && ln -s /opt/native-runtime/deny_gh.py /usr/bin/gh \
    && ln -s /opt/native-runtime/copilot_wrapper.py /usr/local/bin/copilot \
    && chmod 0755 /opt/native-runtime/*.py \
    && /opt/venv/bin/python -c "import mcp, jsonschema, copilot, specula; from copilot import CopilotClient, RuntimeConnection; print('Required imports OK')" \
    && sha256sum /opt/specula-native/lib/*.jar > /opt/native-runtime/jars.sha256

COPY protoc /opt/native-runtime/protoc
RUN echo '507a3f321ae7df61982468712aa78531842ea88e3a3f6934bb9dcbd47881b921  /opt/native-runtime/protoc/bin/protoc' \
     | sha256sum --check --strict \
   && /opt/native-runtime/protoc/bin/protoc --version

LABEL io.specula.commit="088049c5b3474340213cded2664cdb674bff1e1a" \
      io.verification.policy="native-local-no-remote-publication"
ENV PATH="/usr/local/bin:/opt/venv/bin:/opt/cargo-tools/bin:/opt/rust-bin:/usr/local/sbin:/usr/sbin:/usr/bin:/sbin:/bin"
ENV PROTOC="/opt/native-runtime/protoc/bin/protoc" \
    PROTOC_INCLUDE="/opt/native-runtime/protoc/include"
USER 1001:1003
WORKDIR /work
ENTRYPOINT ["/opt/venv/bin/python", "/opt/native-runtime/entrypoint.py"]
