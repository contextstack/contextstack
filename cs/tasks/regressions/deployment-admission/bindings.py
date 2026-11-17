"""Actual executor/config coherence. EXP1848 pre-admission void."""
import json

def validate(templates, models):
    names = json.loads(templates.read_text())
    config = json.loads(models.read_text())
    bindings = {}
    for deployment in config['deployments']:
        if deployment['provider'] == 'cli':
            for name in deployment['models']:
                bindings.setdefault(name, []).append(deployment['id'])
    bad = {name: bindings.get(name, []) for name in names if len(bindings.get(name, [])) != 1}
    assert not bad, 'Missing or ambiguous executor admission bindings: ' + repr(bad)
    assert bindings['claude-code-sonnet-patch'] == bindings['claude-code-sonnet'], 'Patch alias must share the existing Claude capacity pool'

