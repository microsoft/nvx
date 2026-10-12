#!/bin/bash
set -euo pipefail
set +x

config_path=${1:-/opt/nvx-bootstrap/config.json}
setup_root=/opt/nvx-bootstrap
exec >>/var/log/nvx-runner-bootstrap.log 2>&1

settings=$(
    python3 - "$config_path" <<'PY'
import json,sys
with open(sys.argv[1],encoding="utf-8") as stream:
    config=json.load(stream)
if config["os"]!="linux" or config["backend"] not in ("kvm","mshv"):
    raise SystemExit("Expected a Linux KVM or MSHV configuration")
required=["linux",config["backend"],"virtual-machine"]
if [label.lower() for label in config["runnerLabels"][:3]]!=required:
    raise SystemExit("Runner labels must preserve the platform and backend labels")
values=[
    config["backend"],config["adminUsername"],config["repository"],
    ",".join(config["runnerLabels"][3:]),config["runnerDirectory"],
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

runner_name=$(
    python3 - "$config_path" <<'PY'
import json,sys,urllib.request
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
print(config["runnerNamePrefix"]+"-"+str(int(instance)+config["runnerNumberOffset"]))
PY
)
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
elif [ "$backend" = mshv ] && [ -x /usr/local/bin/aztemplates-verify-mshv.sh ]; then
    /usr/local/bin/aztemplates-verify-mshv.sh
fi
[ -c "/dev/$backend" ] || {
    echo "ERROR: /dev/$backend is unavailable" >&2
    exit 1
}

setup_arguments=(
    --backend "$backend"
    --repository-url "https://github.com/$repository"
    --runner-directory "$runner_directory"
)
if [ -n "$extra_runner_labels" ]; then
    setup_arguments+=(--extra-runner-labels "$extra_runner_labels")
fi
sudo -u "$admin_username" -H sh "$setup_root/setup-linux-runner.sh" "${setup_arguments[@]}"

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
printf '%s\n' "$runner_token" |
    sudo -u "$admin_username" -H sh "$setup_root/setup-linux-runner.sh" \
        "${setup_arguments[@]}" --runner-name "$runner_name" \
        --runner-token-stdin
runner_token=
echo NVX_CLOUD_RUNNER_SETUP_COMPLETE=1
