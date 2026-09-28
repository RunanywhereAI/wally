# Re-record from this directory so the gif lands next to the tape:
#   cd docs/assets && vhs demo-light.tape
# Needs vhs and the model already pulled: wally models pull qwen3
Output demo-light.gif
Set Shell zsh
Set FontSize 15
Set Width 1200
Set Height 200
Set Padding 24
Set Theme {"background":"#ffffff","foreground":"#1f2328","cursor":"#1f2328"}
Set TypingSpeed 40ms
Type "wally run qwen3 --hide-thinking 'Give me three short reasons to run a model locally, one line each.'"
Sleep 600ms
Enter
Sleep 10s
