A profiled CLI template (cli-profiles.json: opencode, opencode-go) run through the executor on an installed node,
where the code, state, config and workspaces roots are separate and a Director job's task lives under state/jobs/<id>/tasks/,
records its run row in the state root with complete evidence and writes nothing under the code root.
