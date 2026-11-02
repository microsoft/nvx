set -eu
# Reports what the guest sees of hardware virtualization: whether its CPU
# offers VMX, SVM, and XSAVES, its KVM device, what the KVM probe finds, and
# whether it reads IA32_XSS (0xda0), which a guest hypervisor reads when it
# starts. The host decides what the report must say.
flag() {
    if grep -qw "$1" /proc/cpuinfo; then echo 1; else echo 0; fi
}

kvm=$(stat -c '%A:%u:%g:%t:%T' /dev/kvm 2>/dev/null) || kvm=absent
probe_status=0
probe=$(nvx-kvm-probe 2>&1) || probe_status=$?
# /dev/cpu/0/msr reads the MSR whose index is the offset: 436 blocks of 8.
if dd if=/dev/cpu/0/msr of=/dev/null bs=8 skip=436 count=1 2>/dev/null; then
    xss="read"
else
    xss="fault"
fi
echo "NVX-NESTED-VIRTUALIZATION vmx=$(flag vmx) svm=$(flag svm) xsaves=$(flag xsaves) xss=$xss kvm=$kvm probe_status=$probe_status"
echo "NVX-NESTED-VIRTUALIZATION-PROBE $probe"
echo NVX-NESTED-VIRTUALIZATION-OK
nvx-exit 0
