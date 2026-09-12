# VHS Tape: Safe-to-Share Incident Export with Redaction
# Output: safe_export_demo.gif

Output examples/tapes/safe_export_demo.gif
Set FontSize 18
Set Width 1200
Set Height 600
Set Padding 20

Type "# Export session with automatic sensitive content scanning and redaction"
Enter
Sleep 1s

Type "mcptracer export <session-id> --out bug_report.mtrace"
Enter
Sleep 2s

Type "# Verify offline evidence manifest without network or database access"
Enter
Sleep 1s

Type "mcptracer verify baseline.manifest.json --artifact bug_report.mtrace --assert-spec pin.toml"
Enter
Sleep 3s