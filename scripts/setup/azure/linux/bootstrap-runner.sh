#!/bin/bash
set -euo pipefail
set +x

refresh_metadata_only=false
if [ "${1:-}" = --refresh-metadata ]; then
    refresh_metadata_only=true
    shift
fi
config_path=${1:-/opt/nvx-bootstrap/config.json}
if [[ "$config_path" != /* ]]; then
    config_path="$PWD/$config_path"
fi
setup_root=/opt/nvx-bootstrap
exec >>/var/log/nvx-runner-bootstrap.log 2>&1

settings=$(
    python3 - "$config_path" <<'PY'
import json,sys
with open(sys.argv[1],encoding="utf-8") as stream:
    config=json.load(stream)
if config["os"]!="linux" or config["backend"] not in ("kvm","mshv"):
    raise SystemExit("Expected a Linux KVM or MSHV configuration")
if config["adminUsername"] in ("root","nvx-runner"):
    raise SystemExit("The runner administrator must not be root or nvx-runner")
required=["linux",config["backend"],"virtual-machine"]
if [label.lower() for label in config["runnerLabels"][:3]]!=required:
    raise SystemExit("Runner labels must preserve the platform and backend labels")
additional=config["runnerLabels"][3:]
roles={label.lower() for label in additional if label.lower() in ("general","benchmark")}
if len(roles)>1:
    raise SystemExit("Runner labels must not contain both general and benchmark")
values=[
    config["backend"],config["adminUsername"],config["repository"],
    ",".join(label for label in additional if label.lower() not in ("general","benchmark")),
    config["runnerDirectory"],
    str(config["dataDisk"]["lun"]),config["dataDisk"]["mountPoint"],
    config["kernelMode"],
]
if (
    any(not isinstance(value,str) or "\n" in value or "\r" in value for value in values)
    or any(not values[index] for index in (0,1,2,4,5,6,7))
):
    raise SystemExit("Invalid bootstrap configuration values")
print("\n".join(values))
PY
)
mapfile -t values <<<"$settings"
[ "${#values[@]}" = 8 ] || {
    echo "ERROR: incomplete bootstrap configuration" >&2
    exit 1
}
backend=${values[0]}
admin_username=${values[1]}
repository=${values[2]}
extra_runner_labels=${values[3]}
runner_directory=${values[4]}
export NVX_DATA_DISK_LUN=${values[5]}
export NVX_DATA_DISK_MOUNT_POINT=${values[6]}
kernel_mode=${values[7]}
admin_uid=$(id -u "$admin_username")
[ "$admin_uid" -ne 0 ] || {
    echo "ERROR: the runner administrator must not be root" >&2
    exit 1
}

instance_settings=$(
    python3 - "$config_path" <<'PY'
import json,re,sys,urllib.request
with open(sys.argv[1],encoding="utf-8") as stream:
    config=json.load(stream)
request=urllib.request.Request(
    "http://169.254.169.254/metadata/instance/compute?api-version=2021-02-01",
    headers={"Metadata":"true"},
)
opener=urllib.request.build_opener(urllib.request.ProxyHandler({}))
with opener.open(request,timeout=30) as response:
    metadata=json.load(response)
prefix=config["vmssName"]+"_"
name=metadata["name"]
if (
    metadata["subscriptionId"].lower()!=config["subscriptionId"].lower()
    or metadata["resourceGroupName"].lower()!=config["resourceGroup"].lower()
    or metadata["vmScaleSetName"].lower()!=config["vmssName"].lower()
    or not name.lower().startswith(prefix.lower())
):
    raise SystemExit("Azure instance metadata does not identify the configured VMSS")
instance=name[len(prefix):]
if not instance.isdecimal():
    raise SystemExit("Azure instance metadata has no numeric VMSS instance ID")
runner_name=config["runnerNamePrefix"]+"-"+str(int(instance)+config["runnerNumberOffset"])
benchmark=(
    any(label.lower()=="benchmark" for label in config["runnerLabels"])
    or config.get("benchmarkRunnerName")==runner_name
)
def field(source,name,pattern):
    value=source.get(name,"") if isinstance(source,dict) else None
    if not isinstance(value,str) or not re.fullmatch(pattern,value):
        raise SystemExit("Azure compute metadata contains an invalid "+name)
    return value
def safe_disk(disk,data=False):
    if not isinstance(disk,dict):
        raise SystemExit("Azure compute metadata has no disk profile")
    safe={
        "diskSizeGB":field(disk,"diskSizeGB",r"[0-9]{1,10}"),
        "managedDisk":{"storageAccountType":field(
            disk.get("managedDisk"),"storageAccountType",r"[A-Za-z0-9_]{1,64}"
        )},
        "caching":field(disk,"caching",r"None|ReadOnly|ReadWrite"),
        "writeAcceleratorEnabled":field(disk,"writeAcceleratorEnabled",r"(?i:true|false)"),
    }
    if data:
        safe["lun"]=field(disk,"lun",r"[0-9]{1,2}")
        if int(safe["lun"])>63:
            raise SystemExit("Azure data disk metadata contains an invalid LUN")
        for name in ("bytesPerSecondThrottle","opsPerSecondThrottle"):
            safe[name]=field(disk,name,r"[0-9]{0,20}")
    else:
        safe["diffDiskSettings"]={"option":field(
            disk.get("diffDiskSettings",{}),"option",r"[A-Za-z]{0,32}"
        )}
    return safe
profile=metadata["storageProfile"]
disks=profile.get("dataDisks")
if not isinstance(disks,list) or not disks:
    raise SystemExit("Azure compute metadata has no managed data disks")
safe={
    "vmSize":field(metadata,"vmSize",r"Standard_[A-Za-z0-9_-]{1,80}"),
    "storageProfile":{
        "osDisk":safe_disk(profile.get("osDisk")),
        "dataDisks":[safe_disk(disk,True) for disk in disks],
        "resourceDisk":{"size":field(profile.get("resourceDisk",{}),"size",r"[0-9]{0,16}")},
    },
}
print(runner_name)
print(str(benchmark).lower())
print(json.dumps(safe,separators=(",",":")))
PY
)
mapfile -t instance_values <<<"$instance_settings"
[ "${#instance_values[@]}" = 3 ] || {
    echo "ERROR: incomplete Azure instance settings" >&2
    exit 1
}
runner_name=${instance_values[0]}
benchmark_runner=${instance_values[1]}

metadata_directory=/var/lib/nvx-ci-host-metadata
if [ -L "$metadata_directory" ]; then
    echo "ERROR: host metadata directory must not be a symlink" >&2
    exit 1
fi
install -d -o root -g root -m 0755 "$metadata_directory"
metadata_file=$(mktemp "$metadata_directory/compute.XXXXXX")
trap 'rm -f -- "$metadata_file"' EXIT
printf '%s\n' "${instance_values[2]}" >"$metadata_file"
chmod 0644 "$metadata_file"
mv -T "$metadata_file" "$metadata_directory/compute.json"
trap - EXIT

if [ "$refresh_metadata_only" = true ]; then
    exit 0
fi

bash "$setup_root/setup-data-disk.sh"
mountpoint -q "$NVX_DATA_DISK_MOUNT_POINT" || {
    echo "ERROR: managed data disk is not mounted" >&2
    exit 1
}
if [ "$backend" = mshv ] && [ "$kernel_mode" = azurelinux-mshv ]; then
    if [ ! -c /dev/mshv ]; then
        if [ -f /var/log/enable-mshv.done ]; then
            echo "ERROR: MSHV is unavailable after host-stack enablement; inspect the Dom0 boot path" >&2
            exit 1
        fi
        bash "$setup_root/setup-mshv.sh"
        shutdown -r +1 "Booting the MSHV Dom0 entry for the CI runner"
        echo NVX_CLOUD_RUNNER_REBOOT_SCHEDULED=1
        exit 0
    fi
    bash "$setup_root/verify-mshv.sh"
fi
[ -c "/dev/$backend" ] || {
    echo "ERROR: /dev/$backend is unavailable" >&2
    exit 1
}

setup_arguments=(
    --runner-only
    --backend "$backend"
    --repository-url "https://github.com/$repository"
    --runner-directory "$runner_directory"
)
if [ -n "$extra_runner_labels" ]; then
    setup_arguments+=(--extra-runner-labels "$extra_runner_labels")
fi
sudo -u "$admin_username" -H sh "$setup_root/setup-linux.sh" "${setup_arguments[@]}"

metadata_service=nvx-runner-metadata-block.service
systemctl enable "$metadata_service"
systemctl restart "$metadata_service"
systemctl is-active --quiet "$metadata_service"

runner_token=
cleanup() { runner_token=; }
trap cleanup EXIT HUP INT TERM
runner_token=$(
    python3 - "$config_path" <<'PY'
import json,sys,urllib.request
with open(sys.argv[1],encoding="utf-8") as stream:
    config=json.load(stream)
def request_json(url,headers,method="GET"):
    request=urllib.request.Request(url,headers=headers,method=method)
    opener=(
        urllib.request.build_opener(urllib.request.ProxyHandler({}))
        if url.startswith("http://169.254.169.254/") else urllib.request.build_opener()
    )
    with opener.open(request,timeout=30) as response:
        return json.load(response)
identity=request_json(
    "http://169.254.169.254/metadata/identity/oauth2/token?api-version=2019-08-01&resource=https%3A%2F%2Fvault.azure.net",
    {"Metadata":"true"},
)
access_token=identity["access_token"]
if not access_token:
    raise SystemExit("Managed identity returned no Key Vault access token")
secret=request_json(
    "https://"+config["keyVaultName"]+".vault.azure.net/secrets/"+config["bootstrapSecretName"]+"?api-version=7.4",
    {"Authorization":"Bearer "+access_token},
)
github_token=secret["value"]
if not github_token:
    raise SystemExit("Key Vault returned no runner bootstrap credential")
registration=request_json(
    "https://api.github.com/repos/"+config["repository"]+"/actions/runners/registration-token",
    {"Authorization":"Bearer "+github_token,"Accept":"application/vnd.github+json","X-GitHub-Api-Version":"2022-11-28"},
    method="POST",
)
token=registration["token"]
if not token or "\n" in token or "\r" in token:
    raise SystemExit("GitHub returned an invalid runner registration token")
print(token)
PY
)
registration_arguments=("${setup_arguments[@]}" --runner-name "$runner_name")
if [ "$benchmark_runner" = true ]; then
    registration_arguments+=(--benchmark-runner)
fi
printf '%s\n' "$runner_token" |
    sudo -u "$admin_username" -H sh "$setup_root/setup-linux.sh" \
        "${registration_arguments[@]}" --runner-token-stdin
runner_token=

read -r service_name <"$runner_directory/.service"
if [[ ! "$service_name" =~ ^actions\.runner\.[A-Za-z0-9_.-]+\.service$ ||
    "$service_name" == *..* ]] ||
    [ "$(systemctl show "$service_name" --property=User --value)" != nvx-runner ]; then
    echo "ERROR: the registered runner service is not owned by nvx-runner" >&2
    exit 1
fi
drop_in="/etc/systemd/system/${service_name}.d"
mkdir -p "$drop_in"
printf '[Unit]\nRequires=%s\nWants=network-online.target\nAfter=%s network-online.target\n[Service]\nExecStartPre=+%s\nExecStartPre=+%s --refresh-metadata "%s"\n' \
    "$metadata_service" "$metadata_service" "$setup_root/block-runner-metadata.sh" \
    "$setup_root/bootstrap-runner.sh" "$config_path" \
    >"$drop_in/azure-metadata.conf"
systemctl daemon-reload
systemctl is-active --quiet "$service_name"
systemctl disable setup-nvx-github-runner.service
echo NVX_CLOUD_RUNNER_SETUP_COMPLETE=1
