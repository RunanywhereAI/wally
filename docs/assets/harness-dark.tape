# Re-record from this directory so the gif lands next to the tape:
#   cd docs/assets && vhs harness-dark.tape
# Needs vhs, opencode, and a signed-in wally (wally account login).
# The system theme lets opencode use the terminal colors, so the whole frame is
# GitHub dark. The hidden setup line makes the demo project and isolates opencode's state
# in a temp dir, so the recording does not depend on this machine.
Output harness-dark.gif
Set Shell zsh
Set FontSize 15
Set Width 1200
Set Height 640
Set Padding 24
Set Theme {"background":"#0d1117","foreground":"#e6edf3","cursor":"#e6edf3","selection":"#264f78","black":"#484f58","red":"#ff7b72","green":"#3fb950","yellow":"#d29922","blue":"#58a6ff","magenta":"#bc8cff","cyan":"#39c5cf","white":"#b1bac4","brightBlack":"#6e7681","brightRed":"#ffa198","brightGreen":"#56d364","brightYellow":"#e3b341","brightBlue":"#79c0ff","brightMagenta":"#d2a8ff","brightCyan":"#56d4dd","brightWhite":"#f0f6fc"}
Set TypingSpeed 40ms
Hide
Type `cd $(mktemp -d /tmp/wally.XXXXXX) && export XDG_STATE_HOME=$(mktemp -d) && mkdir -p demo-proj/.opencode && print -l 'def add(a, b):' '    return a - b' > demo-proj/calc.py && printf '{ "theme": "system" }' | tee demo-proj/tui.json > demo-proj/.opencode/tui.json && clear`
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
