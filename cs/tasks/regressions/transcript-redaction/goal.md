# transcript-redaction

Lead and lane transcripts, run output, and Client API replies must not keep
credential shapes. The shared redactor (`ops/bin/_job_reason.py`) covers:

- Director tokens `csl_`, `csn_`, `csd_` plus 64 lower-case hex digits
- PEM private-key blocks, from BEGIN through END
- `Authorization: Basic` and `Authorization: Bearer` headers
- URLs with `user:pass@`
- AWS `AKIA`, GitHub `ghp_` / `gho_` / `github_pat_`, Slack `xox[abpr]-`,
  OpenAI `sk-`, Anthropic `sk-ant-`, Google `AIza`

Redaction runs where those texts are stored or returned: `transcript.md` and
`cli.log` (`persist_output` in the executor, muse adapter, and lead host),
`events.jsonl`, `messages.jsonl` turn text, `check.json` output, events
posted to the Director, the lead session transcript, and `task_output`
(live partial text, transcript tail, check output, error). Synthetic values
only.
