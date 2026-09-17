#!/bin/sh
set -eu

if [ "$#" -ne 4 ]; then
  echo "usage: $0 STAGE_DIR OPENVMM KERNEL INITRD" >&2
  exit 2
fi

stage_dir=$1
openvmm=$2
kernel=$3
initrd=$4

rm -rf -- "$stage_dir"
mkdir -p -- "$stage_dir"
cp -- "$openvmm" "$stage_dir/openvmm"
cp -- "$kernel" "$stage_dir/vmlinux"
cp -- "$initrd" "$stage_dir/initramfs.cpio.gz"
strip --strip-debug "$stage_dir/openvmm"
