Optional MHTML export must never discard a successfully rendered page.

When Chrome cannot create an archive or the archive exceeds its ceiling, retain
the hashed visible text and link map, issue a normal evidence receipt, and record
an explicit non-sensitive `archive_omitted` reason.
