use super::*;
use crate::host::snapshot::manifest::MICROVM_ABI_VERSION;

#[test]
fn the_boot_contract_version_names_the_microvm_abi() {
    // The boot contract includes the microVM ABI that the CLI's guests boot on, which the snapshot
    // decoder reads too, so a change to that ABI changes the boot contract, and so its version,
    // before the pins here change with it. `protocol::root` pins the boot tokens with it.
    assert_eq!(
        (BOOT_CONTRACT_VERSION, MICROVM_ABI_VERSION),
        (5, 1),
        "a change to the microVM ABI changes the boot contract: increment BOOT_CONTRACT_VERSION, \
         and update this test with it"
    );
}
