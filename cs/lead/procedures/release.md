# Public release

Never push, tag or publish without an owner approval bound to the exact commit and tag.

Open with `procedure(action="read", name="release")`. Start with owner authorization.
Use `start_task`, `wait_tasks`, `task_status` and `task_output`. Log lane/run IDs and evidence in the backlog.

## Export lane

1. Prepare and check the dependency-update commit first. Record its SHA,
   dependency changes and verdict. Use the review procedure for private lands.
2. Run `scripts/public-export/export --out DIR --commit SOURCE_SHA` into an empty
   directory. Keep both audit reports private. Review exclusions, findings and
   proofs. Prepare an export commit on the owner-provided public main snapshot.
   Compare the committed tree with the audited inventory and bytes. If the tree
   differs, audit it with `--tree` and stop on any unaudited paths or bytes.
   Record the source, base, export commit and tree SHAs, proposed tag and verdict.
3. Create a separate `RELEASES.json` commit after the export commit. Preserve prior
   public entries and add the tag and **export commit** SHA with channel `pre` for
   an rc or `stable` for a final. Record the manifest commit as the proposed main
   tip. CI checks that the tag target is reachable from main and listed on main.

## Clean-VM install lane

On the target platform build from the exact export commit with
`cs/scripts/node-bundle build --director-rs --release-version TAG DIR`.
On a clean VM run `sh DIR/release/cs/scripts/node-install install DIR`,
`cs init` and smoke checks, including a hosted-lead tool call. Use
install-from-chat when available. Record OS/architecture, commit, tag,
bundle checksum, install and smoke verdicts, and skipped checks.

## Owner-approved push/tag action

Use `needs_you` and follow `owner-decisions`. Present `contextstack/contextstack`,
`refs/heads/main`, expected base, exact manifest commit, exact tag and export
commit it points to, and the lane evidence. The tag triggers publication.
The rc workflow also replaces the `channel-latest` release and moves its tag.
Stop if that CI side effect is outside the approved release scope.
Offer **approve this exact release**, **revise and recheck**, and **stop**.
Add labelled options to the linked backlog item when available. Wait for
direct owner approval for this release. Land, peer and earlier rc approvals
cannot authorize publication.

The owner handles publication on their trusted machine. The hosted lead
has no `release_action` executor. The owner verifies the remote, expected main
base, fast-forward ancestry, manifest pair and unused tag. They push main,
then create and push the exact tag. Never force-push or delete remote refs.
If an input changes, require fresh approval. Record the approval reference
and resulting remote main and tag target SHAs. On any uncertain or partial
outcome, stop and ask the owner to reconcile refs before retrying. Keep
credentials out of requests, artifacts and logs.

## CI-watch lane

After the owner confirms the push, use read-only `gh` access via
`python3 cs/scripts/release-ci-watch --repo OWNER/REPO --tag TAG --commit EXPORT_SHA`.
For public main CI use `--workflow cs-tests.yml --branch main --commit MANIFEST_SHA`
with the same repo. Record the run ID, URL, conclusion and failing job names.
After CI passes, verify bundle assets and checksums with read-only access.

Start with `vX.Y.Z-rc.N`. After a clean rc, repeat the manifest, approval and
CI gates for `vX.Y.Z` on the same export commit. Validate the final-version
bundle before approval. A source change requires a fresh rc and VM check.

## Notes lane

Run `python3 cs/scripts/release-notes --tag TAG --commit EXPORT_SHA` in the
release checkout. Use `--since PREVIOUS_STABLE_TAG` for the full stable range.
Give the owner the draft, base and target SHAs, and install and known-issue
evidence for editing and approval. The owner publishes the approved notes and
backs up the assets, manifests and checksums to DigitalOcean Spaces. Record
the release URL, backup object locations and checksum verification.

## Stop points

Stop for failed or incomplete gates, missing evidence, a mismatched SHA or
tag, a denied approval, or an uncertain outcome. Use `needs_you` with the job,
candidate and evidence. Offer options such as retrying a read, fixing and
rechecking, supplying evidence, or stopping. Resume after the blocker clears
and the owner approves any changed publication inputs.
