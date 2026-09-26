# Re-record with: vhs docs/assets/harness-dark.tape
# Needs vhs, opencode, and a signed-in wally (wally account login).
# The hidden setup line makes the demo project and isolates opencode's state
# in a temp dir, so the recording does not depend on this machine.
Output harness-dark.gif
Set Shell zsh
Set FontSize 15
Set Width 1200
Set Height 640
Set Padding 24
Set Theme {"background":"#0d1117","foreground":"#e6edf3","cursor":"#e6edf3"}
Set TypingSpeed 40ms
Hide
Type `export XDG_STATE_HOME=$(mktemp -d) && mkdir -p demo-proj/.opencode && print -l 'def add(a, b):' '    return a - b' > demo-proj/calc.py && printf '{ "theme": "osaka-jade" }' | tee demo-proj/tui.json > demo-proj/.opencode/tui.json && clear`
Enter
Sleep 500ms
Show
Type "cd demo-proj && wally opencode --cloud -m glm-5.3-flash"
Enter
Sleep 8s
Type "There is a bug in calc.py, fix it."
Sleep 500ms
Enter
Sleep 45s
Screenshot harness-dark-end.png
Sleep 2s
