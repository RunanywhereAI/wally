# Re-record from this directory so the gif lands next to the tape:
#   cd docs/assets && vhs harness-light.tape
# Needs vhs, opencode, and a signed-in wally (wally account login).
# The hidden setup line makes the demo project and isolates opencode's state
# in a temp dir, so the recording does not depend on this machine.
Output harness-light.gif
Set Shell zsh
Set FontSize 15
Set Width 1200
Set Height 640
Set Padding 24
Set Theme {"background":"#ffffff","foreground":"#1f2328","cursor":"#1f2328"}
Set TypingSpeed 40ms
Hide
Type `cd $(mktemp -d) && export XDG_STATE_HOME=$(mktemp -d) && mkdir -p demo-proj/.opencode && print -l 'def add(a, b):' '    return a - b' > demo-proj/calc.py && printf '{ "theme": "github-light" }' | tee demo-proj/tui.json > demo-proj/.opencode/tui.json && clear`
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
Screenshot harness-light-end.png
Sleep 2s
