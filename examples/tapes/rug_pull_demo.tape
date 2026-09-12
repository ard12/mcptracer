# VHS Tape: Catching an MCP Tool Rug-Pull in CI
# Output: rug_pull_demo.gif

Output examples/tapes/rug_pull_demo.gif
Set FontSize 18
Set Width 1200
Set Height 600
Set Padding 20

Type "# Scenario: A dependency silently changed a tool's description"
Enter
Sleep 1s

Type "mcptracer diff <baseline-session-id> <candidate-session-id>"
Enter
Sleep 3s

Type "# Now let's enforce this golden tool contract in CI:"
Enter
Sleep 1s

Type "mcptracer assert <candidate-session-id> --spec examples/rug-pull-demo/pin.toml"
Enter
Sleep 3s