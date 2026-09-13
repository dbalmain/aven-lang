source "$AVEN_COMPLETION_SCRIPT" || exit 91
registered=$(complete -p -- "$AVEN_COMPLETION_PROGRAM") || exit 92
registered=${registered#* -F }
registered=${registered%% *}
declare -F "$registered" >/dev/null || exit 93
# The generated callback reads COMP_LINE and COMP_POINT and nothing else --- not
# COMP_WORDS, not its own positional arguments --- because bash's own splitting
# is what it exists to replace. So setting those two is the whole of what
# Readline would have handed it, and this harness re-implements none of the
# word-splitting it is meant to be testing.
COMP_LINE=$AVEN_COMPLETION_INPUT
COMP_POINT=${#COMP_LINE}
"$registered" "$AVEN_COMPLETION_PROGRAM" '' ''
printf '%s\0' "${#COMPREPLY[@]}"
for candidate in "${COMPREPLY[@]}"; do printf '%s\0' "$candidate"; done
