Protect multi-rank judgment execution: a clean no-work preflight for the first
task may skip that task, but it must not suppress later tasks in the same spec.
The clean-first shortcut remains available for a genuinely single-task spec.

Every declared trial executes. A CLI may commit its output and leave a clean workspace; that is not grounds to fabricate skipped:empty-tree voids for the remaining trials.
