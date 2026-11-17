# web_search fixtures

Negative cases only. `_web.search` validates `query` and `count`, then fetches
`https://html.duckduckgo.com/html/` (and Bing RSS as fallback) through `_web.fetch`, which needs a public
address and a TLS peer (see ../web_fetch.fixtures/README.md). No offline positive case exists without
changing the tool. The three refusals above are decided before any socket opens.
