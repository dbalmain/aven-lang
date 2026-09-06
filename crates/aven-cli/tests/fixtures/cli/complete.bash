source "$AVEN_COMPLETION_SCRIPT" || exit 91
registered=$(complete -p -- "$AVEN_COMPLETION_PROGRAM") || exit 92
registered=${registered#* -F }
registered=${registered%% *}
declare -F "$registered" >/dev/null || exit 93
_aven_test_capture() {
  "$registered" "$@"
  printf '%s\0' "${#COMPREPLY[@]}"
  for candidate in "${COMPREPLY[@]}"; do printf '%s\0' "$candidate"; done
}
complete -F _aven_test_capture -- "$AVEN_COMPLETION_PROGRAM"
bind 'set editing-mode emacs'
bind 'set disable-completion off'
bind 'TAB:complete'
bind 'set input-meta on'
bind 'set convert-meta off'
bind 'set output-meta on'
