---
name: gate
description: Runs the repository gate on an existing lane with no model.
model: gate
writes: false
lane: true
check: ""
budget: {turns: 1, tool_calls: 0, seconds: 3600, usd: 0}
task_type: test
---
Run the machine-configured repository gate.
