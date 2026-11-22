# A theme of the plain kind (no real tool): a PROMPT_COMMAND entry, added at the
# end of the text, that rebuilds PS1 at every prompt.
__appended_theme() { PS1='theme\$ '; }
PROMPT_COMMAND+=$'\n__appended_theme'
