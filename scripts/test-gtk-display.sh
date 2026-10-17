#!/usr/bin/env bash
# Run the GTK/VTE regressions that are intentionally excluded from the
# headless test suite. Each test gets a fresh process, display, and session bus:
# GTK can only be initialized once reliably in a test process, while sweeping
# every ignored test would also execute manual micro-benchmarks.

set -Eeuo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
cd "${PROJECT_ROOT}"

# A developer's font scale, density, history, or theme must not change the
# geometry asserted by these regressions. Give the suite an empty, private XDG
# tree just as CI gets from a fresh account; Cargo/Nix keep their own homes.
test_xdg_root="$(mktemp -d)"
cleanup() {
    rm -rf -- "${test_xdg_root}"
}
trap cleanup EXIT
mkdir -p \
    "${test_xdg_root}/config" \
    "${test_xdg_root}/data" \
    "${test_xdg_root}/state" \
    "${test_xdg_root}/cache" \
    "${test_xdg_root}/runtime"
chmod 700 "${test_xdg_root}/runtime"
export XDG_CONFIG_HOME="${test_xdg_root}/config"
export XDG_DATA_HOME="${test_xdg_root}/data"
export XDG_STATE_HOME="${test_xdg_root}/state"
export XDG_CACHE_HOME="${test_xdg_root}/cache"
export XDG_RUNTIME_DIR="${test_xdg_root}/runtime"

for command in dbus-run-session xvfb-run xdotool; do
    if ! command -v "${command}" >/dev/null 2>&1; then
        printf 'Error: %s is required for display-backed tests.\n' "${command}" >&2
        exit 1
    fi
done

# Nix's dbus-daemon keeps its session policy in the store instead of `/etc`.
# Pass the policy beside the selected daemon when available; distro installs
# continue to use their ordinary `/etc/dbus-1/session.conf` fallback.
dbus_run_args=()
dbus_daemon="$(command -v dbus-daemon)"
dbus_prefix="${dbus_daemon%/bin/dbus-daemon}"
dbus_store_config="${dbus_prefix}/share/dbus-1/session.conf"
if [[ -r "${dbus_store_config}" ]]; then
    dbus_run_args+=("--config-file=${dbus_store_config}")
elif [[ ! -r /etc/dbus-1/session.conf ]]; then
    printf 'Error: no D-Bus session configuration is readable.\n' >&2
    exit 1
fi

tests=(
    agent::capability_display_tests::capability_reason_label_tooltip_and_repeated_transitions
    vte_pty::tests::native_bridge_pairs_remain_raw_and_do_not_leak_across_exec
    terminal::block::tests::session_export_preview_cancel_and_explicit_original_are_safe
    terminal::block::tests::session_export_slow_fsync_keeps_native_input_and_tabs_responsive
    dialogs::tasks_panel::tests::queued_task_controls_keep_the_rendered_task_across_sync
    block_view::incremental_find_tests::block_native_find_controls_keep_query_caret_and_tab_focus
    block_view::incremental_find_tests::chat_native_find_controls_keep_query_caret_and_tab_focus
    block_view::readline_preservation_tests::search_and_native_split_preserve_readline_draft_cursor_and_undo
    agent_task_ui::feedback_tests::task_feedback_keeps_input_draft_and_focus
    block_view::ime_gate::tests::preedit_lifecycle_tracks_each_live_context_without_text
    block_view::more_menu_tests::more_opens_complete_menu_preserves_selection_and_retires_stale_cards
    block_view::blocks::command_reuse_labels_tests::recall_header_describes_insert_only_action
    block_view::shortcut_hints::tests::menu_hints_follow_current_mapping_without_dispatch
    block_view::blocks::fold_preview_tests::folded_preview_tracks_filters_and_preserves_output
    block_view::chat::tests::live_font_updates_align_chat_block_and_window_text
    font::tests::window_text_tracks_terminal_size_on_startup_and_live_updates
    font::tests::ui_font_group_is_independent_on_live_native_widgets
    dialogs::tasks_panel::tests::dashboard_filters_preserve_selection_without_retargeting
    dialogs::tasks_panel::tests::dashboard_search_activate_selects_current_visible_result_only
    dialogs::tasks_panel::tests::dashboard_filter_counts_follow_search_not_selected_view
    dialogs::tasks_panel::tests::task_terminal_buttons_only_emit_focus_for_existing_bindings
    dialogs::tasks_panel::tests::task_reason_updates_and_disappears_without_retargeting
    dialogs::tasks_panel::stream_tests::stream_paging_preserves_current_and_copies_only_visible_history
    dialogs::tasks_panel::keyed_tests::keyed_rows_update_only_changed_widgets_and_prune_archives
    dialogs::tasks_panel::keyed_tests::keyed_selected_row_survives_move_hide_and_reveal
    dialogs::tasks_panel::keyed_tests::keyed_selection_guard_survives_synchronous_callbacks
    dialogs::tasks_panel::keyed_tests::keyed_task_list_mixed_update_benchmark
    dialogs::tasks_panel::keyed_tests::keyed_tasks_keep_focus_and_route_real_keyboard
    terminal::vte::tests::explicit_focus_return_respects_reopened_search
    block_view::fold_geometry_tests::fold_actions_update_visible_and_parked_geometry
    block_view::search_navigation_tests::block_output_search_unfolds_only_target
    block_view::search_navigation_tests::chat_output_search_unfolds_only_target
    block_view::search_navigation_tests::block_live_find_excludes_retained_scrollback
    block_view::search_navigation_tests::chat_live_find_excludes_retained_scrollback
    block_view::search_navigation_tests::block_live_find_rebuilds_after_native_changes
    block_view::search_navigation_tests::chat_live_find_rebuilds_after_native_changes
    block_view::incremental_find_tests::block_incremental_find_reveals_exact_occurrence
    block_view::incremental_find_tests::chat_incremental_find_reveals_exact_occurrence
    block_view::incremental_find_tests::block_filtered_single_hit_reopens_without_taking_focus
    block_view::incremental_find_tests::chat_filtered_single_hit_reopens_without_taking_focus
    block_view::incremental_find_tests::block_resized_reveal_restores_last_filtered_occurrence
    block_view::incremental_find_tests::chat_resized_reveal_restores_last_filtered_occurrence
    block_view::incremental_find_tests::block_pending_replay_tracks_latest_navigation
    block_view::incremental_find_tests::chat_pending_replay_tracks_latest_navigation
    block_view::incremental_find_tests::block_pending_replay_retires_after_interruption
    block_view::incremental_find_tests::chat_pending_replay_retires_after_interruption
    block_view::incremental_find_tests::block_large_replay_preserves_narrowed_native_cursor
    block_view::incremental_find_tests::chat_large_replay_preserves_narrowed_native_cursor
    block_view::blocks::tests::live_clip_reserves_css_insets_without_an_extra_grid_row
    block_view::paste_feedback::tests::notice_preserves_focus
    block_view::tests::ssh_remote_prompt_cursor_stays_inside_live_clip_without_clear
    block_view::chat::tests::native_ime_commit_cancel_and_independent_ptys
    block_view::chat::tests::native_ime_send_button_preserves_composition
    block_view::focus_restore::tests::deferred_pty_focus_respects_newer_owners
    block_view::focus_restore::tests::deferred_focus_preserves_remaps_and_retires_hidden_targets
    block_view::focus_restore::tests::popover_focus_return_retires_on_newer_owner
    organism_ui::tests::idle_refresh_skips_unchanged_tooltip_and_non_owner_spatial_render
    organism_ui::tests::hidden_sticky_heartbeat_catches_up_on_map_without_retaining_pane
    block_view::chat::tests::chat_reparents_only_live_clip_and_hides_submit_for_native_takeover
    block_view::chat::tests::chat_raw_takeover_updates_columns_without_resizing_the_window
    block_view::chat::tests::chat_submit_uses_one_native_commit_and_refuses_a_stale_button
    block_view::chat::tests::chat_buttons_preserve_input_and_feedback_when_writer_refuses
    block_view::chat::tests::chat_stop_is_bound_to_displayed_command_and_foreground
    block_view::chat::tests::chat_buttons_never_queue_behind_inflight_keyboard_input
    block_view::chat::tests::chat_native_activation_never_rebinds_a_delayed_stop
    block_view::chat::tests::chat_native_activation_never_rebinds_send_to_a_new_prompt
    font::tests::context_menu_tracks_live_theme_and_font_without_parent_emphasis
    dialogs::settings::organism_preview::tests::preview_lifecycle_keeps_motion_local_and_stops_hidden_sources
    organism_ui::tests::deferred_agent_loss_retires_closed_panes_but_keeps_hidden_outcomes
    organism_ui::tests::live_settings_reuse_body_preserve_focus_and_stop_disabled_timers
    organism_ui::tests::agent_signal_uses_window_setting_when_one_native_pane_is_disabled
    block_view::history::tests::block_history_retry_handles_failed_and_repaired_files_without_panicking
    sidebar::tests::long_breadcrumbs_do_not_expand_the_sidebar_minimum_width
    block_view::history::tests::unified_relative_path_authority_does_not_follow_a_cwd_change
    block_view::history::tests::unified_failed_restore_preserves_original_bytes_on_save_and_drop
    block_view::history::tests::failed_zone_history_notice_wraps_and_explains_recovery
    block_view::history::tests::unified_pane_lifecycle_preserves_failed_and_incomplete_restore_bytes
    block_view::history::tests::unified_pane_fresh_drop_reopen_and_runtime_retention_remain_writable
    block_view::tests::block_review_actions_are_identity_bound_and_never_execute
    workspace_ops::pane_tree_tests::equalize_without_allocation_leaves_the_tree_untouched
    workspace_ops::pane_tree_tests::leaf_slot_requires_the_exact_holder_tree_and_clears_focus_before_reparent
    block_view::blocks::tests::repeated_action_feedback_restores_the_original_affordance
    block_view::blocks::tests::block_actions_keep_keyboard_and_popover_ownership
    block_view::blocks::tests::block_context_wraps_separately_and_empty_output_actions_are_disabled
    block_view::blocks::tests::unmapped_refit_skips_output_row_scan
    block_view::blocks::tests::a_finished_codex_card_keeps_the_history_inserted_above_its_viewport
    block_view::blocks::tests::earlier_output_notice_follows_the_flag_and_the_fold
    block_view::blocks::tests::only_an_untrusted_record_wears_a_lifecycle_chip
    block_view::blocks::tests::the_selection_hint_sits_on_the_spacers_left
    block_view::blocks::tests::search_reads_the_filtered_view_and_falls_back_to_the_superset
    block_view::blocks::tests::block_density_switches_on_widgets_that_already_exist
    block_view::blocks::tests::output_scrollbar_visibility_cannot_change_the_terminal_width
    block_view::blocks::tests::visual_row_cache_covers_filter_remap_expand_and_resize_refit
    enter_ownership::tests::queued_review_write_claims_enter_at_both_backend_boundaries
    block_view::tests::held_enter_history_recall_does_not_submit_the_recalled_command
    block_view::cross_selection::tests::a_single_native_text_selection_survives_whole_card_selection_precedence
    block_view::css::tests::the_generated_stylesheet_parses_without_error
    terminal::vte::tests::live_screen_search_finds_text_on_the_alternate_screen
    font::tests::default_font_resolution_preserves_grid_and_configuration
    font::tests::an_appended_font_family_resolves_in_the_native_font_map
    font::tests::a_real_vte_is_given_the_icon_family_behind_the_configured_one
    block_view::onboarding::tests::inline_notice_priority_handles_multiple_cards_reparenting_and_narrow_panes
    block_view::onboarding::tests::block_onboarding_overlay_is_non_measuring_and_non_targetable
    block_view::find::tests::unified_vte_fresh_query_reaches_scrollback_before_a_prior_match
    block_view::find::tests::a_live_find_counts_what_the_live_vte_steps_through
    block_view::find::tests::unified_bounded_and_native_fallback_prefer_visible_match_with_huge_old_scrollback
    block_view::find::tests::unified_complete_windows_step_visible_then_wrapped_history_on_real_vte
    block_view::tests::dock_mount_refuses_a_widget_another_region_owns
    block_view::tests::late_inline_notice_adopts_the_panes_current_density
    block_view::tests::bounded_vte_capture_real_vte_uses_half_open_column_boundary
    block_view::tests::real_vte_prompt_area_covers_a_menu_after_the_pane_has_scrolled
    block_view::tests::finished_card_focus_keeps_the_block_keyboard_contract
    block_view::tests::real_vte_reports_travel_the_commit_path_as_reports_not_typing
    block_view::tests::entering_alt_screen_ends_the_block_selection_it_hides
    block_view::tests::a_header_button_press_does_not_select_the_card
    terminal::url::tests::a_real_vte_opens_an_osc8_link_whose_label_is_not_a_url
    block_view::tests::refusal_feedback_refreshes_and_only_the_latest_status_restores
    block_view::scroll::tests::widget_pool_releases_heavy_children_and_stale_controllers
    block_view::unified_images::tests::real_vte_keeps_nonzero_marker_column_through_narrow_wide_rewrap
    block_view::unified_chrome::tests::real_vte_osc8_row_probe_and_rewrap_smoke
    block_view::unified_chrome::tests::real_vte_non_bottom_anchor_calibrates_and_wide_badge_probe_fails_closed
    sidebar::tests::programmatic_location_rebuild_does_not_echo_back_as_a_user_switch
)

export RUST_TEST_THREADS=1
export GDK_BACKEND=x11
export GSK_RENDERER=cairo
export LIBGL_ALWAYS_SOFTWARE=1
export NO_AT_BRIDGE=1
export GTK_A11Y=none

listed_tests="$(cargo test --locked -- --ignored --list)"
for test_name in "${tests[@]}"; do
    if ! grep -Fqx -- "${test_name}: test" <<<"${listed_tests}"; then
        printf 'Error: GTK display regression is not registered: %s\n' "${test_name}" >&2
        exit 1
    fi
done

case_count=0
passed_count=0
failed_cases=()
failed_statuses=()

run_case() {
    local case_name="$1" status
    shift
    case_count=$((case_count + 1))
    printf 'GTK display regression: %s\n' "${case_name}"
    # Keep test failures out of errexit so every isolated case can report its
    # result. Setup and registration checks above remain fail-fast.
    if "$@"; then
        passed_count=$((passed_count + 1))
    else
        status=$?
        failed_cases+=("${case_name}")
        failed_statuses+=("${status}")
        printf 'GTK display regression failed (exit %d): %s\n' "${status}" "${case_name}" >&2
    fi
}

for test_name in "${tests[@]}"; do
    # GTK caches its accessibility backend on first use. Only the existing
    # companion fixture needs TestATContext for exact decorative Hidden values;
    # all other invocations retain the original disabled accessibility backend.
    fixture_a11y=none
    if [[ "${test_name}" == organism_ui::tests::live_settings_reuse_body_preserve_focus_and_stop_disabled_timers ]]; then
        fixture_a11y="test"
    fi
    fixture_im_module="${GTK_IM_MODULE-}"
    if [[ "${test_name}" == block_view::chat::tests::native_ime_commit_cancel_and_independent_ptys || "${test_name}" == block_view::chat::tests::native_ime_send_button_preserves_composition ]]; then
        fixture_im_module=simple
    fi
    GTK_IM_MODULE="${fixture_im_module}" GTK_A11Y="${fixture_a11y}" run_case "${test_name}" \
        dbus-run-session "${dbus_run_args[@]}" -- \
        xvfb-run --auto-servernum --server-args='-screen 0 1280x800x24 -nolisten tcp' \
        cargo test --locked "${test_name}" -- --ignored --exact --nocapture
    if [[ "${test_name}" == block_view::tests::held_enter_history_recall_does_not_submit_the_recalled_command ]]; then
        for mode in review-mouse-only-fresh review-mouse-only-keypad-fresh review-mouse-only-keypad-pane-fresh; do
            ANVIL_ENTER_MODE="${mode}" ANVIL_QA_PREWARM_ENTER=1 ANVIL_QA_INHERITED_ENTER=1 \
                run_case "${test_name} (settled release then outside-focus Enter: ${mode})" \
                dbus-run-session "${dbus_run_args[@]}" -- \
                xvfb-run --auto-servernum --server-args='-screen 0 1280x800x24 -nolisten tcp' \
                cargo test --locked "${test_name}" -- --ignored --exact --nocapture
        done
    fi
    if [[ "${test_name}" == enter_ownership::tests::queued_review_write_claims_enter_at_both_backend_boundaries ]]; then
        for inherited in 0 1; do
            for dual in 0 1; do
                ANVIL_QA_FILE_DROP=1 ANVIL_QA_INHERITED_ENTER="${inherited}" ANVIL_QA_DUAL_ENTER="${dual}" \
                    run_case "${test_name} (file-drop Enter ownership: inherited=${inherited} dual=${dual})" \
                    dbus-run-session "${dbus_run_args[@]}" -- \
                    xvfb-run --auto-servernum --server-args='-screen 0 1280x800x24 -nolisten tcp' \
                    cargo test --locked "${test_name}" -- --ignored --exact --nocapture
            done
        done
        ANVIL_QA_FILE_DROP=1 ANVIL_QA_FRESH_ENTER=1 \
            run_case "${test_name} (file-drop fresh Enter after release)" \
            dbus-run-session "${dbus_run_args[@]}" -- \
            xvfb-run --auto-servernum --server-args='-screen 0 1280x800x24 -nolisten tcp' \
            cargo test --locked "${test_name}" -- --ignored --exact --nocapture
        ANVIL_QA_DUAL_ENTER=1 run_case "${test_name} (simultaneous main/keypad Enter ownership)" \
            dbus-run-session "${dbus_run_args[@]}" -- \
            xvfb-run --auto-servernum --server-args='-screen 0 1280x800x24 -nolisten tcp' \
            cargo test --locked "${test_name}" -- --ignored --exact --nocapture
        for dual in 0 1; do
            ANVIL_QA_INHERITED_ENTER=1 ANVIL_QA_DUAL_ENTER="${dual}" \
                run_case "${test_name} (outside-origin Enter: dual=${dual})" \
                dbus-run-session "${dbus_run_args[@]}" -- \
                xvfb-run --auto-servernum --server-args='-screen 0 1280x800x24 -nolisten tcp' \
                cargo test --locked "${test_name}" -- --ignored --exact --nocapture
        done
    fi
done

printf 'GTK display regressions passed: %d/%d.\n' "${passed_count}" "${case_count}"
if (( ${#failed_cases[@]} > 0 )); then
    printf 'Failed GTK display regressions (%d):\n' "${#failed_cases[@]}" >&2
    for index in "${!failed_cases[@]}"; do
        printf '  %s (exit %d)\n' "${failed_cases[index]}" "${failed_statuses[index]}" >&2
    done
    exit 1
fi
