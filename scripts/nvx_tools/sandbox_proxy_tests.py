"""Public CLI acceptance for proxy-only sandbox networking.

The scenario provisions a sandbox through `nvx-rs sandbox` with a proxy-only
network: egress, ingress, and host-loopback access are denied, and a proxy on
host loopback, which the guest reaches at its gateway, is the guest's only way
out. A managed exec request in the Ubuntu workload layer then requests a URL
through a test proxy on host loopback, which answers it, and tries other
destinations, which host listeners show never arrive: another port of the
gateway, the proxy's port over UDP, at the host's own address, and over IPv6.
The same probes first run in a sandbox whose network lets them all through,
which shows that they work. The scenario also checks that the planner refuses
a proxy beside allowed egress, and one at an address other than the gateway,
before anything launches, and that OpenVMM reports the policy that it applied.
"""

from __future__ import annotations

import ipaddress
import random
import shutil
import socket
import subprocess
import tempfile
import threading
from collections.abc import Generator
from contextlib import ExitStack, contextmanager, suppress
from dataclasses import dataclass
from pathlib import Path

from .managed_exec_tests import bounded_text, load_workload_artifacts
from .sandbox_lifecycle_tests import (
    LOG,
    OUTCOME,
    PROVISIONED_ENTRIES,
    STOPPED_ENTRIES,
    LifecycleAcceptance,
    Refusal,
    check_managed_report,
    managed_report,
    run_acceptance,
)

GUEST_NETWORK = "192.0.2.2/24"
GATEWAY = "192.0.2.1"
# The portable profile embeds the NIC's IPv4 addresses in fd00::/96.
GATEWAY_IPV6 = "fd00::c000:201"
# An address that is not the gateway, for the refused provision.
OTHER_ADDRESS = "198.51.100.1"
PROXY_TARGET = "http://nvx-proxy-only.invalid/probe"
PROXY_REQUEST_LINE = f"GET {PROXY_TARGET} HTTP/1.".encode()
PROXY_RESPONSE = "NVX-PROXY-ONLY-RESPONSE"
REQUEST_LIMIT = 4096
# The dynamic port range, from which the proxy's port is drawn.
PORT_RANGE = (49152, 65535)
PORT_ATTEMPTS = 16
# How long a host listener waits for a connection that must not arrive.
NEGATIVE_OBSERVATION_SECONDS = 0.25
# The options of the open network, which lets the guest reach every probe's
# destination: egress allowed, and host-loopback access in both directions.
OPEN_OPTIONS = [
    "--net",
    GUEST_NETWORK,
    "--network-profile",
    "portable",
    "--network-egress",
    "allow",
    "--host-loopback",
    "allow",
]
# OpenVMM's report of the open network's policy.
OPEN_NETWORK_POLICY: dict[str, object] = {
    "status": "applied",
    "status_code": 0,
    "mode": "allow-all",
    "allow_rule_count": 0,
    "deny_rule_count": 0,
    "host_loopback": "allow",
}
# OpenVMM's report of the network policy that a proxy-only network lowers to:
# egress denied without rules, and host-loopback access denied.
NETWORK_POLICY: dict[str, object] = {
    "status": "applied",
    "status_code": 0,
    "mode": "deny-all",
    "allow_rule_count": 0,
    "deny_rule_count": 0,
    "host_loopback": "deny",
}
# The guest's probes, which run as `sh -c PROBE_SCRIPT probe PROXY TARGET
# [PROTOCOL NAME HOST PORT]...`. The script requests TARGET through the proxy
# at PROXY and prints `proxy STATUS RESPONSE`, then tries each destination
# with nc, sending a UDP probe's name as its datagram, and prints
# `NAME STATUS`.
PROBE_SCRIPT = """\
response=$(http_proxy=$1 wget -T 5 -qO- "$2" 2>&1)
echo "proxy $? $response"
shift 2
while [ "$#" -ge 4 ]; do
    case $1 in
    tcp) nc -z -w 2 "$3" "$4" </dev/null >/dev/null 2>&1 ;;
    udp) printf '%s\\n' "$2" | nc -u -w 1 "$3" "$4" >/dev/null 2>&1 ;;
    *) exit 2 ;;
    esac
    echo "$2 $?"
    shift 4
done
"""


@dataclass(frozen=True)
class Probe:
    """A destination other than the proxy, and the host listener at which the
    guest's traffic to it arrives if the network lets it through."""

    protocol: str
    name: str
    host: str
    port: int
    listener: socket.socket

    def arguments(self) -> list[str]:
        return [
            f"--arg={value}"
            for value in (self.protocol, self.name, self.host, str(self.port))
        ]


@dataclass(frozen=True)
class HostEndpoints:
    """The test proxy's listener on host loopback, at `port`, and the probes."""

    port: int
    proxy: socket.socket
    probes: tuple[Probe, ...]


def host_address() -> str | None:
    """Returns the host's own address on its default IPv4 route, if it has one."""
    with suppress(OSError), socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as route:
        # Connecting a UDP socket sends nothing; it selects the source address
        # of the route.
        route.connect((OTHER_ADDRESS, 9))
        address = str(route.getsockname()[0])
        if not ipaddress.ip_address(address).is_loopback:
            return address
    return None


def ipv6_loopback() -> bool:
    """Returns whether the host can listen on IPv6 loopback."""
    if not socket.has_ipv6:
        return False
    try:
        with socket.socket(socket.AF_INET6, socket.SOCK_STREAM) as probe:
            probe.bind(("::1", 0))
    except OSError:
        return False
    return True


def listen(
    sockets: ExitStack, family: int, kind: int, address: tuple[str, int]
) -> socket.socket:
    """Binds a socket of `family` and `kind`, which `sockets` closes, to
    `address`, and listens on it if it is a TCP socket."""
    endpoint = sockets.enter_context(socket.socket(family, kind))
    if family == socket.AF_INET6:
        endpoint.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
    endpoint.bind(address)
    if kind == socket.SOCK_STREAM:
        endpoint.listen(1)
    return endpoint


@contextmanager
def host_endpoints() -> Generator[HostEndpoints]:
    """Binds the test proxy's TCP listener on host loopback, and at the same
    port a UDP listener on host loopback, a TCP listener at the host's own
    address, and one on IPv6 loopback, where the host has them; and a TCP
    listener at another port of host loopback."""
    address = host_address()
    ipv6 = ipv6_loopback()
    first, last = PORT_RANGE
    refused: list[str] = []
    for port in random.SystemRandom().sample(range(first, last + 1), PORT_ATTEMPTS):
        with ExitStack() as sockets:
            try:
                proxy = listen(
                    sockets, socket.AF_INET, socket.SOCK_STREAM, ("127.0.0.1", port)
                )
                udp = listen(
                    sockets, socket.AF_INET, socket.SOCK_DGRAM, ("127.0.0.1", port)
                )
                probes = [Probe("udp", "gateway-udp", GATEWAY, port, udp)]
                if address is not None:
                    at_address = listen(
                        sockets, socket.AF_INET, socket.SOCK_STREAM, (address, port)
                    )
                    probes.append(
                        Probe("tcp", "host-address", address, port, at_address)
                    )
                if ipv6:
                    at_ipv6 = listen(
                        sockets, socket.AF_INET6, socket.SOCK_STREAM, ("::1", port)
                    )
                    probes.append(
                        Probe("tcp", "gateway-ipv6", GATEWAY_IPV6, port, at_ipv6)
                    )
            except OSError as error:
                refused.append(f"{port}: {error}")
                continue
            other = listen(
                sockets, socket.AF_INET, socket.SOCK_STREAM, ("127.0.0.1", 0)
            )
            other_port = int(other.getsockname()[1])
            probes.insert(0, Probe("tcp", "gateway-port", GATEWAY, other_port, other))
            yield HostEndpoints(port, proxy, tuple(probes))
            return
    raise RuntimeError(
        f"failed to bind the test proxy's ports after {PORT_ATTEMPTS} attempts: "
        + "; ".join(refused)
    )


def read_request_head(connection: socket.socket) -> bytes:
    head = bytearray()
    while b"\r\n\r\n" not in head:
        chunk = connection.recv(1024)
        if not chunk:
            break
        head.extend(chunk)
        if len(head) > REQUEST_LIMIT:
            raise RuntimeError("the proxy request exceeds its size limit")
    return bytes(head)


class ProxyServer:
    """Answers each HTTP request on its listener itself, until it stops."""

    def __init__(self, listener: socket.socket, timeout: float) -> None:
        self.listener = listener
        self.timeout = timeout
        self.request_lines: list[bytes] = []
        self.errors: list[Exception] = []
        self.stopping = threading.Event()
        self.thread = threading.Thread(
            target=self.serve, name="nvx-test-proxy", daemon=True
        )

    def __enter__(self) -> ProxyServer:
        self.thread.start()
        return self

    def __exit__(self, *_: object) -> None:
        self.stopping.set()
        self.thread.join(self.timeout)

    def serve(self) -> None:
        self.listener.settimeout(0.05)
        body = PROXY_RESPONSE.encode()
        response = (
            b"HTTP/1.1 200 OK\r\nContent-Length: "
            + str(len(body)).encode()
            + b"\r\nConnection: close\r\n\r\n"
            + body
        )
        while not self.stopping.is_set():
            try:
                connection, _ = self.listener.accept()
            except TimeoutError:
                continue
            except OSError as error:
                self.errors.append(error)
                return
            with connection:
                try:
                    connection.settimeout(self.timeout)
                    head = read_request_head(connection)
                    self.request_lines.append(head.partition(b"\r\n")[0])
                    connection.sendall(response)
                except Exception as error:
                    self.errors.append(error)


def proxy_options(
    port: int, *, egress: str = "deny", address: str = GATEWAY
) -> list[str]:
    """Returns the options of a network with `egress` whose proxy listens on
    host loopback at `port`, and which the guest reaches at `address`: a
    proxy-only network, with the defaults."""
    return [
        "--net",
        GUEST_NETWORK,
        "--network-profile",
        "portable",
        "--network-egress",
        egress,
        "--host-loopback",
        "deny",
        "--network-proxy",
        f"{address}:{port}",
    ]


class ProxyOnlyAcceptance(LifecycleAcceptance):
    evidence_prefix = "sandbox-proxy-only-"

    def run(self) -> None:
        with host_endpoints() as endpoints:
            self.check_refusals(endpoints.port)
            # The open network lets every probe through, so that their failure
            # in the proxy-only network shows its policy, not a broken probe.
            self.probe_network(
                endpoints, "open", OPEN_OPTIONS, OPEN_NETWORK_POLICY, reached=True
            )
            self.probe_network(
                endpoints,
                "proxy-only",
                proxy_options(endpoints.port),
                NETWORK_POLICY,
                reached=False,
            )

    def check_refusals(self, port: int) -> None:
        """The planner refuses a proxy beside allowed egress, and one at an
        address other than the gateway, before it creates any state."""
        shutil.copyfile(self.artifacts.scratch_template, self.scratch)
        for options, refusal in (
            (proxy_options(port, egress="allow"), Refusal("unsupported")),
            (
                proxy_options(port, address=OTHER_ADDRESS),
                Refusal(
                    "unsupported",
                    ("--network-proxy", f"{OTHER_ADDRESS}:{port}", GATEWAY),
                ),
            ),
        ):
            self.invoke(
                "provision",
                *self.launch_options(self.scratch),
                *options,
                state=self.state,
                expected=1,
                refusal=refusal,
            )
            if self.state.exists():
                raise RuntimeError("a refused provision created the sandbox state")
        self.require_processes((), "after the refused provisions")

    def probe_network(
        self,
        endpoints: HostEndpoints,
        label: str,
        options: list[str],
        policy: dict[str, object],
        *,
        reached: bool,
    ) -> None:
        """Runs the probes in a sandbox with the network of `options`, in which
        they reach their destinations if `reached`, and which OpenVMM reports
        as `policy`; `label` names its evidence."""
        shutil.copyfile(self.artifacts.scratch_template, self.scratch)
        self.invoke(
            "provision", *self.launch_options(self.scratch), *options, state=self.state
        )
        self.require_entries(
            self.state, PROVISIONED_ENTRIES, f"after the {label} provision"
        )
        self.start(description=f"{label} start")
        with ProxyServer(endpoints.proxy, self.timeout) as proxy:
            result = self.exec(
                "/bin/sh",
                "--arg=-c",
                f"--arg={PROBE_SCRIPT}",
                "--arg=probe",
                f"--arg=http://{GATEWAY}:{endpoints.port}",
                f"--arg={PROXY_TARGET}",
                *(
                    argument
                    for probe in endpoints.probes
                    for argument in probe.arguments()
                ),
                expected=None,
            )
        self.save_evidence(f"{label}-probe.log", result.stdout + result.stderr)
        self.save_evidence(
            f"{label}-proxy.log",
            b"".join(line + b"\n" for line in proxy.request_lines),
        )
        self.check_probes(label, result, endpoints.probes, reached=reached)
        self.check_proxy(label, proxy)
        self.check_listeners(label, endpoints.probes, reached=reached)
        self.invoke("stop", state=self.state)
        self.require_entries(self.state, STOPPED_ENTRIES, f"after the {label} stop")
        self.require_processes((), f"after the {label} stop")
        self.copy_evidence(self.state / OUTCOME, f"{label}-outcome.json")
        check_managed_report(managed_report(self.state), self.backend, policy)
        self.copy_evidence(self.state / LOG, f"{label}-openvmm.log")
        self.invoke("deprovision", state=self.state)
        if self.state.exists():
            raise RuntimeError(f"the {label} deprovision kept the sandbox state")

    def check_probes(
        self,
        label: str,
        result: subprocess.CompletedProcess[bytes],
        probes: tuple[Probe, ...],
        *,
        reached: bool,
    ) -> None:
        """The guest reached the proxy at its gateway, and each probe connected
        if `reached`, or else failed to."""
        lines = result.stdout.decode(errors="replace").splitlines()
        expected = [f"proxy 0 {PROXY_RESPONSE}"]
        if result.returncode != 0 or result.stderr or lines[:1] != expected:
            raise RuntimeError(
                f"the guest of the {label} network did not reach the proxy at its "
                f"gateway: status {result.returncode}, "
                f"stdout={bounded_text(result.stdout)!r}, "
                f"stderr={bounded_text(result.stderr)!r}"
            )
        reported = lines[1:]
        if len(reported) != len(probes):
            raise RuntimeError(
                f"the guest of the {label} network reported {len(reported)} probes, "
                f"expected {len(probes)}: {bounded_text(result.stdout)!r}"
            )
        for probe, line in zip(probes, reported, strict=True):
            name, _, status = line.partition(" ")
            if name != probe.name or not status.isdigit():
                raise RuntimeError(
                    f"the guest of the {label} network reported {line!r} for the "
                    f"{probe.name} probe"
                )
            # nc exits with 1 when it cannot connect, and with another status
            # when it cannot run. A UDP sender cannot see a lost datagram, and
            # may see an unrelated error, so only the host tells whether a
            # datagram arrived.
            if probe.protocol == "udp":
                statuses = {0, 1}
            else:
                statuses = {0} if reached else {1}
            if int(status) not in statuses:
                raise RuntimeError(
                    f"the {probe.name} probe to {probe.host} port {probe.port} "
                    f"returned status {status} in the {label} network"
                )

    def check_proxy(self, label: str, proxy: ProxyServer) -> None:
        """The test proxy received the guest's one request, for its target."""
        if proxy.thread.is_alive():
            raise RuntimeError("the test proxy did not stop")
        if proxy.errors:
            raise RuntimeError(
                f"the test proxy failed in the {label} network: {proxy.errors[0]}"
            ) from proxy.errors[0]
        if len(proxy.request_lines) != 1 or not proxy.request_lines[0].startswith(
            PROXY_REQUEST_LINE
        ):
            raise RuntimeError(
                f"the test proxy received {proxy.request_lines!r} in the {label} "
                f"network, expected one {PROXY_REQUEST_LINE.decode()!r} request"
            )

    def check_listeners(
        self, label: str, probes: tuple[Probe, ...], *, reached: bool
    ) -> None:
        """Each probe's traffic arrived at its host listener if `reached`, and
        none did otherwise."""
        for probe in probes:
            probe.listener.settimeout(
                self.timeout if reached else NEGATIVE_OBSERVATION_SECONDS
            )
            try:
                if probe.protocol == "udp":
                    received = probe.listener.recv(4096)
                else:
                    connection, _ = probe.listener.accept()
                    connection.close()
                    received = b""
            except TimeoutError:
                if reached:
                    raise RuntimeError(
                        f"the {probe.name} probe did not reach the host at port "
                        f"{probe.port} in the {label} network"
                    ) from None
                continue
            if not reached:
                raise RuntimeError(
                    f"the {probe.name} probe reached the host at port {probe.port} "
                    f"in the {label} network: {bounded_text(received)!r}"
                )
            if probe.protocol == "udp" and received != f"{probe.name}\n".encode():
                raise RuntimeError(
                    f"the {probe.name} probe sent {bounded_text(received)!r}"
                )


def run_sandbox_proxy_only(
    backend: str, *, timeout: float, output_dir: Path, architecture: str = "x86_64"
) -> None:
    artifacts = load_workload_artifacts(architecture=architecture)
    output_dir.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="nvx-public-proxy-"))
    run_acceptance(
        ProxyOnlyAcceptance(
            backend,
            timeout=timeout,
            output_dir=output_dir,
            artifacts=artifacts,
            root=root,
            architecture=architecture,
        )
    )
