# Case matrix

NVX deleted its runtime adapter, `tests/runtime-adapter`, once the acceptance that replaces it
passed (#1086, #1119). Before then, this matrix gave each test of the adapter, of CI and of the
docs, and each microVM scenario, one fate, by name:

- **kept**: the test stays as it is, and runs without the adapter.
- **rewritten**: other tests check what it checks, without the adapter: those that the row
  names, which exist, or those that a later pull request adds, which the row names by its slice
  of #1076. Where only a part of what it checks leaves NVX, the row says which.
- **retired**: what it checks leaves NVX, as the row says, so nothing replaces it.

SL5.1b (#1118) added the smoke tests on real VMs of a cold boot: `test-microvm --scenario
vm-lifecycle` creates, spawns, reattaches to and stops a VM through the public API, and the CLI's
12 cold cells, EROFS and GPT images, each over a scratch or a RAM upper, run on KVM, MSHV and WHP,
as [Cold boots](#cold-boots) lists. SL5.1c (#1118) added those of snapshots, which capture a
snapshot, open it and restore it with nvx-init as the guest, run concurrent restores of one
snapshot under the CLI's lock, and check that a failed restore reports its attribution, as
[Snapshots](#snapshots) lists. SL3.7c (#1114) let a named snapshot keep a NIC, and runs the
network policies of its restores on real VMs with
`test-microvm --scenario untiered-network-policy`, as [Snapshots](#snapshots) lists too.

The inventory is fixed: it lists the tests as they are where this matrix was added, from
`tests/runtime-adapter/scenarios.txt` and the adapter's sources, `MICROVM_TEST_SCENARIOS` and
`MICROVM_EXPLICIT_SCENARIOS` in `scripts/nvx_tools/microvm_tests.py`, the workflows and actions
in `.github/`, and the code of the docs. A pull request that changes one of these tests updates
its row. A row may name a test of code that a later pull request of #1076 deletes, such as the
exec engine (#1087) or the old clone path (#1121): that pull request gives that test a fate of
its own in turn.

| Section | Tests | Kept | Rewritten | Retired |
| --- | ---: | ---: | ---: | ---: |
| [The adapter's scenarios](#the-adapters-scenarios) | 157 | 0 | 113 | 44 |
| [The adapter's other tests](#the-adapters-other-tests) | 169 | 0 | 9 | 160 |
| [MicroVM scenarios](#microvm-scenarios) | 54 | 50 | 3 | 1 |
| [CI](#ci) | 69 | 42 | 3 | 24 |
| [Docs](#docs) | 36 | 18 | 2 | 16 |

## The adapter's scenarios

`scenarios.txt` named 157 scenarios, which CI's runtime adapter step ran on KVM, MSHV
and WHP: the 154 of the adapter's stage, the two that `dev` added since,
`template_capture::the_host_program_refuses_a_repair_delay_that_it_cannot_read` and
`vm_clone::openvmm_reads_none_of_the_host_s_input_until_the_repair_acknowledges_the_restore`, and
the Linux-only `vm_clone::restore_exclusion::hard_linked_restore_views_obey_the_callers_exclusion`.
The adapter's README said what each proved.

### `template_capture` (3)

| Scenario | Fate | Coverage, or why it retires |
| --- | --- | --- |
| `a_capture_takes_its_class_and_inputs_from_the_request` | rewritten | A capture's inputs: `nvx-rs/src/host/vm/tests.rs::capture_creation_records_sparse_disks_devices_and_a_complete_serde_context`; `nvx-rs/src/host/config/tests.rs::each_input_that_identifies_a_capture_changes_its_identity`. The class and the repair delay of a template's clones retire: the template store, its generations and leases leave with the old clone path (#1121) |
| `the_host_program_refuses_a_repair_delay_that_it_cannot_read` | retired | The adapter's `RUNTIME_ADAPTER_REPAIR_DELAY` and the usage status of its host program |
| `the_host_program_refuses_a_template_before_it_creates_anything` | rewritten | A network that the lowering refuses fails before anything is created: `nvx-rs/tests/cli.rs::network_policies_that_the_planner_refuses_fail_before_launch`; the template store, its generations and leases leave with the old clone path (#1121) |

### `template_operations` (4)

| Scenario | Fate | Coverage, or why it retires |
| --- | --- | --- |
| `a_refresh_refuses_a_template_before_it_creates_anything` | rewritten | As above: `nvx-rs/tests/cli.rs::network_policies_that_the_planner_refuses_fail_before_launch`; refreshes leave with the template store (#1121) |
| `an_empty_template_store_lists_no_generation` | retired | The template store's listings, inspections and removals leave with the old clone path (#1121) |
| `the_host_program_refuses_command_lines_of_the_operations` | retired | The command lines and usage statuses of the adapter's template operations |
| `the_operations_open_a_template_store_and_create_none` | retired | The template store leaves with the old clone path (#1121) |

### `clone_start` (11)

| Scenario | Fate | Coverage, or why it retires |
| --- | --- | --- |
| `a_clone_s_plan_request_and_record_carry_its_network` | rewritten | A restore's own policy reaches its record: `nvx-rs/src/host/vm/tests.rs::resolved_restore_record_uses_captured_presence_address_and_its_own_policy`; `nvx-rs/src/host/network/tests.rs::proxy_urls_name_the_gateway_as_the_guest_sees_it`. Clone records retire (#1121) |
| `a_clone_s_record_names_its_template_store_and_generation_beside_its_plan` | retired | Clone records and template generations leave with the old clone path (#1121) |
| `a_clone_s_request_takes_the_staged_bindings_and_the_guest_s_defaults` | rewritten | A restore's resources and devices: `nvx-rs/src/host/snapshot/tests.rs::snapshot_accepts_matching_real_disk_geometry_and_every_resource_boundary`; `nvx-rs/src/host/vm/tests.rs::prepared_record_keeps_checked_root_view_export_resources_and_transaction`. The template store's staged bindings retire (#1121) |
| `a_host_s_time_keeps_its_text_form` | retired | The text form of the adapter's clock fixture. NVX's host time: `nvx-rs/src/host/snapshot/tests.rs::snapshot_accepts_reports_early_and_expired_time_as_unavailable` |
| `a_run_refuses_a_network_or_shares_that_the_plan_refuses_before_it_stages_anything` | rewritten | `nvx-rs/tests/cli.rs::network_policies_that_the_planner_refuses_fail_before_launch`; `nvx-rs/src/host/filesystem/tests.rs::read_only_paths_inside_read_write_directories_are_refused` |
| `a_run_refuses_a_selection_that_names_no_template_once_its_preflight_staged_the_images` | rewritten | Images that OpenVMM cannot attach are refused before anything: `nvx-rs/src/host/config/tests.rs::cold_boot_arguments_refuse_non_raw_block_extensions`; `nvx-rs/src/host/config/tests.rs::cold_boot_arguments_refuse_disk_syntax_in_image_and_scratch_paths`. Template selections and leases retire (#1121) |
| `a_start_refuses_a_generation_that_the_template_store_does_not_hold_before_anything_else` | retired | Template generations leave with the old clone path (#1121) |
| `a_start_refuses_a_network_that_the_plan_refuses_before_anything_else` | rewritten | `nvx-rs/tests/cli.rs::network_policies_that_the_planner_refuses_fail_before_launch` |
| `a_start_refuses_shares_that_it_cannot_export_before_anything_else` | rewritten | `nvx-rs/src/host/filesystem/tests.rs::read_only_paths_inside_read_write_directories_are_refused`; `nvx-rs/src/host/filesystem/tests.rs::an_export_refuses_guest_targets_that_are_not_canonical_or_that_a_root_reserves` |
| `the_host_program_refuses_options_that_do_not_make_a_run` | retired | The command line and statuses of the adapter's `run` |
| `the_host_program_refuses_options_that_do_not_make_a_start` | retired | The command line and statuses of the adapter's `start` |

### `host_session` (26)

| Scenario | Fate | Coverage, or why it retires |
| --- | --- | --- |
| `a_clone_s_order_ends_its_table_at_a_mapping_that_the_guest_fails_and_stops_the_guest` | rewritten | `nvx-rs/src/guest/managed_agent/dispatch/tests.rs::a_failed_mapping_keeps_refusing_workloads_through_dispatch`; `nvx-rs/src/guest/mappings/table/tests.rs::a_failed_mapping_keeps_those_before_it_and_latches_the_table`; `nvx-rs/src/host/restore_driver/tests.rs::mapping_and_start_failures_are_told_apart_from_the_machines`, whose restore sends no Start after a failed table, and leaves the VM to its caller to stop |
| `a_clone_s_order_maps_its_export_s_children_by_their_indices` | rewritten | `nvx-rs/src/host/config/tests.rs::mapping_table_is_the_exact_export_projection_counted_by_the_boot_token`; `nvx-rs/src/host/filesystem/tests.rs::unrelated_directories_are_exported_with_their_own_access`; `nvx-rs/src/host/restore_driver/tests.rs::a_lost_session_is_replaced_with_the_same_activate_message_and_the_live_session_returns`, whose restore sends its table after Ready; `nvx-rs/src/host/restore_driver/tests.rs::mapping_and_start_failures_are_told_apart_from_the_machines`, whose restore refuses a table that its record doesn't count |
| `a_clone_s_order_sends_its_record_first_then_starts_its_sandbox_as_after_a_cold_boot` | rewritten | `nvx-rs/src/host/restore/tests.rs::restore_machine_resends_canonical_bytes_and_stops_at_ready`; `nvx-rs/src/host/session/tests.rs::activate_sends_its_payload_and_returns_the_activation_that_ready_reports`; `nvx-rs/src/host/restore_driver/tests.rs::a_lost_session_is_replaced_with_the_same_activate_message_and_the_live_session_returns`, whose restore sends its activate message, then its table and its Start after Ready; `nvx-rs/src/host/restore_driver/tests.rs::mapping_and_start_failures_are_told_apart_from_the_machines` |
| `a_clone_s_session_that_ends_after_its_bootstrap_is_lost_and_the_next_takes_the_same_record` | rewritten | `nvx-rs/src/host/restore/tests.rs::restore_machine_resends_canonical_bytes_and_stops_at_ready`; `nvx-rs/src/host/restore/tests.rs::restore_machine_session_loss_invalidates_each_session_phase`; `nvx-rs/src/host/restore_driver/tests.rs::a_lost_session_is_replaced_with_the_same_activate_message_and_the_live_session_returns` |
| `a_mapping_that_the_guest_fails_ends_the_table_and_the_host_stops_the_guest` | rewritten | `nvx-rs/src/guest/managed_agent/dispatch/tests.rs::a_failed_mapping_keeps_refusing_workloads_through_dispatch`; `nvx-rs/tests/cli.rs::a_mapping_that_the_guest_refuses_leaves_no_usable_start` |
| `a_receive_that_times_out_keeps_the_partial_message` | rewritten | `nvx-rs/tests/nvxs/host.rs::a_receive_that_times_out_keeps_the_partial_record` |
| `a_reset_or_a_disconnect_in_the_middle_of_a_message_drops_it` | rewritten | `nvx-rs/tests/nvxs/session.rs::a_reset_abandons_the_message_that_the_host_left_unfinished`; `nvx-rs/tests/nvxs/host.rs::resets_truncations_violations_and_transport_failures_end_the_connection` |
| `a_send_that_fails_or_times_out_ends_the_client_with_what_it_holds` | rewritten | `nvx-rs/tests/nvxs/host.rs::a_send_that_fails_after_writing_ends_the_connection`; `nvx-rs/tests/nvxs/host.rs::transport_timeouts_keep_the_original_read_and_write_causes` |
| `a_stop_skips_the_guest_s_late_messages_about_the_executions_that_the_driver_gave_up_on` | retired | The adapter's execution driver, over the exec engine that leaves (#1087) |
| `answers_whose_bodies_do_not_decode_end_the_client` | retired | The adapter's framing of requests and answers |
| `only_a_session_that_ended_is_lost_to_a_clone_s_order` | rewritten | `nvx-rs/src/host/restore/tests.rs::restore_machine_session_loss_invalidates_each_session_phase`; `nvx-rs/src/host/restore/tests.rs::restore_machine_keeps_operation_phase_evidence_and_original_source`; `nvx-rs/src/host/restore_driver/tests.rs::a_lost_session_is_replaced_with_the_same_activate_message_and_the_live_session_returns`, whose restore replaces a reset session; `nvx-rs/src/host/restore_driver/tests.rs::a_third_lost_session_and_a_malformed_ready_are_final`; `nvx-rs/src/host/restore_driver/tests.rs::ready_has_sixty_seconds_and_start_thirty_from_ready_for_each_pending_accept_and_read`, whose restore ends at an attachment or a Ready that comes too late; `nvx-rs/src/host/session/tests.rs::attach_reports_authentication_failure`; `nvx-rs/src/host/session/tests.rs::activate_refuses_a_reply_other_than_a_valid_ready`. The guest's build in its Ready and the adapter's framing of a record retire with the adapter's messages |
| `requests_check_their_answers_and_end_the_client_when_they_fail` | retired | The adapter's framing of requests and answers |
| `the_client_attaches_and_exchanges_messages_that_span_records` | rewritten | `nvx-rs/tests/nvxs/host.rs::the_host_connection_waits_attaches_and_exchanges_messages` |
| `the_client_configures_a_layered_root_probes_it_and_learns_how_its_teardown_ended` | retired | The adapter's root configuration, probes and teardown report |
| `the_client_fails_on_records_that_the_protocol_does_not_allow` | rewritten | `nvx-rs/src/protocol/nvxs/tests/host.rs::the_host_checks_the_brokers_sequence_numbers`; `nvx-rs/src/protocol/nvxs/tests/host.rs::the_host_checks_the_sessions_identity`; `nvx-rs/tests/nvxs/host.rs::resets_truncations_violations_and_transport_failures_end_the_connection` |
| `the_client_reassembles_messages_from_partial_and_shared_records` | rewritten | `nvx-rs/tests/nvxs/host.rs::the_host_connection_waits_attaches_and_exchanges_messages`; `nvx-rs/src/protocol/nvxs/tests/record.rs::the_parser_returns_the_same_records_for_every_split_of_a_stream` |
| `the_client_refuses_messages_that_its_framing_does_not_allow` | retired | The adapter's message headers, kinds and limit |
| `the_client_returns_the_stage_of_the_root_that_failed` | retired | The adapter's stages of a root |
| `the_client_waits_for_the_guest_and_reports_refusals_while_attaching` | rewritten | `nvx-rs/tests/nvxs/host.rs::the_host_connection_reports_refusals_and_closed_streams_while_attaching` |
| `the_driver_ends_its_run_on_a_message_that_breaks_the_rules` | retired | The adapter's execution driver, over the exec engine that leaves (#1087) |
| `the_driver_fires_the_timers_of_its_executions_while_the_guest_says_nothing` | retired | The adapter's execution driver, over the exec engine that leaves (#1087) |
| `the_driver_refuses_starts_before_it_sends_them` | retired | The adapter's execution driver, over the exec engine that leaves (#1087) |
| `the_driver_runs_executions_at_once_and_collects_their_output_and_outcomes` | retired | The adapter's execution driver, over the exec engine that leaves (#1087) |
| `the_driver_s_sends_wait_for_a_guest_that_stops_reading_until_the_deadline_of_its_run` | retired | The adapter's execution driver, over the exec engine that leaves (#1087) |
| `the_host_orders_a_layered_root_ready_then_its_table_in_parts_then_start` | rewritten | `nvx-rs/src/host/session/tests.rs::host_mappings_travel_in_records_that_the_guest_acknowledges`; `nvx-rs/src/guest/managed_agent/dispatch/tests.rs::restored_ready_maps_and_first_exec_share_the_existing_start_transition` |
| `the_host_sends_nothing_more_to_a_guest_agent_of_another_build_or_whose_root_failed` | rewritten | A failed root reports no Ready and admits nothing: `nvx-rs/src/guest/managed_agent/session/tests.rs::failed_application_never_reports_ready_or_admits_root_dependent_work`. The adapter's check of its guest agent's build retires |

### `staged_images` (4)

| Scenario | Fate | Coverage, or why it retires |
| --- | --- | --- |
| `a_launch_takes_the_bindings_that_the_store_stages_for_its_images` | rewritten | `nvx-rs/tests/cli.rs::start_refuses_a_configuration_without_a_valid_distro_binding`; `nvx-rs/tests/cli.rs::start_refuses_a_configuration_without_a_valid_scratch_binding`. The adapter's staging store retires |
| `staging_verifies_the_image_and_binds_the_scratch` | rewritten | `nvx-rs/src/host/sandbox/distro/tests.rs::verification_binds_the_layer_its_manifest_and_the_policy`; `nvx-rs/src/host/sandbox/scratch/tests.rs::binds_refuse_what_is_not_a_scratch_image`; `nvx-rs/src/host/sandbox/distro/tests.rs::a_staged_image_that_was_replaced_fails_its_check` |
| `the_host_program_stages_images_in_the_store` | retired | The adapter's `stage` command and its record |
| `the_store_records_the_staged_images` | retired | The adapter's record of its staged images |

### `layered_launch` (16)

| Scenario | Fate | Coverage, or why it retires |
| --- | --- | --- |
| `a_launch_claims_its_scratch_image_until_it_releases_its_checked_files` | rewritten | `nvx-rs/src/host/sandbox/scratch/tests.rs::a_scratch_image_is_claimed_by_one_launch_and_checked_again_after_a_release`; `nvx-rs/src/host/sandbox/scratch/tests.rs::a_claim_lasts_while_the_vmm_holds_the_image` |
| `a_launch_reads_only_the_networks_that_the_plan_takes` | rewritten | `nvx-rs/tests/cli.rs::network_policies_that_the_planner_refuses_fail_before_launch`; `nvx-rs/src/host/network/tests.rs::invalid_rules_are_rejected_with_the_policy_compilers_messages` |
| `a_launch_records_what_it_launched` | retired | The adapter's launch records. Callers keep `SavedVm`: `nvx-rs/src/host/vm/tests/reattach.rs::verifying_saved_values_checks_runtime_and_process_without_opening_an_endpoint` |
| `a_launch_refuses_images_that_are_not_staged_or_changed_since` | rewritten | `nvx-rs/src/host/sandbox/distro/tests.rs::a_staged_image_that_was_replaced_fails_its_check`; `nvx-rs/src/host/sandbox/scratch/tests.rs::checks_fail_without_reading_when_the_image_changes`; `nvx-rs/tests/cli.rs::start_refuses_a_configuration_without_a_valid_distro_binding` |
| `a_launch_refuses_shares_that_it_cannot_export` | rewritten | `nvx-rs/src/host/filesystem/tests.rs::policy_paths_hide_show_and_narrow_writes`; `nvx-rs/src/host/filesystem/tests.rs::an_export_refuses_a_read_only_path_inside_a_writable_directory` |
| `a_launch_renders_each_element_of_its_network_through_the_plan` | rewritten | `nvx-rs/src/host/config/tests.rs::cold_boot_arguments_lower_layered_roots_networks_and_exports_by_path`; `nvx-rs/src/host/network/tests.rs::proxy_urls_name_the_gateway_as_the_guest_sees_it` |
| `a_launch_shares_host_paths_inside_the_authorized_directories` | rewritten | `nvx-rs/src/host/filesystem/tests.rs::policy_paths_hide_show_and_narrow_writes`; `nvx-rs/src/host/config/tests.rs::cold_boot_arguments_lower_layered_roots_networks_and_exports_by_path` |
| `a_layered_launch_hands_openvmm_the_checked_images` | rewritten | `nvx-rs/src/host/config/tests.rs::cold_boot_arguments_lower_layered_roots_networks_and_exports_by_path`. A launch names its images by path: the checked-file handoff leaves (SL6.3, #1122), as `images_replaced_after_the_checks_never_reach_the_vm` says |
| `a_layered_launch_passes_what_the_guest_checks_as_it_is` | rewritten | `nvx-rs/src/guest/init/boot/tests.rs::cold_boot_reads_the_actual_root_view_export_maps_and_cli_contracts` |
| `a_layered_launch_plans_its_blocks_and_boot_tokens_with_the_adapters_plan` | rewritten | `nvx-rs/src/host/config/tests.rs::cold_command_lines_carry_the_root_the_view_and_the_mapping_count`; `nvx-rs/src/host/config/tests.rs::cold_boot_arguments_lower_layered_roots_networks_and_exports_by_path` |
| `an_execution_finds_the_url_of_its_network_s_proxy_in_its_environment` | rewritten | `nvx-rs/src/guest/managed_agent/launch/tests.rs::restored_proxy_defaults_replace_template_values_before_exec_overrides`; `test-microvm --scenario sandbox-proxy-only` |
| `shares_read_their_host_path_their_target_and_their_access` | rewritten | `nvx-rs/src/host/cli/run/guest_tokens/tests.rs::planned_boot_shares_keep_table_sources_targets_access_and_file_kind` |
| `the_host_program_refuses_a_network_before_it_launches` | rewritten | `nvx-rs/tests/cli.rs::network_policies_that_the_planner_refuses_fail_before_launch` |
| `the_host_program_refuses_options_that_do_not_make_a_command` | retired | The adapter's command lines and usage statuses |
| `the_host_program_refuses_shares_before_it_touches_the_store` | rewritten | `nvx-rs/src/host/filesystem/tests.rs::read_only_paths_inside_read_write_directories_are_refused`; `nvx-rs/src/host/filesystem/tests.rs::an_export_refuses_a_read_only_path_inside_a_writable_directory` |
| `the_record_of_a_launch_gives_back_its_plan` | retired | The adapter's launch record and its reader |

### `vm_restart` (3)

| Scenario | Fate | Coverage, or why it retires |
| --- | --- | --- |
| `a_host_program_of_another_runtime_finds_the_vm_mismatched_and_signals_nothing` | rewritten | `nvx-rs/src/host/vm/tests/reattach.rs::a_saved_vm_runs_until_its_process_exits_and_another_runtime_is_refused_without_a_signal`; `nvx-rs/src/host/vm/tests/reattach.rs::verifying_saved_values_checks_runtime_and_process_without_opening_an_endpoint` |
| `a_launch_tears_down_a_vm_whose_guest_agent_is_of_another_build` | retired | The adapter's guest agent and the build that its Ready reports |
| `a_restarted_host_program_reattaches_to_the_vm_and_stops_it` | rewritten | `nvx-rs/tests/vm_lifecycle.rs::a_cold_vm_is_reattached_from_saved_values_and_stopped`, which `test-microvm --scenario vm-lifecycle` runs; `nvx-rs/src/host/vm/tests/reattach.rs::reattaching_returns_the_held_owned_child_and_authenticated_connection`; `nvx-rs/src/host/vm/tests.rs::stop_forces_only_its_verified_process_after_grace` |

### `vm_steps` (3)

| Scenario | Fate | Coverage, or why it retires |
| --- | --- | --- |
| `a_launch_that_the_hook_ends_after_its_checks_leaves_no_launch` | rewritten | A launch that fails after its checks leaves nothing of its own: `nvx-rs/src/host/vm/tests.rs::creation_rolls_back_its_log_and_never_removes_preexisting_outputs_or_inputs`; `nvx-rs/src/host/sandbox/scratch/tests.rs::a_scratch_image_is_claimed_by_one_launch_and_checked_again_after_a_release`. The step hook retires |
| `images_replaced_after_the_checks_never_reach_the_vm` | retired | The checked-file handoff leaves (SL6.3, #1122): a launch names its images by path, so what takes an image's path after its check is what OpenVMM opens. Callers stage their images in a directory that only their own account can write, as the docs of `host::sandbox::distro`, `host::sandbox::scratch` and `host::seal` require, so that no other account can replace a checked image |
| `the_hook_pauses_a_launch_at_each_of_its_steps` | retired | The adapter's step hook |

### `vm_layered` (43)

| Scenario | Fate | Coverage, or why it retires |
| --- | --- | --- |
| `a_changed_image_is_refused_before_spawn` | rewritten | `nvx-rs/src/host/sandbox/distro/tests.rs::a_staged_image_that_was_replaced_fails_its_check`; `nvx-rs/src/host/sandbox/distro/tests.rs::the_staged_check_fails_without_reading_when_the_layer_changes`; `nvx-rs/tests/cli.rs::start_refuses_a_configuration_without_a_valid_distro_binding` |
| `a_denied_network_lets_no_probe_through` | rewritten | A deny-all network now keeps its NIC, which lets nothing through: `nvx-rs/src/host/config/tests.rs::an_explicit_deny_all_network_keeps_its_nic`; `test-microvm --scenario directional-network-policy` |
| `a_failed_mapping_tears_the_vm_down` | rewritten | The guest's refusal, which keeps the mappings before it, latches the table and refuses the start: `nvx-rs/src/guest/mappings/table/tests.rs::a_failed_mapping_keeps_those_before_it_and_latches_the_table`; `nvx-rs/src/guest/managed_agent/dispatch/tests.rs::a_failed_mapping_keeps_refusing_workloads_through_dispatch`. The stop that the guest confirms, with a successful outcome and no record of the VM: `nvx-rs/tests/cli.rs::a_mapping_that_the_guest_refuses_leaves_no_usable_start`. The teardown of the root and its mappings: `nvx-rs/src/guest/root/tests.rs::a_teardown_unmounts_the_layers_top_first_and_reports_every_failure`; `nvx-rs/src/guest/mappings/mapper/tests/privileged.rs::a_teardown_unmounts_each_mapping_and_reports_a_busy_one` |
| `a_forward_reaches_its_guest_port` | rewritten | `test-microvm --scenario host-loopback-policy` |
| `a_proxy_only_network_lets_only_its_proxy_through` | rewritten | `test-microvm --scenario sandbox-proxy-only` |
| `a_read_only_share_inside_a_writable_one_is_refused_before_launch` | rewritten | `nvx-rs/src/host/filesystem/tests.rs::an_export_refuses_a_read_only_path_inside_a_writable_directory` |
| `a_replaced_scratch_is_refused_before_spawn` | rewritten | `nvx-rs/src/host/sandbox/scratch/tests.rs::a_scratch_image_is_claimed_by_one_launch_and_checked_again_after_a_release`; `nvx-rs/tests/cli.rs::start_refuses_a_configuration_without_a_valid_scratch_binding` |
| `a_share_inside_a_reserved_tree_is_refused_before_launch` | rewritten | `nvx-rs/src/host/filesystem/tests.rs::an_export_refuses_guest_targets_that_are_not_canonical_or_that_a_root_reserves`; `nvx-rs/src/host/filesystem/tests.rs::reserved_targets_depend_on_the_root` |
| `a_share_over_a_reserved_tree_is_refused_before_launch` | rewritten | `nvx-rs/src/host/filesystem/tests.rs::an_export_refuses_guest_targets_that_are_not_canonical_or_that_a_root_reserves`; `nvx-rs/src/host/filesystem/tests.rs::reserved_targets_depend_on_the_root` |
| `accountless_workload_identity_fails_its_stage` | rewritten | `test-microvm --scenario workload-identity`; `nvx-rs/src/guest/init/boot/tests.rs::account_creation_replaces_only_the_requested_unusable_identity_once` |
| `allowed_host_loopback_publishes_the_listeners` | rewritten | `test-microvm --scenario host-loopback-policy` |
| `an_allow_rule_lets_only_its_destination_through` | rewritten | `test-microvm --scenario l3-l4-egress-policy`; `nvx-rs/src/host/network/policy/tests/properties.rs::policies_lower_to_canonical_rules_that_match_exactly` |
| `an_unauthorized_host_path_is_refused_before_launch` | retired | The adapter's own directories that it authorizes: an NVX caller chooses the host paths that it maps |
| `busy_root_fails_the_teardown` | rewritten | `nvx-rs/src/guest/mappings/mapper/tests/privileged.rs::a_teardown_unmounts_each_mapping_and_reports_a_busy_one`; `nvx-rs/src/guest/root/tests.rs::a_teardown_unmounts_the_layers_top_first_and_reports_every_failure` |
| `corrupt_layer_fails_the_assembly` | rewritten | `nvx-rs/src/guest/root/tests.rs::a_layer_that_is_not_ext4_or_whose_journal_needs_a_recovery_is_refused_before_any_mount` |
| `cyclic_journal_is_not_loaded` | rewritten | `nvx-rs/src/guest/root/block/tests.rs::the_superblock_tells_whether_an_ext4_journal_exists_and_needs_a_recovery`; `nvx-rs/src/guest/root/tests.rs::a_gpt_disk_s_layers_are_mounted_under_the_overlay_top_first` |
| `erofs_over_ram` | rewritten | `test-microvm --scenario sandbox-ram-upper`, among the [cold cells](#cold-boots); `nvx-rs/src/guest/root/tests.rs::a_tmpfs_upper_needs_no_scratch_device` |
| `erofs_over_ram_maps_the_table` | rewritten | `nvx-rs/src/guest/mappings/table/tests.rs::mappings_keep_the_paths_and_the_access_of_their_entries`; `nvx-rs/src/guest/init/boot/tests.rs::layered_cold_boot_maps_through_handles_opened_before_its_only_switch` |
| `erofs_over_ram_with_untrusted_lowers` | rewritten | `nvx-rs/src/guest/root/tests.rs::both_overlay_modes_assemble_on_both_uppers_with_their_own_option_sets` |
| `erofs_over_scratch` | rewritten | `test-microvm --scenario sandbox-lifecycle`, among the [cold cells](#cold-boots); `nvx-rs/src/guest/root/tests.rs::assembly_mounts_the_image_and_the_scratch_under_the_overlay`. The adapter's cgroups retire with the exec engine (#1087) |
| `erofs_over_scratch_maps_the_table` | rewritten | `nvx-rs/src/guest/mappings/mapper/tests.rs::mappings_are_restricted_copies_attached_in_order`; `nvx-rs/src/guest/init/boot/tests.rs::layered_cold_boot_maps_through_handles_opened_before_its_only_switch` |
| `erofs_over_scratch_with_untrusted_lowers` | rewritten | `nvx-rs/src/guest/root/tests.rs::both_overlay_modes_assemble_on_both_uppers_with_their_own_option_sets` |
| `executions_run_as_the_workload_identity` | rewritten | `test-microvm --scenario workload-identity`; `test-microvm --scenario managed-exec-config`. An execution's own identity, which the adapter refused, retires with the exec engine (#1087) |
| `executions_run_in_the_root` | rewritten | `test-microvm --scenario sandbox-lifecycle`; `test-microvm --scenario managed-exec-config`. Executions that name their identities retire with the exec engine (#1087) |
| `executions_take_the_settings_of_a_proxy_only_network` | rewritten | `test-microvm --scenario sandbox-proxy-only`; `nvx-rs/src/guest/managed_agent/launch/tests.rs::restored_proxy_defaults_replace_template_values_before_exec_overrides` |
| `first_partition_over_its_bound_fails_the_assembly` | rewritten | `nvx-rs/src/gpt/tests.rs::a_first_partition_over_the_bound_fails_before_it_is_read`; `nvx-rs/src/guest/root/tests.rs::the_first_partition_is_returned_as_opaque_bytes_within_its_bound` |
| `forbidden_journal_fails_the_assembly` | rewritten | `nvx-rs/src/guest/root/tests.rs::the_journal_policy_is_checked_in_the_scratch_s_superblock_before_any_mount` |
| `full_ram_upper_refuses_writes` | rewritten | The tmpfs takes its cap as its size, which the kernel enforces: `nvx-rs/src/guest/root/tests.rs::both_overlay_modes_assemble_on_both_uppers_with_their_own_option_sets`; `nvx-rs/src/host/config/tests.rs::a_ram_upper_holds_1_to_90_percent_of_the_memory_and_half_by_default` |
| `full_scratch_refuses_writes` | retired | ext4 refuses writes to a full scratch; NVX sets nothing of it |
| `gpt_over_ram` | rewritten | `test-microvm --scenario sandbox-gpt-layer`, among the [cold cells](#cold-boots); `nvx-rs/src/guest/root/tests.rs::a_gpt_disk_s_layers_are_mounted_under_the_overlay_top_first` |
| `gpt_over_ram_maps_the_table` | rewritten | `nvx-rs/src/guest/mappings/table/tests.rs::mappings_keep_the_paths_and_the_access_of_their_entries`; `nvx-rs/src/guest/init/boot/tests.rs::layered_cold_boot_maps_through_handles_opened_before_its_only_switch` |
| `gpt_over_ram_with_untrusted_lowers` | rewritten | `nvx-rs/src/guest/root/tests.rs::both_overlay_modes_assemble_on_both_uppers_with_their_own_option_sets` |
| `gpt_over_scratch` | rewritten | `test-microvm --scenario sandbox-gpt-layer`, among the [cold cells](#cold-boots); `nvx-rs/src/guest/root/tests.rs::the_first_partition_is_returned_as_opaque_bytes_within_its_bound`; `nvx-rs/src/guest/root/tests.rs::a_gpt_disk_s_layers_are_mounted_under_the_overlay_top_first` |
| `gpt_over_scratch_maps_the_table` | rewritten | `nvx-rs/src/guest/mappings/mapper/tests.rs::targets_are_created_only_on_the_root_s_own_mounts`; `nvx-rs/src/guest/init/boot/tests.rs::layered_cold_boot_maps_through_handles_opened_before_its_only_switch` |
| `gpt_over_scratch_with_untrusted_lowers` | rewritten | `nvx-rs/src/guest/root/tests.rs::both_overlay_modes_assemble_on_both_uppers_with_their_own_option_sets` |
| `invalid_configuration_fails_its_stage` | retired | The adapter's configuration stage and its status |
| `other_disk_guid_fails_the_assembly` | rewritten | `nvx-rs/src/guest/root/tests.rs::a_gpt_disk_that_differs_from_the_expected_identities_is_refused_before_any_mount` |
| `other_identity_fails_the_assembly` | rewritten | `nvx-rs/src/guest/root/tests.rs::the_expected_uuid_is_checked_in_the_superblock_before_the_mount` |
| `outbound_host_loopback_publishes_no_listener` | rewritten | `test-microvm --scenario host-loopback-policy` |
| `published_listeners_refuse_binding_and_raw_sockets` | retired | The capability allowlist of the old guest entry leaves with the exec engine (#1087): nvx-init runs each execution as a plain process |
| `recovery_required_layer_fails_the_assembly` | rewritten | `nvx-rs/src/guest/root/tests.rs::a_layer_that_is_not_ext4_or_whose_journal_needs_a_recovery_is_refused_before_any_mount` |
| `root_profiles_enter_the_root` | retired | Per-execution namespaces, root profiles and capability sets leave with the exec engine (#1087) |
| `unstaged_images_are_refused_before_spawn` | rewritten | `nvx-rs/tests/cli.rs::start_refuses_a_configuration_without_a_valid_distro_binding`; `nvx-rs/src/host/sandbox/distro/tests.rs::a_staged_image_that_was_replaced_fails_its_check` |

### `vm_template` (8)

| Scenario | Fate | Coverage, or why it retires |
| --- | --- | --- |
| `a_class_that_no_capture_takes_is_refused_before_the_store` | rewritten | `nvx-rs/src/host/config/tests.rs::each_refusal_of_a_capture_names_what_it_refuses`; `nvx-rs/src/host/vm/tests.rs::capture_creation_refuses_unaligned_boot_memory_before_creating_outputs` |
| `a_network_that_lets_nothing_through_has_no_nic` | rewritten | A deny-all network now keeps its NIC: `nvx-rs/src/host/config/tests.rs::an_explicit_deny_all_network_keeps_its_nic`; `nvx-rs/src/host/snapshot/tests.rs::snapshot_accepts_refuses_both_nic_presence_mismatches_and_each_named_address_mismatch` |
| `a_refresh_captures_and_retires_the_generations_that_it_supersedes` | retired | Refreshes, generations, eviction and tombstones leave with the old clone path (#1121) |
| `a_refresh_refuses_a_class_that_no_capture_takes_before_the_store` | retired | Refreshes leave with the template store (#1121); a capture's refusals: `nvx-rs/src/host/config/tests.rs::each_refusal_of_a_capture_names_what_it_refuses` |
| `a_removal_reports_busy_while_leases_keep_the_generation` | retired | Removals, leases, tombstones and generations leave with the old clone path (#1121) |
| `a_template_whose_processors_are_all_online_at_boot_limits_none` | rewritten | `nvx-rs/src/host/snapshot/tests.rs::snapshot_accepts_treats_zero_boot_processors_as_all_capacity_online`; `nvx-rs/src/host/config/tests.rs::fresh_captures_pass_with_supported_processors_and_aligned_memory`; `test-microvm --scenario smp-snapshot` |
| `a_template_without_a_network_has_no_nic` | rewritten | `nvx-rs/src/host/vm/tests.rs::capture_creation_records_sparse_disks_devices_and_a_complete_serde_context`; `nvx-rs/src/host/snapshot/tests.rs::snapshot_accepts_refuses_both_nic_presence_mismatches_and_each_named_address_mismatch`. A store that serves it again retires (#1121) |
| `the_networks_of_every_policy_at_one_guest_s_address_share_one_template` | rewritten | `nvx-rs/src/host/network/tests.rs::templates_attach_their_nic_under_the_complete_deny_all_policy`; `test-microvm --scenario untiered-network-policy`, whose four restores of one named snapshot with a NIC each install a network policy of their own. A cached template retires (#1121) |

### `vm_clone` (36)

| Scenario | Fate | Coverage, or why it retires |
| --- | --- | --- |
| `a_clone_binds_the_resolver_of_its_policy_s_dns_server_read_only` | rewritten | `test-microvm --scenario untiered-network-policy`, whose restored guests' resolvers name exactly the DNS server of their policies; `test-microvm --scenario network-snapshot`; `nvx-rs/src/guest/root/tests/view.rs::actual_resolver_failures_keep_the_network_class_and_root_owned_bind` |
| `a_clone_maps_an_aggregate_of_directories_and_files_beneath_its_root_before_its_sandbox_starts` | rewritten | `test-microvm --scenario platform-aggregate-export`; `nvx-rs/src/guest/mappings/table/tests.rs::mappings_keep_the_paths_and_the_access_of_their_entries` |
| `a_clone_of_a_template_with_a_nic_maps_its_export_beside_its_resolver` | rewritten | `nvx-rs/src/guest/activation/state/apply/tests.rs::the_record_s_view_is_applied_once_with_its_own_dns`; `test-microvm --scenario filesystem-snapshot` |
| `a_clone_reads_no_more_of_its_disks_than_its_root_needs` | rewritten | A restore reads no content of its disks or snapshot: `nvx-rs/src/host/snapshot/tests.rs::snapshot_accepts_never_reopens_snapshot_paths_or_reads_runtime_artifacts`; `nvx-rs/src/host/sandbox/distro/tests.rs::the_staged_check_reads_no_content`; `nvx-rs/src/host/sandbox/scratch/tests.rs::checks_claim_the_image_without_reading_it`. The adapter's in-guest read statistics retire |
| `a_clone_refuses_a_scratch_image_whose_upper_an_earlier_clone_used` | rewritten | `nvx-rs/src/guest/root/tests.rs::a_fresh_upper_s_directories_must_be_missing_or_empty` |
| `a_clone_runs_the_executions_that_name_their_identity_once_it_is_ready` | retired | Executions that name their identities leave with the exec engine (#1087) |
| `a_clone_s_executions_find_the_proxy_of_its_policy` | rewritten | `nvx-rs/src/guest/managed_agent/launch/tests.rs::restored_proxy_defaults_replace_template_values_before_exec_overrides`; `test-microvm --scenario platform-network-policy` |
| `a_clone_s_time_settles_after_it_is_ready` | rewritten | `test-microvm --scenario time-host-correction`; `test-microvm --scenario restore-downtime` |
| `a_clone_whose_activation_fails_ends_before_it_is_ready` | rewritten | `nvx-rs/src/host/restore/tests.rs::restore_failures_attribute_the_existing_policy_free_evidence_classes`; `nvx-rs/src/host/restore/tests.rs::restore_exit_reports_keep_guest_status_classification_and_actual_details`; `nvx-rs/src/guest/init/boot/tests.rs::a_failed_activation_ends_the_vm_with_the_status_of_its_class`; `nvx-rs/src/guest/managed_agent/session/tests.rs::a_failed_restored_activation_gives_init_the_status_of_its_class`; `nvx-rs/src/host/cli/sandbox/lifecycle/start/tests/restore.rs::a_restore_that_fails_before_its_guest_is_ready_reports_its_attribution`; `test-microvm --scenario snapshot-core`, whose restore over a zeroed scratch image ends its VM before its workload runs, with nvx-init's status of an unusable root, 54, which the CLI attributes to the request. Quarantine and leases retire (#1121) |
| `a_clone_whose_mapping_fails_ends_before_its_sandbox_starts` | rewritten | `nvx-rs/src/guest/mappings/mapper/tests.rs::a_failure_at_any_step_leaves_nothing_mounted`; `nvx-rs/src/guest/mappings/table/tests.rs::a_failed_mapping_keeps_those_before_it_and_latches_the_table` |
| `a_clone_whose_view_cannot_be_set_ends_before_it_is_ready` | rewritten | `nvx-rs/src/guest/activation/state/apply/tests.rs::the_record_s_view_is_applied_once_with_its_own_dns`; `nvx-rs/src/guest/root/tests/view.rs::a_later_bind_failure_stays_owned_and_its_teardown_removes_the_earlier_bind` |
| `a_clone_whose_vm_exits_once_a_stop_found_it_fails_before_its_lease_is_released` | retired | Clone records and leases leave with the old clone path (#1121). A stop that finds an exit: `nvx-rs/src/host/vm/tests.rs::stop_preserves_report_results_after_a_real_self_exit` |
| `a_forward_whose_port_another_socket_holds_fails_the_clone_as_the_host_s` | rewritten | `test-microvm --scenario platform-network-policy`; `nvx-rs/src/host/restore/tests.rs::restore_failures_attribute_the_existing_policy_free_evidence_classes` |
| `a_generation_is_eligible_by_the_clock_of_its_capture` | rewritten | `nvx-rs/src/host/snapshot/eligibility/tests.rs::within_one_boot_the_downtime_is_the_host_monotonic_clocks`; `nvx-rs/src/host/snapshot/tests.rs::snapshot_accepts_reports_early_and_expired_time_as_unavailable` |
| `a_generation_refuses_clones_of_fewer_processors_than_its_template_booted_with` | rewritten | `nvx-rs/src/host/snapshot/tests.rs::snapshot_accepts_refuses_a_blockless_root_and_each_processor_mismatch`; `test-microvm --scenario restore-processors` |
| `a_host_program_of_another_runtime_ends_the_clone_through_its_verified_identity` | rewritten | `nvx-rs/src/host/vm/tests/reattach.rs::a_saved_vm_runs_until_its_process_exits_and_another_runtime_is_refused_without_a_signal`; `nvx-rs/src/host/vm/tests.rs::stop_forces_only_its_verified_process_after_grace`. Leases retire (#1121) |
| `a_run_in_a_template_of_a_network_finds_its_policy_s_proxy` | rewritten | `test-microvm --scenario platform-network-policy`; `test-microvm --scenario untiered-network-policy`, whose runs restore a named snapshot with a NIC under a network policy of their own, though with no proxy. The template's `run` retires (#1121) |
| `a_run_runs_one_execution_in_its_clone_then_tears_the_clone_down` | rewritten | `test-microvm --scenario snapshot-core`, whose `run --restore-snapshot` runs one workload in its restored VM and removes its sandbox. The template's `run` and leases retire (#1121) |
| `a_run_s_execution_finds_its_shares_beneath_the_root` | rewritten | `test-microvm --scenario platform-aggregate-export` |
| `a_run_whose_clone_does_not_start_runs_nothing` | retired | Template selections and the template's `run` leave with the old clone path (#1121) |
| `a_run_whose_clone_s_vm_exits_during_its_execution_ends_with_the_clone_s_failure` | retired | The template's `run`, clone records and leases leave with the old clone path (#1121) |
| `a_selection_that_names_no_template_leases_nothing` | retired | Template selections and leases leave with the old clone path (#1121) |
| `a_start_refuses_an_export_whose_mapped_objects_changed_before_any_vmm_starts` | rewritten | `nvx-rs/src/host/filesystem/tests.rs::an_exported_object_that_a_workload_could_move_is_checked_again_before_a_start`; `nvx-rs/src/host/filesystem/tests.rs::swapped_objects_fail_verification`; `nvx-rs/src/host/cli/sandbox/state/tests.rs::records_keep_the_export_with_its_pins_and_starts_refuse_changed_pins`, whose record keeps the pins beside the export, and whose start refuses a changed pin before the VM's configuration takes the export (SL6.5b) |
| `a_start_refuses_what_the_template_cannot_serve_before_any_vmm_starts` | rewritten | `nvx-rs/src/host/snapshot/tests.rs::snapshot_accepts_refuses_an_image_longer_than_its_placeholder`; `nvx-rs/src/host/snapshot/tests.rs::snapshot_accepts_refuses_each_scratch_geometry_mismatch`; `nvx-rs/src/host/snapshot/tests.rs::snapshot_accepts_refuses_each_memory_mismatch_without_rounding_or_overflow` |
| `a_template_without_a_nic_serves_clones_without_a_network` | rewritten | `nvx-rs/src/host/snapshot/tests.rs::snapshot_accepts_refuses_both_nic_presence_mismatches_and_each_named_address_mismatch` |
| `a_tombstone_leaves_the_running_clone_of_its_generation_to_its_lease` | retired | Tombstones, generations and leases leave with the old clone path (#1121) |
| `clones_at_once_keep_their_own_uppers_and_machine_ids` | rewritten | `test-microvm --scenario snapshot-core`, whose concurrent restores of one snapshot each keep their own upper and machine ID; `test-microvm --scenario scratch-snapshot` |
| `clones_at_once_see_only_their_own_exports` | rewritten | `test-microvm --scenario snapshot-core`, whose concurrent restores of one snapshot each see their own share alone; `test-microvm --scenario platform-aggregate-export` |
| `each_command_reattaches_to_the_clone_from_a_new_process` | rewritten | `nvx-rs/src/host/vm/tests/reattach.rs::reattaching_returns_the_held_owned_child_and_authenticated_connection`; `nvx-rs/tests/vm_lifecycle.rs::a_cold_vm_is_reattached_from_saved_values_and_stopped`. The adapter's probes retire |
| `one_generation_serves_clones_of_each_processor_count_from_its_boot_online_count_to_its_capacity` | rewritten | `test-microvm --scenario restore-processors`; `test-microvm --scenario platform-restore-processors` |
| `one_generation_serves_clones_of_each_upper_and_memory_target` | rewritten | `test-microvm --scenario scratch-snapshot`; `test-microvm --scenario restore-memory`. Leases retire (#1121) |
| `one_generation_serves_clones_of_either_format_over_either_upper` | rewritten | `test-microvm --scenario snapshot-tiers`; `nvx-rs/src/host/snapshot/tests.rs::snapshot_accepts_matching_real_disk_geometry_and_every_resource_boundary` |
| `one_template_serves_the_clones_of_every_network_policy_at_its_guest_s_address` | rewritten | `test-microvm --scenario untiered-network-policy`, whose four restores of one named snapshot with a NIC each install a network policy of their own at the NIC's address; `test-microvm --scenario platform-network-policy` |
| `openvmm_reads_none_of_the_host_s_input_until_the_repair_acknowledges_the_restore` | retired | It watches OpenVMM's own gating of the host's input through the repair delay of the adapter's templates, which leaves with them. NVX's side stays: `nvx-rs/src/guest/activation/state/tests.rs::the_deadline_starts_at_the_acknowledgement_and_retries_never_extend_it`; `nvx-rs/src/guest/init/boot/tests.rs::restored_sessions_keep_the_deadline_of_the_actual_repair_acknowledgement` |
| `restore_exclusion::hard_linked_restore_views_obey_the_callers_exclusion` (linux) | rewritten | `test-microvm --scenario snapshot-core`, whose concurrent restores of one snapshot take turns at the CLI's lock and leave no link to its files. The view that a restore hard-links, which `vm::create` documents: `nvx-rs/src/host/snapshot/tests.rs::a_restore_view_hard_links_only_the_fixed_openvmm_inventory`; `nvx-rs/src/host/vm/tests.rs::snapshot_creation_owns_the_fixed_view_and_uses_captured_nic_with_requested_policy`; `nvx-rs/src/host/vm/tests.rs::snapshot_creation_refuses_a_true_cross_filesystem_view_without_copying`; `nvx-rs/src/host/vm/tests.rs::snapshot_creation_never_replaces_existing_outputs_views_or_listeners`; `nvx-rs/src/host/vm/tests.rs::the_same_prebound_listener_keeps_its_queue_through_both_launch_paths_without_inheritance`. OpenVMM's own checks before its restore-ready connection, which fail a restore whose view changed or whose caller died, and which `vm::create` documents too, leave NVX's tests |
| `the_lease_of_a_start_that_ended_before_its_record_is_released_through_its_template_store` | retired | Leases and clone records leave with the old clone path (#1121) |

## The adapter's other tests

The adapter's library and its targets that `scenarios.txt` didn't name held these tests, which
checked the adapter's own code, and CI ran in its configurations of the adapter.

| File | Tests | Fate | Coverage, or why it retires |
| --- | --- | --- | --- |
| `src/exec/tests.rs` | `starts_round_trip_with_every_unit_flag_and_argument`, `a_start_s_layout_is_its_key_limits_timers_input_and_arguments`, `a_start_s_layout_ends_with_its_identity_working_directory_and_environment`, `starts_that_break_the_rules_are_refused`, `host_messages_round_trip_through_their_frames`, `guest_messages_round_trip_through_their_frames`, `the_longest_chunk_fits_the_framing`, `bodies_that_break_the_rules_are_refused`, `each_side_decodes_only_its_own_messages`, `a_peer_and_a_runner_exchange_their_messages_through_the_frames`, `the_host_s_side_sends_the_cancellation_of_an_execution_that_it_gives_up_on_at_once`, `the_host_s_side_keeps_at_most_max_capture_bytes_of_a_stream_and_cancels_the_rest` | retired | The adapter's framing of executions, over the exec engine that leaves (#1087) |
| `src/frame/tests.rs` | `the_header_holds_the_body_length_then_the_kind`, `each_kind_decodes_from_its_code_alone`, `messages_arrive_whole_in_pieces_of_any_size`, `messages_arrive_whole_across_a_split_at_every_byte`, `a_push_stops_at_the_end_of_a_message`, `malformed_headers_fail_before_their_body_is_taken`, `clearing_drops_a_partial_message`, `encoding_refuses_a_body_beyond_the_limit` | retired | The adapter's message framing over the control session |
| `src/generations/tests.rs` | `an_eviction_retires_the_generations_of_the_served_class_that_it_supersedes`, `an_eviction_retires_the_tombstones_that_interrupted_retirements_left_alone` | retired | Template generations, eviction and tombstones leave with the old clone path (#1121) |
| `src/probe/tests.rs` | `a_probe_is_its_name_then_its_arguments`, `facts_are_one_line_each_with_escaped_values`, `a_probe_names_each_fact_once`, `a_probe_names_its_facts_with_valid_keys` | retired | The adapter's probes |
| `src/root/tests.rs` | `a_configuration_is_one_line_per_field`, `a_configuration_that_breaks_the_rules_is_refused`, `stages_have_codes_names_and_exit_statuses`, `a_failure_is_its_stage_then_its_problem`, `a_readiness_is_the_length_of_its_build_then_the_build`, `a_clone_s_readiness_reports_its_activation_after_the_build`, `a_readiness_that_breaks_the_rules_is_refused`, `a_mapping_table_travels_in_parts_that_the_guest_reads_in_order`, `mapping_tables_and_parts_that_break_the_rules_are_refused`, `a_failure_of_the_mapping_table_is_its_count_then_its_problem` | retired | The adapter's root configuration, readiness, stages and table framing. NVX's own: `nvx-rs/src/guest/mappings/table/tests.rs::parts_apply_in_order_until_the_table_is_complete` and the activation codec's vectors |
| `src/template/tests.rs` | `the_platform_tier_s_allowlist_is_openvmm_s`, `the_platform_tier_s_allowlist_takes_tokens_as_openvmm_splits_them` | rewritten | `nvx-rs/src/host/config/tests.rs::capture_platform_allowlist_matches_openvmms_named_contract` |
| `src/template/tests.rs` | `a_command_line_without_the_platform_tier_boots_cold` | rewritten | `nvx-rs/src/host/config/tests.rs::cold_boot_arguments_select_the_configured_guest_without_managed_tokens` |
| `src/template/tests.rs` | `a_template_s_command_line_holds_the_platform_tier_s_tokens_alone`, `template_mode_refuses_a_token_outside_the_allowlist` | rewritten | `nvx-rs/src/host/config/tests.rs::capture_command_lines_keep_only_unowned_platform_tokens`; `nvx-rs/src/host/config/tests.rs::capture_arguments_accept_platform_tokens_and_refuse_nonplatform_tokens` |
| `src/template/tests.rs` | `template_mode_refuses_a_gateway_that_openvmm_would_not_render` | retired | The adapter's template mode; a NIC's address is checked as `NicConfig` is built |
| `src/template/tests.rs` | `template_mode_refuses_quotes_and_the_delimiter` | rewritten | `nvx-rs/src/host/config/tests.rs::cold_boot_command_lines_refuse_native_control_console_syntax` |
| `src/template/tests.rs` | `a_template_s_statuses_are_its_own`, `a_template_s_repair_delay_is_positive_milliseconds_of_at_most_half_a_minute` | retired | The adapter's template statuses and its clones' repair delay |
| `src/worker/activation/tests.rs` | `each_class_of_failure_ends_the_clone_with_its_own_status`, `the_activation_s_timeout_covers_the_wait_for_both_devices`, `the_activation_probe_reports_the_clone_s_events_after_the_acknowledgement` | retired | The adapter's guest worker. NVX's activation: `nvx-rs/src/guest/activation/state/tests.rs` and nvx-init's `nvx-rs/src/guest/init/tests/restore.rs`, and nvx-init's status of each class of failure: `nvx-rs/src/guest/init/boot/tests.rs::a_failed_activation_ends_the_vm_with_the_status_of_its_class` |
| `src/worker/capture/tests.rs` | `closing_the_fence_acquires_it_until_it_opens`, `each_stage_of_a_capture_ends_the_worker_with_a_status_of_its_own`, `a_request_that_the_time_abi_could_not_repair_ends_with_the_violation_s_status`, `a_template_s_devices_are_those_of_a_root_and_the_host_filesystem`, `a_clone_assembles_its_root_with_the_template_s_configuration`, `the_worker_acknowledges_the_broker_s_reset_before_the_capture_and_reads_nothing_more`, `no_capture_runs_without_the_session_s_handshake` | retired | The adapter's guest worker and its admission fence, which leaves (#1087). NVX's capture: `nvx-rs/src/guest/snapshot/tests.rs` and nvx-init's `nvx-rs/src/guest/init/tests/capture.rs` |
| `src/worker/entries/tests.rs` | `capabilities_are_named_without_their_prefix`, `status_fields_are_found_by_name`, `outcomes_name_the_error`, `refusals_name_what_refused_the_identity`, `unknown_probes_and_arguments_are_refused`, `absolute_paths_lose_their_leading_slash` | retired | The adapter's guest worker |
| `src/worker/executions/tests.rs` | `a_cancellation_after_the_timeout_leaves_a_program_that_ended_before_it_its_own_end` | retired | The adapter's guest worker, over the exec engine that leaves (#1087) |
| `src/worker/mapping/tests.rs` | `refusals_name_their_kind_and_path`, `the_probes_take_named_targets_and_paths_in_the_export` | retired | The adapter's guest worker and its probes |
| `src/worker/probes/tests.rs` | `a_mount_table_is_read_with_its_escapes`, `escapes_are_three_octal_digits`, `process_records_are_read_as_the_kernel_writes_them`, `the_memory_total_is_read_in_bytes`, `the_os_release_id_loses_its_quotes`, `unknown_probes_and_arguments_are_refused`, `the_sectors_read_are_the_third_field_of_a_disk_s_statistics`, `the_time_state_s_lines_are_facts_with_their_keys_in_the_facts_form`, `the_first_partition_is_reported_in_hexadecimal`, `the_cpus_that_a_restore_brought_online_are_those_of_its_record` | retired | The adapter's probes |
| `src/worker/tests.rs` | `announcer_launch_keeps_the_socket_and_monitors_its_owned_child`, `announcer_drop_stops_and_reaps_the_child_that_it_owns`, `announcer_failed_exec_reports_the_cause_and_leaves_no_child` | retired | The adapter's own launcher of a clone's announcer, which SL6.13c added when it deleted `nvx::guest::ingress::Announcer`. `nvx-ingress start`'s launch: `nvx-rs/src/guest/ingress/tests.rs` |
| `src/worker/tests.rs` | `pid_1_goes_on_past_the_stops_of_the_threads_that_it_traces` | retired | The adapter's PID 1. nvx-init's: `nvx-rs/src/guest/init/tests.rs` |
| `tests/guest_session.rs` | `the_guest_agent_answers_messages_that_span_records`, `a_reset_starts_the_new_epoch_with_a_new_message`, `the_guest_agent_ends_with_its_stream`, `the_guest_agent_fails_on_records_that_the_protocol_does_not_allow`, `the_guest_agent_refuses_messages_that_its_framing_does_not_allow`, `the_guest_agent_of_a_vm_without_a_layered_root_answers_probes_with_a_failure`, `the_guest_binary_serves_the_session_on_its_standard_streams`, `the_guest_agent_serves_a_tty_in_raw_mode`, `the_guest_binary_serves_a_tty_in_place_of_the_managed_agent`, `the_worker_assembles_the_root_serves_it_and_tears_it_down_on_stop`, `the_worker_assembles_an_image_over_ram_in_the_overlay_mode_of_its_configuration`, `the_worker_assembles_a_gpt_disk_with_the_first_partition_bound_of_its_configuration`, `the_worker_reports_the_stage_that_failed_and_ends_with_its_status`, `the_worker_checks_the_account_of_the_sandbox_s_workload_identity_in_the_assembled_root`, `the_worker_reports_a_teardown_that_failed_before_it_confirms_the_stop`, `the_worker_accepts_only_its_configuration_or_a_stop_before_its_root`, `a_broker_that_closes_only_its_end_of_the_stream_receives_what_the_worker_queued`, `the_worker_mounts_the_export_applies_the_mapping_table_in_order_and_starts_the_sandbox`, `a_table_of_zero_mappings_does_no_work`, `a_failed_mapping_latches_the_root_and_holds_the_sandbox_back`, `the_worker_refuses_parts_that_do_not_continue_the_table`, `the_worker_refuses_executions_and_the_start_out_of_order`, `the_guest_binary_runs_pid_1_and_the_worker_of_a_layered_root`, `a_test_that_gives_up_on_pid_1_ends_every_process_that_it_started`, `a_test_ends_every_process_that_outlives_pid_1`, `pid_1_collects_what_real_time_signals_leave_and_supervises_on`, `pid_1_finds_the_children_that_real_time_signals_end_whatever_their_names`, `the_worker_runs_executions_at_once_with_frame_counted_credit_and_one_output_pool`, `a_stalled_sink_stalls_only_its_own_execution_while_timers_cancellations_and_control_go_on`, `the_worker_refuses_probes_while_it_runs_executions`, `a_full_stream_to_the_host_holds_back_only_output_while_cancellations_and_stops_go_on`, `executions_that_write_without_end_take_turns_at_the_stream_to_the_host`, `the_worker_refuses_starts_beyond_its_limits_with_a_failed_launch`, `the_worker_ends_its_session_on_messages_about_executions_that_break_their_rules`, `a_reset_or_a_later_generation_abandons_executions_whose_programs_end`, `the_worker_ends_its_executions_when_the_host_stops_it`, `the_worker_collects_the_status_of_a_program_that_outlives_its_execution`, `the_worker_ends_what_is_left_of_the_process_group_of_a_program_that_ended`, `an_execution_s_settlement_ends_a_descendant_that_left_its_process_group`, `an_execution_that_does_not_settle_keeps_its_place_until_the_host_stops_the_worker`, `an_execution_runs_in_its_working_directory_with_exactly_its_environment`, `an_execution_enters_the_root_as_the_identity_that_its_start_names`, `an_execution_enters_the_root_with_the_publication_of_listeners_that_its_configuration_names`, `the_worker_refuses_at_stage_0_a_start_whose_identity_breaks_the_rules`, `the_worker_collects_the_status_of_a_program_that_a_real_time_signal_ended`, `an_execution_that_starts_with_the_root_counts_its_timeout_from_its_own_start`, `a_host_that_a_test_drops_while_its_broker_pauses_ends_the_worker_and_its_programs`, `the_worker_remembers_the_generations_that_the_host_started`, `the_worker_runs_executions_on_a_tty`, `a_clone_answers_the_restore_s_reset_and_activates_with_the_record_of_its_activate`, `a_clone_takes_nothing_but_its_activate_before_it_is_ready`, `a_clone_s_activation_fails_finally_with_the_status_of_the_failure_s_class`, `application_failures_follow_export_gateway_and_view_order`, `a_clone_fails_on_an_activate_longer_than_a_record_before_it_takes_its_body`, `a_clone_that_the_host_does_not_activate_in_time_ends_with_the_deadline_s_status`, `a_clone_s_application_ends_by_the_activation_s_deadline`, `a_clone_takes_what_came_during_its_application_before_it_is_ready`, `a_clone_whose_stream_takes_nothing_still_ends_once_its_activation_failed`, `a_clone_whose_broker_closes_the_stream_before_it_is_ready_is_abandoned`, `a_clone_whose_broker_is_gone_still_ends_as_its_activation_tells`, `a_ready_clone_fails_on_an_activate_that_is_not_a_retry`, `a_clone_s_executions_name_their_own_identity_and_take_the_hostname_of_its_record`, `a_clone_s_executions_enter_the_root_with_the_publication_of_its_record_s_network`, `a_clone_whose_announcer_ends_ends_with_the_network_s_status`, `a_clone_reports_when_its_events_happened_after_the_acknowledgement`, `a_clone_maps_its_record_s_table_from_its_export_and_leaves_the_proxy_to_the_host` | retired | The adapter's guest agent and worker. NVX's guest session, nvx-init and managed agent: `nvx-rs/tests/nvxs/guest.rs`, `nvx-rs/src/guest/init/tests.rs` and its `tests/`, and `nvx-rs/src/guest/managed_agent/**/tests.rs` |
| `tests/independent_build.rs` | `the_adapter_is_a_workspace_of_its_own_with_its_own_lockfile`, `the_adapter_declares_the_minimum_rust_version_of_nvx`, `the_host_binary_reports_the_contract_that_it_was_built_against`, `the_guest_binary_reports_the_contract_that_it_was_built_against` | retired | The adapter's own workspace, manifest and binaries |
| `tests/independent_build.rs` | `the_protocol_only_configuration_depends_on_nvx_alone_without_features`, `the_host_only_configuration_enables_the_host_side_of_nvx_alone`, `the_guest_only_configuration_pulls_in_no_host_or_tokio_dependency` | retired | The features that the adapter's own configurations select of `nvx`, and the dependencies that they pull in, which leave with the adapter, the repository's one crate that depends on `nvx`. CI still builds `nvx` with each feature set alone, warnings denied, in the Lint step of `check-nvx-rs` and its musl check of the guest binaries, but no test checks which dependencies a feature set pulls in |
| `tests/independent_build.rs` | `the_linkage_check_finds_what_needs_a_dynamic_loader`, `the_guest_binary_is_a_static_executable` | rewritten | The static check of NVX's guest binaries: `scripts/test_nvx_tools.py::StaticElfTests`, which `nvx.py build-guest-binaries` runs in CI |
| `tests/initramfs.rs` | `the_archive_replaces_nvx_s_agents_with_links_to_the_guest_agent_launchers`, `a_template_s_archive_holds_the_delay_of_its_clones_repair_after_the_launchers`, `the_launchers_run_the_guest_agent_on_the_control_tty`, `the_initramfs_is_a_copy_of_nvx_s_with_the_archive_at_a_multiple_of_4_bytes` | retired | The adapter's initramfs, which runs its guest agent in place of NVX's agents |
| `tests/listener_client.rs` | `a_published_listener_exchanges_its_greeting`, `a_connection_reset_before_inspection_is_retried`, `a_self_connection_does_not_hold_the_publication_port` | retired | The adapter's client of published listeners, for its VM tests. Publication: `test-microvm --scenario host-loopback-policy` |
| `tests/runtime.rs` | `the_runtime_identity_is_nvx_s_with_the_guest_agent_s_build_in_either_layout`, `another_build_of_the_guest_agent_is_another_runtime` | retired | The adapter's guest agent's build in its runtime identity |
| `tests/runtime.rs` | `missing_install_metadata_fails_as_nvx_reports_it` | rewritten | `nvx-rs/src/host/artifacts/tests.rs::a_checkout_identifies_its_runtime_by_the_build_provenance_of_its_artifacts`; `nvx-rs/src/host/artifacts/tests.rs::a_release_identifies_its_runtime_by_its_checksum_list` |
| `tests/sandbox_names.rs` | `a_process_never_reuses_the_name_of_a_dropped_sandbox` | retired | The adapter's sandboxes for its VM tests |
| `tests/step_hook.rs` | `a_step_hook_names_a_step_and_what_it_does_there`, `a_pause_waits_until_its_file_is_gone`, `a_pause_waits_until_the_deletion_of_its_file_completes`, `the_host_program_refuses_a_command_whose_step_hook_it_cannot_read`, `a_start_takes_a_step_hook_at_checked_alone` | retired | The adapter's step hook |

## MicroVM scenarios

`nvx.py test-microvm` runs these scenarios, which use no adapter: CI runs those of the default
suite in its shards, and the public sandbox scenarios in its guest and sandbox jobs. The public
`sandbox-gpt-layer` and `vm-lifecycle` scenarios came with SL5.1b, after this matrix;
[Cold boots](#cold-boots) says what they check. SL5.1c rewrote `snapshot-core` and
`snapshot-tiers` as public scenarios, and SL3.7c `untiered-network-policy`;
[Snapshots](#snapshots) says what they check.

| Scenario | Fate | Note |
| --- | --- | --- |
| `command-line-limit` | kept |  |
| `console-exit` | kept |  |
| `console-snapshot` | kept |  |
| `cpu-profile-fallback` | kept |  |
| `denied-filesystem-paths` | kept |  |
| `directional-network-policy` | kept |  |
| `endpoint-policy-snapshot` | kept |  |
| `filesystem-comma-paths` | kept |  |
| `filesystem-owner` | kept |  |
| `filesystem-policy` | kept |  |
| `filesystem-shares` | kept |  |
| `filesystem-snapshot` | kept |  |
| `guest-boot` | kept |  |
| `guest-identity` | kept |  |
| `host-loopback-policy` | kept |  |
| `interrupt-map` | kept |  |
| `ipv6-egress-policy` | kept |  |
| `l3-l4-egress-policy` | kept |  |
| `l3-l4-egress-port-ranges` | kept |  |
| `l3-l4-egress-protocols` | kept |  |
| `lifecycle` | kept |  |
| `managed-exec-config` | kept | A [cold cell](#cold-boots): the EROFS layer over a scratch upper, through `provision` and `start` |
| `managed-lifecycle` | kept | It runs the ignored unit test of the host's private control session in `nvx-rs/src/host/session/tests/managed_lifecycle.rs`, with the `cli` feature, which SL6.12 (#1127) moved there from the crate's integration tests |
| `network-snapshot` | kept |  |
| `platform-aggregate-export` | kept |  |
| `platform-filesystem` | kept |  |
| `platform-network-policy` | kept |  |
| `platform-restore-processors` | kept |  |
| `platform-zero-fill` | kept |  |
| `private-network-ingress` | kept |  |
| `restore-downtime` | kept |  |
| `restore-memory` | kept |  |
| `restore-processors` | kept |  |
| `run-console-restore` | retired | SL3.8b removes `nvx-rs run`'s restore of a raw OpenVMM snapshot and the boot console that it reattached on the terminal: `run --restore-snapshot` restores a named snapshot through `snapshot restore`'s path |
| `sandbox-blocks` | kept |  |
| `sandbox-gpt-layer` | kept | The [cold cells](#cold-boots) of a GPT disk, over a scratch upper and over a RAM upper, through `run`, and `provision` and `start` |
| `sandbox-lifecycle` | kept | A [cold cell](#cold-boots): the EROFS layer over a scratch upper, through `provision` and `start`, which also reattach to the VM from a new process for each command. Since SL4.4c, its request that writes 2 MiB checks that `exec` streams all of it, where it checked NVXC version 1's 1 MiB bound |
| `sandbox-proxy-only` | kept | Its proxy-only network, over the EROFS layer and a scratch upper; it is no [cold cell](#cold-boots) of its own |
| `sandbox-ram-upper` | kept | A [cold cell](#cold-boots): the EROFS layer over a RAM upper, through `run`, and `provision` and `start`. Since SL3.10c, it checks that `provision` records the configuration's format 10 |
| `scratch-snapshot` | kept |  |
| `smp` | kept |  |
| `smp-lapic` | kept | Runs only when a run names it |
| `smp-snapshot` | kept |  |
| `snapshot-core` | rewritten | On a named snapshot, as [Snapshots](#snapshots) says. Its checks of raw snapshots retire: the guest's capture with `nvx-snapshot` and its terminal boundary; a snapshot request without a destination, which OpenVMM releases, and which no scenario sends any longer, though `snapshot-rollback` keeps the checks of the source after a released request, which follows the capture that it rolls back; the CPU clocks of a sleeping task, an armed timer's wait, the restore packet's entropy file and `nvx-reseed`, the VM generation ID, and the time ABI's port access. It had no network |
| `snapshot-rollback` | kept |  |
| `snapshot-tiers` | rewritten | On a named snapshot of the platform tier, as [Snapshots](#snapshots) says. Its checks of raw snapshots retire: the workload-start and instance-checkpoint tiers, a restore of the wrong tier, the restore's input gate and its timeout, a restore of an instance checkpoint without `--machine microvm`, and a second claim of one, and its run on the debug kernel. It checked nothing of its NIC |
| `structured-outcome` | kept |  |
| `time-abi-conformance` | kept |  |
| `time-host-correction` | kept |  |
| `untiered-network-policy` | rewritten | On named snapshots with a NIC, which `snapshot capture --net` captures, as [Snapshots](#snapshots) says, keeping its checks of a restore's DNS, egress, forwards and listener publication. Its raw-snapshot parts retire: the raw capture and restores, the guest's repair marker, the console release, and OpenVMM's refusals of raw restores that the CLI cannot issue, in whose place the CLI must refuse a restore without `--network-profile`, and one with `--net` |
| `virtio-features` | kept |  |
| `virtio-net` | kept |  |
| `vm-lifecycle` | kept | A [cold boot](#cold-boots) through the public API: `nvx-rs/tests/vm_lifecycle.rs::a_cold_vm_is_reattached_from_saved_values_and_stopped`. Since SL4.4d, it pings and stops nvx-init with NVXC version 2's frames, where it sent version 1's |
| `workload-identity` | kept | Since SL6.3 (#1122), it also checks that a guest given OpenVMM's workload identity without `nvx_identity=`, or with another one, fails closed |

### Cold boots

The CLI's twelve cold cells are the two image formats, an EROFS layer and a GPT disk, each over
each upper, a scratch image and RAM, on each of KVM, MSHV and WHP: CI's guest and sandbox job of
each backend runs the public scenarios and the steps that the table names. A cell passes once
`sandbox run`, and a managed sandbox's `provision` and `start`, each boot its image over its upper
in a VM that nvx-init manages, and `/sbin/nvx-sandbox-smoke` passes its checks there. The
scenarios also check that each of their VMs stops with a successful outcome report and a complete
teardown; CI's layer smoke test checks the status of the probe that its run runs.

| Image | Upper | `run` | `provision` and `start` |
| --- | --- | --- | --- |
| EROFS | scratch | CI's "Run Ubuntu sandbox layer smoke test" step, on Linux and on Windows | `test-microvm --scenario sandbox-lifecycle`; `test-microvm --scenario managed-exec-config` |
| EROFS | RAM | `test-microvm --scenario sandbox-ram-upper` | `test-microvm --scenario sandbox-ram-upper` |
| GPT | scratch | `test-microvm --scenario sandbox-gpt-layer` | `test-microvm --scenario sandbox-gpt-layer` |
| GPT | RAM | `test-microvm --scenario sandbox-gpt-layer` | `test-microvm --scenario sandbox-gpt-layer` |

`sandbox-gpt-layer` writes its own GPT disk, whose layer partition a guest fills with an ext4 copy
of the tree of the EROFS layer, and checks that the guest's root has that partition as its lower
directory. `test-microvm --scenario vm-lifecycle` cold-boots a VM through the public API instead:
`nvx-rs/tests/vm_lifecycle.rs::a_cold_vm_is_reattached_from_saved_values_and_stopped` creates and
spawns it, with nvx-init over the EROFS layer and a scratch upper, lets the launch go, reattaches
to it twice from the values that it saved, and stops it through its guest, which must exit without
a forced termination and report a complete teardown. No cell captures or restores a snapshot:
[Snapshots](#snapshots) lists the smoke tests that do.

### Snapshots

`test-microvm --scenario snapshot-core` and `test-microvm --scenario snapshot-tiers`, which CI's
guest and sandbox job of each backend runs, capture a named snapshot with `nvx-rs snapshot
capture`, whose VM nvx-init runs, and restore it with `nvx-rs snapshot restore` and `nvx-rs run
--restore-snapshot`, which open it as a strict snapshot under the CLI's lock of the snapshot, as
[Run](run.md#named-snapshots) describes.

| Smoke test | Where |
| --- | --- |
| A real VM is captured, opened as a strict snapshot and restored with nvx-init | `snapshot-core`: two one-off restores and three managed ones, whose guests keep the snapshot's uptime, read the host's clock, and take fresh random values and machine IDs. `snapshot-tiers`: a managed restore of each image format, EROFS and GPT, over each upper, scratch and RAM |
| Concurrent restores of one snapshot are serialized by the CLI's lock | `snapshot-core`: three restores at once, whose workloads end only when the scenario releases them. While one holds the lock, no other links its view of the snapshot, and a restore that cannot wait for the lock fails with `sandbox-busy` before its workload runs. The three then run at once, each with its own share, upper and machine ID |
| The selected failed restore reports its attribution | `snapshot-core`: a scratch image that the snapshot cannot serve is refused before any VM starts, with an error that names the lengths of the requested scratch and of the snapshot's, which attributes the failure to the request; the managed sandbox of the refused restore holds its configuration and its lock alone, with no trace of a launch. A restore over a zeroed scratch image fails in its guest, which cannot mount the image as its upper: its VM ends with nvx-init's status of an unusable root, 54, and frees the lock, and the CLI reports `guest-exit status 54` with the restore's evidence, `UnusableRoot`, which it attributes to the request |
| A restore of a snapshot with a NIC installs a network policy of its own | `untiered-network-policy`: a snapshot that `snapshot capture --net` captures under the complete deny-all policy, restored four times with `snapshot restore` and `run --restore-snapshot`. Each guest names exactly its policy's DNS server and runs the announcer of its listeners only when its policy publishes them, reaches exactly the host services that its egress admits, and serves the host through a forward and through the port that its policy publishes, on host loopback alone or to the host's private networks. The CLI refuses a restore without `--network-profile`, and one with `--net`, before it provisions anything |

A snapshot that `snapshot capture` captures without `--net` has no NIC, so its restores take no
network option; one captured with `--net` keeps its NIC, and each of its restores installs the
network policy of its own options on it.

## CI

These are the steps of CI's workflows and actions that check something, and the Python tests of
CI's wiring that named the adapter. Builds, downloads, uploads and publications check nothing of
their own. The tests that a step runs have rows of their own where they concern the adapter.

| Where | Case | Fate | Note |
| --- | --- | --- | --- |
| `.github/actions/check-quality` | Check code quality | kept |  |
| `.github/actions/check-quality` | Check Markdown links | kept |  |
| `.github/actions/check-quality` | Check test layout | kept |  |
| `.github/actions/check-quality` | Check the kernel configuration against its feature inventory | kept |  |
| `.github/actions/check-quality` | Test Specula CI wiring | kept |  |
| `.github/actions/check-quality` | Check PowerShell syntax | kept |  |
| `.github/actions/check-nvx-rs` | Check formatting | kept |  |
| `.github/actions/check-nvx-rs` | Lint | kept | Builds `nvx` with each feature set, warnings denied: all features, `host` (the default), no default features, and `serde`, `cli` and `testing` alone, which checks `serde` alone since this matrix |
| `.github/actions/check-nvx-rs` | Test | kept | Runs `nvx-rs/tests/security_checklist.rs` too |
| `.github/actions/check-nvx-rs` | Check artifact bundling | retired | The `bundled` feature, which staged a release at build time, leaves NVX with its build script and `artifacts.json` (SL6.6a, #1122) |
| `.github/actions/check-nvx-rs` | Build documentation | kept |  |
| `.github/actions/check-nvx-rs` | Check the public API | kept |  |
| `.github/actions/check-nvx-rs` | Check the minimum supported Rust version | kept |  |
| `.github/actions/check-nvx-rs` | Check the macOS build | kept |  |
| `.github/actions/check-nvx-rs` | Lint and test the guest binaries | kept | Builds `nvx` with `guest` alone, warnings denied, `nvx-rs/examples/guest-workflows.rs` among its targets |
| `.github/actions/check-nvx-rs` | Build and check the guest binaries | kept |  |
| `.github/actions/check-nvx-rs` | Check the runtime adapter's protocol-only configuration | rewritten | The Lint step's builds without default features and with `serde` alone, and the public API's snapshots |
| `.github/actions/check-nvx-rs` | Check the runtime adapter's host-only configuration | rewritten | The Lint step's build of `host`, `nvx-rs/examples/host-workflows.rs`, and `nvx-rs/tests/security_checklist.rs` |
| `.github/actions/check-nvx-rs` | Check the runtime adapter's guest-only configuration | rewritten | The musl check of the guest binaries, which builds `guest` alone with `nvx-rs/examples/guest-workflows.rs`, and their static check |
| `.github/actions/validate-nvx` | Validate NVX CLI on Linux and on Windows: the Python tests | kept | Their tests of the adapter's runner and wiring are below |
| `.github/actions/validate-nvx` | Validate NVX CLI on Linux and on Windows: `nvx.py test-runtime-adapter --help` | retired | The adapter's runner |
| `.github/actions/validate-nvx` | Validate NVX CLI on Linux and on Windows: the other `--help` checks | kept |  |
| `.github/workflows/ci.yml` | nvx-cli-linux, nvx-cli-windows: Compare OpenVMM's protocol vectors | kept |  |
| `.github/workflows/ci.yml` | nvx-cli-linux, nvx-cli-windows: Compare OpenVMM's CPU profiles | kept |  |
| `.github/workflows/ci.yml` | guest-artifact-reuse: the two artifact reuse rehearsals | kept |  |
| `.github/workflows/ci.yml` | openvmm-ci: Check OpenVMM's CI of the pinned revision | kept |  |
| `.github/workflows/ci.yml` | performance-gate: Gate on performance regressions | kept |  |
| `.github/workflows/ci.yml` | required-status-check: Verify required jobs | kept | SL5.2 removes the adapter's job from what it requires |
| `.github/workflows/ci.yml` | crate-package: Package, check, and build the nvx crate | kept | Since SL6.6a, the package holds no build script or `artifacts.json`, and builds once, with every feature |
| `.github/workflows/ci.yml` | crate-release: Check the nvx package | kept | Since SL6.6a, the package holds no build script or `artifacts.json` |
| `.github/workflows/ci.yml` | nvx-microvm-adapter-whp: the runtime adapter's job on WHP | retired | The adapter's scenarios, whose fates are above |
| `.github/workflows/run-nvx-microvm-tests.yml` | Run NVX microVM tests on Linux and on Windows | kept | The default suite's scenarios, by shard and on the debug kernel |
| `.github/workflows/run-nvx-microvm-tests.yml` | Run public managed sandbox acceptance on Linux and on Windows | kept | Runs `sandbox-gpt-layer` and `vm-lifecycle` too, since SL5.1b: see [Cold boots](#cold-boots); and `snapshot-core` and `snapshot-tiers`, since SL5.1c: see [Snapshots](#snapshots) |
| `.github/workflows/run-nvx-microvm-tests.yml` | Run Ubuntu and Azure Linux guest smoke tests on Linux and on Windows | kept |  |
| `.github/workflows/run-nvx-microvm-tests.yml` | Run extended Ubuntu guest tests on Linux/KVM | kept | No longer runs `snapshot-core`, which needs the Alpine control guest since SL5.1c rewrote it |
| `.github/workflows/run-nvx-microvm-tests.yml` | Run the Ubuntu sandbox layer, live-share, managed live-share, caller-owned live-share, concurrent shares and share access policy tests on Linux and on Windows | kept | The layer smoke test is the `run` of a [cold cell](#cold-boots). Since SL3.10c, the share tests check that `provision` records the configuration's format 10 |
| `.github/workflows/run-nvx-microvm-tests.yml` | Build the runtime adapter's guest agent on Linux; Download the runtime adapter's guest agent on Windows | retired | The adapter's guest agent |
| `.github/workflows/run-nvx-microvm-tests.yml` | Run runtime adapter scenarios on Linux and on Windows | retired | The adapter's scenarios, whose fates are above |
| `.github/workflows/run-platform.yml` | Run benchmark: the acceptance test, the performance suite, the multi-vCPU shell restore and the device operation rates | kept |  |
| `.github/actions/validate-runner` | Validate KVM, Validate MSHV, and Qualify host time | kept |  |
| `.github/workflows/test-changes.yml` | Flag changes to existing tests | kept |  |
| `.github/workflows/nvx-rs-mutants.yml` | Run cargo-mutants | kept |  |
| `.github/workflows/adversarial.yml` | Run bounded adversarial campaign | kept |  |

| Python tests | Fate | Note |
| --- | --- | --- |
| `scripts/test_nvx_tools.py::RuntimeAdapterSuiteTests`: `test_cli_registers_the_suite`, `test_ci_runs_the_suite_by_name_on_each_backend`, `test_scenarios_name_targets_and_tests_in_the_order_of_their_lines`, `test_scenarios_refuse_other_lines`, `test_scenarios_select_the_host_platform_in_line_order`, `test_the_adapter_s_readme_lists_its_scenarios`, `test_libtest_s_terse_list_names_the_tests`, `test_naming_problems_name_missing_and_unnamed_tests`, `test_a_run_passes_once_each_scenario_ran_and_passed`, `test_a_run_fails_once_a_scenario_failed_or_did_not_run`, `test_the_handler_checks_the_names_then_runs_each_target_by_name`, `test_the_handler_takes_a_relative_directory_of_artifacts_from_the_caller`, `test_the_handler_checks_and_runs_the_same_platform_scenarios`, `test_platform_selection_keeps_unlisted_and_ignored_test_errors`, `test_the_handler_runs_nothing_once_the_names_are_not_the_tests`, `test_the_handler_refuses_an_adapter_that_cargo_does_not_describe` | retired | The adapter's runner, `scripts/nvx_tools/runtime_adapter_tests.py`, and its `nvx.py` command |
| `scripts/test_nvx_tools.py::CiConfigurationTests`: `test_nvx_rs_checks_the_runtime_adapter_in_each_configuration`, `test_ci_change_classification_runs_microvm_tests_for_the_runtime_adapter`, `test_runtime_adapter_scenarios_take_a_guest_agent_that_linux_builds` | retired | The adapter's CI wiring |
| `scripts/test_nvx_tools.py::CiConfigurationTests`: `test_ci_jobs_download_exactly_the_guest_artifacts_that_they_read`, `test_ci_change_classification_selects_suites_by_input`, `test_ci_change_classification_covers_each_suite_input`, `test_ci_shares_openvmm_inputs_and_binary_artifacts`, `test_ci_runs_the_steps_after_the_default_microvm_suite_beside_it`, `test_every_microvm_job_uploads_its_scenario_durations` | kept | SL5.2 removes the adapter's parts of what they check |
| `scripts/test_test_changes.py::ClassifyTests`: `test_paths_and_their_categories` | kept | SL5.2 removes its adapter path |

## Docs

Rustdoc checks the code of the crate's documentation, which these rows name as rustdoc does, by
file and line: it compiles each, runs those that aren't `no_run`, and checks that those marked
`compile_fail` don't compile. The examples compile with the crate's other targets;
`host-workflows` and `guest-workflows`, which came with this matrix, are new, and have no row.
The code of `guest` and `guest::snapshot`, which the guest-agent guidelines quote, came after
this matrix. The README's own code is no doctest.

| Code | Fate | Note |
| --- | --- | --- |
| `nvx-rs/src/activation/mod.rs - activation (line 130)` | kept |  |
| `nvx-rs/src/exec/mod.rs - exec (line 113)` | retired | The exec engine leaves (SL6.11a) |
| `nvx-rs/src/gpt/mod.rs - gpt (line 43)` | kept |  |
| `nvx-rs/src/guest/activation.rs - guest::activation (line 127)` | kept | SL6.11c rewrote it on `guest::snapshot::capture`, when its capture preparation and admission fence left (#1087) |
| `nvx-rs/src/guest/activation/state.rs - guest::activation::state::Activation (line 54)` | kept | SL6.13b (#1127) rewrote it without the guest's space, which became private: its agent applies the record in its own space |
| `nvx-rs/src/guest/cgroup.rs - guest::cgroup (line 36)` | retired | `guest::cgroup` leaves (SL6.11b) |
| `nvx-rs/src/guest/entry.rs - guest::entry (line 90)` | retired | `guest::entry` leaves (SL6.11b) |
| `nvx-rs/src/guest/exec.rs - guest::exec (line 79)` | retired | `guest::exec` leaves (SL6.11b) |
| `nvx-rs/src/guest/exec.rs - guest::exec::Execution::adopt (line 619)` | retired | `guest::exec` leaves (SL6.11b) |
| `nvx-rs/src/guest/gpt.rs - guest::gpt (line 18)` | kept | SL6.13b (#1127) moved it to the module's unit tests, `nvx-rs/src/guest/gpt/tests.rs::a_disk_s_steps_compile`, since the guest's space became private |
| `nvx-rs/src/guest/mappings/mapper.rs - guest::mappings::mapper (line 107)` | kept | SL6.11d (#1123) moved it to the mapper's unit tests, `nvx-rs/src/guest/mappings/mapper/tests.rs::a_mapper_s_steps_compile`, which map and unmount through the seams that NVX's programs use, since the mapper's `apply` and `teardown`, which only tests used, left |
| `nvx-rs/src/guest/mappings/table.rs - guest::mappings::table::MappingTable (line 46)` | kept |  |
| `nvx-rs/src/guest/mod.rs - guest (line 33)` | kept | The guest-agent guidelines quote it |
| `nvx-rs/src/guest/nvxs.rs - guest::nvxs (line 22)` | kept |  |
| `nvx-rs/src/guest/process.rs - guest::process (line 12)` | retired | `guest::process` leaves (SL6.11b) |
| `nvx-rs/src/guest/process.rs - guest::process (line 39)` | retired | `guest::process` leaves (SL6.11b) |
| `nvx-rs/src/guest/reseed.rs - guest::reseed (line 17)` | kept |  |
| `nvx-rs/src/guest/root.rs - guest::root (line 126)` | kept | SL6.11b removed its bootstrap, which left with the cgroups. SL6.11d (#1123) moved it to the root's unit tests, `nvx-rs/src/guest/root/tests.rs::a_layered_root_s_steps_compile`, without the first partition's bytes, the layers and `Root::map`, which only tests used, and which left |
| `nvx-rs/src/guest/snapshot.rs - guest::snapshot (line 25)` | kept | The guest-agent guidelines quote it |
| `nvx-rs/src/guest/spawn.rs - guest::spawn (line 41)` | retired | `guest::spawn` leaves (SL6.11b) |
| `nvx-rs/src/guest/spawn.rs - guest::spawn::LaunchStage::from_code (line 201)` | retired | `LaunchStage::from_code` and its test, `launch_stages_keep_their_numbers`, leave with `guest::spawn` (SL6.11b). The stages of nvx-init's executions keep their numbers in the crate's own fork boundary, whose launch report `nvx-rs/src/guest/init/tests/exec.rs::native_workload_cwd_accepts_procfs_links_but_closes_inherited_descriptors` reads from a real launch |
| `nvx-rs/src/guest/terminal.rs - guest::terminal (line 10)` | kept |  |
| `nvx-rs/src/guest/time/mod.rs - guest::time (line 9)` | kept | SL6.11c removed its capture and restore finish, which became crate-private: `guest::snapshot::capture` runs them |
| `nvx-rs/src/host/sandbox/prepared.rs - host::sandbox::prepared::PreparedLaunch (line 92)` | retired | `PreparedLaunch` leaves with the checked-file handoff (SL6.3, #1122) |
| `nvx-rs/src/host/store/mod.rs - host::store (line 72)` | retired | The sandbox store leaves (SL6.1, #1121) |
| `nvx-rs/src/host/supervisor/mod.rs - host::supervisor (line 32)` | retired | The store-coupled supervisor leaves with the sandbox store (SL6.1, #1121), and `host::supervisor`, the transitional public path of the low-level spawn, with SL6.3 (#1122): `host::vm` spawns VMMs, and the tests of the spawn moved to `nvx-rs/src/host/spawn/tests.rs` and `nvx-rs/src/host/process/tests/orchestrator.rs` |
| `nvx-rs/src/host/template/clone.rs - host::template::clone::ClonePlan (line 247)` | retired | `ClonePlan` and `CloneRequest` leave with the old clone path (SL6.2b, #1121) |
| `nvx-rs/src/host/template/plan.rs - host::template::plan::TemplatePlan (line 137)` | retired | `TemplatePlan` leaves with the template store (SL6.2a, #1121) |
| `nvx-rs/src/host/template/store.rs - host::template::store::Store (line 473)` | retired | The template store leaves (SL6.2a, #1121) |
| `nvx-rs/src/host/transport.rs - host::transport (line 22)` | kept |  |
| `nvx-rs/src/lib.rs - (line 58)` | rewritten | Its `Instance` and sandbox store leave (SL6.1, #1121): `nvx-rs/src/lib.rs - (line 67)` compiles a cold boot and a stop through `host::vm` |
| `nvx-rs/src/protocol/nvxs/mod.rs - protocol::nvxs (line 68)` | kept |  |
| `nvx-rs/src/host/snapshot/capture.rs - host::snapshot::capture::finish_capture (line 48)` | kept |  |
| `nvx-rs/src/host/store/mod.rs - host::store (line 104)` | retired | The sandbox store leaves (SL6.1, #1121) |
| `nvx-rs/README.md`, its Example: an instance through `LaunchSpec` and the sandbox store | rewritten | Nothing compiles it. `nvx-rs/examples/host-workflows.rs` compiles the documented host workflows, and the library's guide (SL6.10a) replaces the example. SL6.1 (#1121) made it the cold boot that `nvx-rs/src/lib.rs - (line 67)` compiles |
| `nvx-rs/examples/external-launch.rs` | kept | Compiled with the crate's targets, and run by hand on a host. Since SL6.3 (#1122), which deletes `host::supervisor`, a minimal launcher of its own on `std::process` starts its VMM |
