# The shipped model catalog starts a gateway

Every release ships `gateway/models.json` as its catalog. On 2026-11-14 a generic catalog carried a deployment whose estate capacity shares exceeded its `max_concurrent`, so the gateway refused to start on any fresh install; the contract suite missed it because it builds its own config. This check starts the built gateway with the shipped catalog, no host overlay and a temp home, and needs `/v1/models` to answer.
