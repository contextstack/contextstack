# release-bundles

Install stage 1 (vault `Plan.RealWork.Install.2026-11-11`, decisions 9–12).
`.github/workflows/release-bundles.yml` builds macOS arm64, Linux x86_64 and
Linux arm64. Each build job runs `node-bundle build` with `--publish-dir`.
`node_publish.publish` writes one platform; `node_publish.py merge` unions the
three into `<channel>.json` and `<channel>.txt`. No release host and no secrets.

Run: `tasks/regressions/release-bundles/check`
