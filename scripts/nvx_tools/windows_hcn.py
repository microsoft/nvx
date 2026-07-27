"""Windows Host Compute Network endpoint lifecycle."""

from __future__ import annotations

import ctypes
import ipaddress
import json
import os
import socket
import time
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Iterable, Mapping, Sequence

from .common import ScriptError


HCN_QUERY = '{"SchemaVersion":{"Major":2,"Minor":0},"Flags":1}'
HCN_NETWORK_NOT_FOUND = 0x803B0001
HCN_ENDPOINT_NOT_FOUND = 0x803B0002
HCN_ENDPOINT_NOT_ATTACHED = 0x803B0034
WIN32_NOT_FOUND = 0x80070490


class _Guid(ctypes.Structure):
    _fields_ = [
        ("data1", ctypes.c_uint32),
        ("data2", ctypes.c_uint16),
        ("data3", ctypes.c_uint16),
        ("data4", ctypes.c_ubyte * 8),
    ]

    @classmethod
    def from_uuid(cls, value: uuid.UUID) -> "_Guid":
        return cls.from_buffer_copy(value.bytes_le)


class _AdapterAddresses(ctypes.Structure):
    pass


_AdapterAddressesPointer = ctypes.POINTER(_AdapterAddresses)
_AdapterAddresses._fields_ = [
    ("length", ctypes.c_ulong),
    ("interface_index", ctypes.c_ulong),
    ("next", _AdapterAddressesPointer),
    ("adapter_name", ctypes.c_char_p),
    ("first_unicast_address", ctypes.c_void_p),
    ("first_anycast_address", ctypes.c_void_p),
    ("first_multicast_address", ctypes.c_void_p),
    ("first_dns_server_address", ctypes.c_void_p),
    ("dns_suffix", ctypes.c_wchar_p),
    ("description", ctypes.c_wchar_p),
    ("friendly_name", ctypes.c_wchar_p),
    ("physical_address", ctypes.c_ubyte * 8),
    ("physical_address_length", ctypes.c_ulong),
    ("flags", ctypes.c_ulong),
    ("mtu", ctypes.c_ulong),
    ("interface_type", ctypes.c_ulong),
    ("oper_status", ctypes.c_int),
    ("ipv6_interface_index", ctypes.c_ulong),
    ("zone_indices", ctypes.c_ulong * 16),
]


@dataclass(frozen=True)
class HcnEndpointConfig:
    guest_address: str = "192.168.240.2"
    prefix_length: int = 24
    gateway: str = "192.168.240.1"
    dns_servers: tuple[str, ...] = ("1.1.1.1",)
    mac_address: str | None = None
    attach_to_host: bool = False
    owner: str = "nvx-external"


def validate_hcn_topology(config: HcnEndpointConfig) -> ipaddress.IPv4Network:
    if not 1 <= config.prefix_length <= 30:
        raise ScriptError("prefix length must be in 1..=30")
    try:
        guest = ipaddress.IPv4Address(config.guest_address)
    except ipaddress.AddressValueError as error:
        raise ScriptError(
            f"guest address must be IPv4, got {config.guest_address!r}"
        ) from error
    try:
        gateway = ipaddress.IPv4Address(config.gateway)
    except ipaddress.AddressValueError as error:
        raise ScriptError(f"gateway must be IPv4, got {config.gateway!r}") from error
    if not config.dns_servers:
        raise ScriptError("at least one DNS server is required")
    for value in config.dns_servers:
        try:
            ipaddress.IPv4Address(value)
        except ipaddress.AddressValueError as error:
            raise ScriptError(f"DNS server must be IPv4, got {value!r}") from error

    network = ipaddress.IPv4Network((guest, config.prefix_length), strict=False)
    expected_gateway = network.network_address + 1
    if gateway != expected_gateway:
        raise ScriptError(
            f"gateway must be the first usable address {expected_gateway} in {network}"
        )
    if guest in (network.network_address, network.broadcast_address, gateway):
        raise ScriptError(f"guest address {guest} is not usable in {network}")
    return network


def normalize_mac(value: str) -> str:
    parts = value.replace(":", "-").split("-")
    if len(parts) != 6 or any(
        len(part) != 2 or any(character not in "0123456789abcdefABCDEF" for character in part)
        for part in parts
    ):
        raise ScriptError(
            f"MAC address must contain six hexadecimal octets, got {value!r}"
        )
    return "-".join(part.upper() for part in parts)


def generate_hcn_mac() -> str:
    random = uuid.uuid4().bytes
    return f"00-15-5D-52-{0xC0 + (random[0] & 0x0F):02X}-{random[1]:02X}"


class HcnApi:
    """Small ctypes wrapper around the HCN v2 API."""

    def __init__(self) -> None:
        if os.name != "nt":
            raise ScriptError("HCN endpoint management requires Windows")
        self.hcn = ctypes.WinDLL("computenetwork.dll")
        self.ip_helper = ctypes.WinDLL("iphlpapi.dll")
        self.ole32 = ctypes.WinDLL("ole32.dll")
        self._bind()

    def _bind(self) -> None:
        pointer = ctypes.c_void_p
        guid_pointer = ctypes.POINTER(_Guid)
        pointer_pointer = ctypes.POINTER(pointer)
        query_args = [pointer, ctypes.c_wchar_p, pointer_pointer, pointer_pointer]
        open_args = [guid_pointer, pointer_pointer, pointer_pointer]

        self.hcn.HcnCreateNetwork.argtypes = [
            guid_pointer,
            ctypes.c_wchar_p,
            pointer_pointer,
            pointer_pointer,
        ]
        self.hcn.HcnCreateNetwork.restype = ctypes.c_long
        self.hcn.HcnCloseNetwork.argtypes = [pointer]
        self.hcn.HcnCloseNetwork.restype = ctypes.c_long
        self.hcn.HcnEnumerateNetworks.argtypes = [
            ctypes.c_wchar_p,
            pointer_pointer,
            pointer_pointer,
        ]
        self.hcn.HcnEnumerateNetworks.restype = ctypes.c_long
        self.hcn.HcnOpenNetwork.argtypes = open_args
        self.hcn.HcnOpenNetwork.restype = ctypes.c_long
        self.hcn.HcnQueryNetworkProperties.argtypes = query_args
        self.hcn.HcnQueryNetworkProperties.restype = ctypes.c_long
        self.hcn.HcnDeleteNetwork.argtypes = [guid_pointer, pointer_pointer]
        self.hcn.HcnDeleteNetwork.restype = ctypes.c_long

        self.hcn.HcnCreateEndpoint.argtypes = [
            pointer,
            guid_pointer,
            ctypes.c_wchar_p,
            pointer_pointer,
            pointer_pointer,
        ]
        self.hcn.HcnCreateEndpoint.restype = ctypes.c_long
        self.hcn.HcnOpenEndpoint.argtypes = open_args
        self.hcn.HcnOpenEndpoint.restype = ctypes.c_long
        self.hcn.HcnQueryEndpointProperties.argtypes = query_args
        self.hcn.HcnQueryEndpointProperties.restype = ctypes.c_long
        self.hcn.HcnCloseEndpoint.argtypes = [pointer]
        self.hcn.HcnCloseEndpoint.restype = ctypes.c_long
        self.hcn.HcnDeleteEndpoint.argtypes = [guid_pointer, pointer_pointer]
        self.hcn.HcnDeleteEndpoint.restype = ctypes.c_long

        self.hcn.HcnEnumerateNamespaces.argtypes = [
            ctypes.c_wchar_p,
            pointer_pointer,
            pointer_pointer,
        ]
        self.hcn.HcnEnumerateNamespaces.restype = ctypes.c_long
        self.hcn.HcnOpenNamespace.argtypes = open_args
        self.hcn.HcnOpenNamespace.restype = ctypes.c_long
        self.hcn.HcnQueryNamespaceProperties.argtypes = query_args
        self.hcn.HcnQueryNamespaceProperties.restype = ctypes.c_long
        self.hcn.HcnModifyNamespace.argtypes = [
            pointer,
            ctypes.c_wchar_p,
            pointer_pointer,
        ]
        self.hcn.HcnModifyNamespace.restype = ctypes.c_long
        self.hcn.HcnCloseNamespace.argtypes = [pointer]
        self.hcn.HcnCloseNamespace.restype = ctypes.c_long

        self.ip_helper.ConvertInterfaceIndexToLuid.argtypes = [
            ctypes.c_ulong,
            ctypes.POINTER(ctypes.c_uint64),
        ]
        self.ip_helper.ConvertInterfaceIndexToLuid.restype = ctypes.c_ulong
        self.ip_helper.GetAdaptersAddresses.argtypes = [
            ctypes.c_ulong,
            ctypes.c_ulong,
            ctypes.c_void_p,
            _AdapterAddressesPointer,
            ctypes.POINTER(ctypes.c_ulong),
        ]
        self.ip_helper.GetAdaptersAddresses.restype = ctypes.c_ulong
        self.ip_helper.SendARP.argtypes = [
            ctypes.c_ulong,
            ctypes.c_ulong,
            ctypes.c_void_p,
            ctypes.POINTER(ctypes.c_ulong),
        ]
        self.ip_helper.SendARP.restype = ctypes.c_ulong
        self.ole32.CoTaskMemFree.argtypes = [ctypes.c_void_p]

    def _take_string(self, pointer: ctypes.c_void_p) -> str:
        if not pointer.value:
            return ""
        try:
            return ctypes.wstring_at(pointer.value)
        finally:
            self.ole32.CoTaskMemFree(pointer)

    def _complete(
        self,
        status: int,
        error_record: ctypes.c_void_p,
        operation: str,
        ignored: Iterable[int] = (),
    ) -> None:
        record = self._take_string(error_record)
        code = ctypes.c_uint32(status).value
        if code & 0x80000000 and code not in ignored:
            raise ScriptError(
                f"{operation} failed with HRESULT 0x{code:08X}; "
                f"error record: {record or '<none>'}"
            )

    def _enumerate(self, function: Any, operation: str) -> list[str]:
        values = ctypes.c_void_p()
        error = ctypes.c_void_p()
        status = function(HCN_QUERY, ctypes.byref(values), ctypes.byref(error))
        self._complete(status, error, operation)
        text = self._take_string(values)
        return list(json.loads(text or "[]"))

    def _query(self, function: Any, handle: ctypes.c_void_p, operation: str) -> dict[str, Any]:
        properties = ctypes.c_void_p()
        error = ctypes.c_void_p()
        status = function(
            handle, HCN_QUERY, ctypes.byref(properties), ctypes.byref(error)
        )
        self._complete(status, error, operation)
        return dict(json.loads(self._take_string(properties)))

    def _open_query(
        self,
        identifier: uuid.UUID,
        open_function: Any,
        query_function: Any,
        close_function: Any,
        kind: str,
    ) -> dict[str, Any]:
        guid = _Guid.from_uuid(identifier)
        handle = ctypes.c_void_p()
        error = ctypes.c_void_p()
        status = open_function(ctypes.byref(guid), ctypes.byref(handle), ctypes.byref(error))
        self._complete(status, error, f"HcnOpen{kind}({identifier})")
        try:
            return self._query(
                query_function, handle, f"HcnQuery{kind}Properties({identifier})"
            )
        finally:
            self._close(close_function, handle, kind)

    def _close(self, function: Any, handle: ctypes.c_void_p | None, kind: str) -> None:
        if handle is None or not handle.value:
            return
        self._complete(function(handle), ctypes.c_void_p(), f"HcnClose{kind}")

    def enumerate_networks(self) -> list[str]:
        return self._enumerate(self.hcn.HcnEnumerateNetworks, "HcnEnumerateNetworks")

    def network_properties(self, identifier: uuid.UUID) -> dict[str, Any]:
        return self._open_query(
            identifier,
            self.hcn.HcnOpenNetwork,
            self.hcn.HcnQueryNetworkProperties,
            self.hcn.HcnCloseNetwork,
            "Network",
        )

    def create_network(self, identifier: uuid.UUID, document: Mapping[str, Any]) -> ctypes.c_void_p:
        guid = _Guid.from_uuid(identifier)
        handle = ctypes.c_void_p()
        error = ctypes.c_void_p()
        status = self.hcn.HcnCreateNetwork(
            ctypes.byref(guid),
            json.dumps(document, separators=(",", ":")),
            ctypes.byref(handle),
            ctypes.byref(error),
        )
        self._complete(status, error, f"HcnCreateNetwork({identifier})")
        return handle

    def close_network(self, handle: ctypes.c_void_p | None) -> None:
        self._close(self.hcn.HcnCloseNetwork, handle, "Network")

    def delete_network(self, identifier: uuid.UUID) -> None:
        guid = _Guid.from_uuid(identifier)
        error = ctypes.c_void_p()
        status = self.hcn.HcnDeleteNetwork(ctypes.byref(guid), ctypes.byref(error))
        self._complete(
            status,
            error,
            f"HcnDeleteNetwork({identifier})",
            (HCN_NETWORK_NOT_FOUND,),
        )

    def create_endpoint(
        self,
        network: ctypes.c_void_p,
        identifier: uuid.UUID,
        document: Mapping[str, Any],
    ) -> ctypes.c_void_p:
        guid = _Guid.from_uuid(identifier)
        handle = ctypes.c_void_p()
        error = ctypes.c_void_p()
        status = self.hcn.HcnCreateEndpoint(
            network,
            ctypes.byref(guid),
            json.dumps(document, separators=(",", ":")),
            ctypes.byref(handle),
            ctypes.byref(error),
        )
        self._complete(status, error, f"HcnCreateEndpoint({identifier})")
        return handle

    def endpoint_properties(self, handle: ctypes.c_void_p, identifier: uuid.UUID) -> dict[str, Any]:
        return self._query(
            self.hcn.HcnQueryEndpointProperties,
            handle,
            f"HcnQueryEndpointProperties({identifier})",
        )

    def close_endpoint(self, handle: ctypes.c_void_p | None) -> None:
        self._close(self.hcn.HcnCloseEndpoint, handle, "Endpoint")

    def delete_endpoint(self, identifier: uuid.UUID) -> None:
        guid = _Guid.from_uuid(identifier)
        error = ctypes.c_void_p()
        status = self.hcn.HcnDeleteEndpoint(ctypes.byref(guid), ctypes.byref(error))
        self._complete(
            status,
            error,
            f"HcnDeleteEndpoint({identifier})",
            (HCN_ENDPOINT_NOT_FOUND,),
        )

    def host_default_namespace(self) -> uuid.UUID:
        for value in self._enumerate(
            self.hcn.HcnEnumerateNamespaces, "HcnEnumerateNamespaces"
        ):
            identifier = uuid.UUID(value)
            properties = self._open_query(
                identifier,
                self.hcn.HcnOpenNamespace,
                self.hcn.HcnQueryNamespaceProperties,
                self.hcn.HcnCloseNamespace,
                "Namespace",
            )
            if properties.get("Type") == "HostDefault" or properties.get("IsDefault"):
                return identifier
        raise ScriptError("HCN did not report a HostDefault namespace")

    def modify_namespace(
        self,
        namespace: uuid.UUID,
        endpoint: uuid.UUID,
        request: str,
        *,
        ignore_missing: bool = False,
    ) -> None:
        guid = _Guid.from_uuid(namespace)
        handle = ctypes.c_void_p()
        error = ctypes.c_void_p()
        status = self.hcn.HcnOpenNamespace(
            ctypes.byref(guid), ctypes.byref(handle), ctypes.byref(error)
        )
        code = ctypes.c_uint32(status).value
        if ignore_missing and code == WIN32_NOT_FOUND:
            self._take_string(error)
            return
        self._complete(status, error, f"HcnOpenNamespace({namespace})")
        try:
            settings = json.dumps(
                {
                    "ResourceType": "Endpoint",
                    "RequestType": request,
                    "Settings": {"EndpointId": str(endpoint)},
                },
                separators=(",", ":"),
            )
            error = ctypes.c_void_p()
            status = self.hcn.HcnModifyNamespace(
                handle, settings, ctypes.byref(error)
            )
            ignored = (
                (HCN_ENDPOINT_NOT_FOUND, HCN_ENDPOINT_NOT_ATTACHED)
                if ignore_missing and request == "Remove"
                else ()
            )
            self._complete(
                status, error, f"HcnModifyNamespace({request}, {endpoint})", ignored
            )
        finally:
            self._close(self.hcn.HcnCloseNamespace, handle, "Namespace")

    def find_interface(self, mac_address: str, endpoint_name: str) -> tuple[int, int, str]:
        size = ctypes.c_ulong(15 * 1024)
        while True:
            buffer = ctypes.create_string_buffer(size.value)
            first = ctypes.cast(buffer, _AdapterAddressesPointer)
            status = self.ip_helper.GetAdaptersAddresses(
                socket.AF_UNSPEC,
                0x100,
                None,
                first,
                ctypes.byref(size),
            )
            if status != 111:
                break
        if status != 0:
            raise ScriptError(f"GetAdaptersAddresses failed with Win32 status {status}")

        expected_mac = bytes.fromhex(normalize_mac(mac_address).replace("-", ""))
        expected_alias = f"vEthernet ({endpoint_name})"
        matches: list[tuple[int, str]] = []
        current = first
        while current:
            row = current.contents
            observed = bytes(row.physical_address[: row.physical_address_length])
            alias = row.friendly_name or ""
            if row.interface_index and observed == expected_mac and alias == expected_alias:
                matches.append((int(row.interface_index), alias))
            current = row.next
        if len(matches) > 1:
            raise ScriptError(
                f"multiple host interfaces expose {expected_alias!r} with MAC {mac_address}"
            )
        if not matches:
            raise LookupError(expected_alias)
        interface_index, alias = matches[0]
        luid = ctypes.c_uint64()
        status = self.ip_helper.ConvertInterfaceIndexToLuid(
            interface_index, ctypes.byref(luid)
        )
        if status != 0:
            raise ScriptError(
                f"ConvertInterfaceIndexToLuid({interface_index}) failed with Win32 status {status}"
            )
        return interface_index, int(luid.value), alias

    def gateway_mac(self, gateway: str) -> str:
        destination = int.from_bytes(socket.inet_aton(gateway), byteorder="little")
        buffer = (ctypes.c_ubyte * 32)()
        length = ctypes.c_ulong(len(buffer))
        status = self.ip_helper.SendARP(
            destination, 0, ctypes.byref(buffer), ctypes.byref(length)
        )
        if status != 0 or length.value == 0:
            raise LookupError(gateway)
        return "-".join(f"{value:02X}" for value in buffer[: length.value])


def _prefix_available(api: Any, network: ipaddress.IPv4Network) -> None:
    for value in api.enumerate_networks():
        identifier = uuid.UUID(str(value))
        properties = api.network_properties(identifier)
        for ipam in properties.get("Ipams", ()):
            for subnet in ipam.get("Subnets", ()):
                text = subnet.get("IpAddressPrefix")
                if not text:
                    continue
                try:
                    existing = ipaddress.IPv4Network(text, strict=False)
                except (ipaddress.AddressValueError, ValueError):
                    continue
                if network.overlaps(existing):
                    raise ScriptError(
                        f"requested HCN prefix {network} overlaps network "
                        f"{identifier} prefix {existing}"
                    )


def wait_hcn_interface(
    api: HcnApi,
    mac_address: str,
    endpoint_name: str,
    gateway: str,
    timeout: float = 30,
) -> dict[str, Any]:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            interface_index, interface_luid, alias = api.find_interface(
                mac_address, endpoint_name
            )
            gateway_mac = api.gateway_mac(gateway)
            return {
                "interfaceIndex": interface_index,
                "interfaceLuid": interface_luid,
                "interfaceAlias": alias,
                "gatewayMac": gateway_mac,
            }
        except LookupError:
            time.sleep(0.05)
    raise ScriptError(
        f"timed out resolving host vNIC {endpoint_name!r} with MAC "
        f"{mac_address} and gateway {gateway}"
    )


def _network_document(
    config: HcnEndpointConfig,
    network: ipaddress.IPv4Network,
    identifier: uuid.UUID,
) -> dict[str, Any]:
    return {
        "SchemaVersion": {"Major": 2, "Minor": 0},
        "Owner": config.owner,
        "Name": f"nvx-{identifier}",
        "Type": "NAT",
        "MacPool": {
            "Ranges": [
                {
                    "StartMacAddress": "00-15-5D-52-C0-00",
                    "EndMacAddress": "00-15-5D-52-CF-FF",
                }
            ]
        },
        "Ipams": [
            {
                "Type": "Static",
                "Subnets": [
                    {
                        "IpAddressPrefix": str(network),
                        "Routes": [
                            {
                                "DestinationPrefix": "0.0.0.0/0",
                                "NextHop": config.gateway,
                            }
                        ],
                    }
                ],
            }
        ],
    }


def _endpoint_document(
    config: HcnEndpointConfig,
    network_identifier: uuid.UUID,
    endpoint_identifier: uuid.UUID,
    mac_address: str,
) -> dict[str, Any]:
    return {
        "SchemaVersion": {"Major": 2, "Minor": 0},
        "Owner": config.owner,
        "Name": f"nvx-{endpoint_identifier}-ep",
        "HostComputeNetwork": str(network_identifier),
        "IpConfigurations": [
            {
                "IpAddress": config.guest_address,
                "PrefixLength": config.prefix_length,
            }
        ],
        "Routes": [
            {"DestinationPrefix": "0.0.0.0/0", "NextHop": config.gateway}
        ],
        "Dns": {"ServerList": list(config.dns_servers), "Search": []},
        "MacAddress": mac_address,
    }


def create_hcn_endpoint(
    config: HcnEndpointConfig,
    *,
    api: Any | None = None,
    interface_resolver: Callable[..., dict[str, Any]] = wait_hcn_interface,
) -> dict[str, Any]:
    network = validate_hcn_topology(config)
    mac_address = normalize_mac(config.mac_address or generate_hcn_mac())
    api = api or HcnApi()
    _prefix_available(api, network)

    network_identifier = uuid.uuid4()
    endpoint_identifier = uuid.uuid4()
    adapter_identifier = uuid.uuid4()
    endpoint_name = f"nvx-{endpoint_identifier}-ep"
    network_handle = None
    endpoint_handle = None
    network_created = False
    endpoint_created = False
    namespace_identifier: uuid.UUID | None = None
    host_attached = False
    try:
        network_handle = api.create_network(
            network_identifier,
            _network_document(config, network, network_identifier),
        )
        network_created = True
        endpoint_handle = api.create_endpoint(
            network_handle,
            endpoint_identifier,
            _endpoint_document(
                config, network_identifier, endpoint_identifier, mac_address
            ),
        )
        endpoint_created = True
        properties = api.endpoint_properties(endpoint_handle, endpoint_identifier)
        observed_mac = normalize_mac(str(properties.get("MacAddress", "")))

        if config.attach_to_host:
            namespace_identifier = api.host_default_namespace()
            host_attached = True
            api.modify_namespace(
                namespace_identifier, endpoint_identifier, "Add"
            )
            interface = interface_resolver(
                api,
                observed_mac,
                endpoint_name,
                config.gateway,
            )
        else:
            interface = {
                "interfaceIndex": 0,
                "interfaceLuid": 0,
                "interfaceAlias": "",
                "gatewayMac": "",
            }

        api.close_endpoint(endpoint_handle)
        endpoint_handle = None
        api.close_network(network_handle)
        network_handle = None
        return {
            "version": 1,
            "owner": config.owner,
            "networkId": str(network_identifier),
            "endpointId": str(endpoint_identifier),
            "adapterId": str(adapter_identifier),
            "networkName": f"nvx-{network_identifier}",
            "endpointName": endpoint_name,
            "guestAddress": config.guest_address,
            "prefixLength": config.prefix_length,
            "networkAddress": str(network.network_address),
            "gateway": config.gateway,
            "dnsServers": list(config.dns_servers),
            "macAddress": observed_mac,
            "hostAttached": host_attached,
            "hostNamespaceId": str(namespace_identifier) if host_attached else "",
            **interface,
        }
    except Exception as primary:
        failures: list[str] = []
        if host_attached and namespace_identifier is not None:
            try:
                api.modify_namespace(
                    namespace_identifier,
                    endpoint_identifier,
                    "Remove",
                    ignore_missing=True,
                )
            except Exception as error:
                failures.append(f"namespace detach: {error}")
        for label, closer, handle in (
            ("endpoint handle close", api.close_endpoint, endpoint_handle),
            ("network handle close", api.close_network, network_handle),
        ):
            try:
                closer(handle)
            except Exception as error:
                failures.append(f"{label}: {error}")
        if endpoint_created:
            try:
                api.delete_endpoint(endpoint_identifier)
            except Exception as error:
                failures.append(f"endpoint delete: {error}")
        if network_created:
            try:
                api.delete_network(network_identifier)
            except Exception as error:
                failures.append(f"network delete: {error}")
        suffix = f"; rollback failures: {'; '.join(failures)}" if failures else ""
        raise ScriptError(f"{primary}{suffix}") from primary


def remove_hcn_endpoint(descriptor: Mapping[str, Any], *, api: Any | None = None) -> None:
    api = api or HcnApi()
    endpoint_identifier = uuid.UUID(str(descriptor["endpointId"]))
    network_identifier = uuid.UUID(str(descriptor["networkId"]))
    failures: list[str] = []
    if descriptor.get("hostAttached") and descriptor.get("hostNamespaceId"):
        try:
            api.modify_namespace(
                uuid.UUID(str(descriptor["hostNamespaceId"])),
                endpoint_identifier,
                "Remove",
                ignore_missing=True,
            )
        except Exception as error:
            failures.append(f"namespace detach: {error}")
    try:
        api.delete_endpoint(endpoint_identifier)
    except Exception as error:
        failures.append(f"endpoint delete: {error}")
    try:
        api.delete_network(network_identifier)
    except Exception as error:
        failures.append(f"network delete: {error}")
    if failures:
        raise ScriptError(f"external HCN cleanup failed: {'; '.join(failures)}")


def setup_hcn_endpoint(
    output_path: Path,
    config: HcnEndpointConfig,
    *,
    api: Any | None = None,
    interface_resolver: Callable[..., dict[str, Any]] = wait_hcn_interface,
) -> dict[str, Any]:
    output_path = output_path.expanduser().resolve()
    if output_path.exists():
        raise ScriptError(
            f"HCN endpoint descriptor already exists; clean it up before reuse: {output_path}"
        )
    output_path.parent.mkdir(parents=True, exist_ok=True)
    descriptor = create_hcn_endpoint(
        config, api=api, interface_resolver=interface_resolver
    )
    try:
        with output_path.open("x", encoding="utf-8", newline="\n") as output:
            json.dump(descriptor, output, indent=2)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
    except Exception as primary:
        try:
            remove_hcn_endpoint(descriptor, api=api)
        except Exception as rollback:
            raise ScriptError(
                f"{primary}; rollback failed for endpoint "
                f"{descriptor['endpointId']}: {rollback}"
            ) from primary
        raise
    return descriptor


def cleanup_hcn_endpoint(
    descriptor_path: Path,
    *,
    keep_descriptor: bool = False,
    api: Any | None = None,
) -> bool:
    if not descriptor_path.is_file():
        print(f"HCN descriptor is already absent: {descriptor_path}")
        return False
    descriptor = json.loads(descriptor_path.read_text(encoding="utf-8"))
    remove_hcn_endpoint(descriptor, api=api)
    if not keep_descriptor:
        descriptor_path.unlink()
    print(f"Removed externally managed HCN endpoint {descriptor['endpointId']}")
    return True