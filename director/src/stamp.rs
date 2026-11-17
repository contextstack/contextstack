//! What the Director writes into a job's spec before it is offered: the intent it carries out (`intent:`, here) and
//! a budget's reservation (`budget.usd`, `reservations.rs`). Both edit the bundle's spec object in place, so the
//! job's `bundle_sha256` covers them; the intent keeps the bundle as given.
//!
//! A job made from an intent names that intent's ULID as `intent:` (ADR 0015: the runner refuses a spec without
//! one). The client cannot know the ULID before it submits, so a spec states none and the Director adds it; a spec
//! that states another is refused, naming the field. A job made by `director-rs job create` carries no intent and
//! its spec is offered as it is.

use crate::jobs::BundleFile;
use crate::store::Store;
use crate::util::sha256_hex;

/// Replace the bundle's `spec` with `edit(spec bytes)`: the new object is stored and the file entry points at it.
pub fn rewrite(
    store: &Store,
    files: &mut [BundleFile],
    spec: &str,
    edit: impl FnOnce(&[u8]) -> Result<Vec<u8>, String>,
) -> Result<(), String> {
    let file = files
        .iter_mut()
        .find(|f| f.path == spec)
        .ok_or_else(|| format!("the bundle has no {spec}"))?;
    let bytes = store
        .read_object(&file.sha256)
        .ok_or_else(|| format!("{spec} is not stored"))?;
    let out = edit(&bytes)?;
    store.put_object(&out)?;
    file.sha256 = sha256_hex(&out);
    file.bytes = out.len() as u64;
    Ok(())
}

/// Why a spec cannot carry an intent.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// The spec names another intent; `String` says which.
    Mismatch(String),
    /// The spec is not a YAML mapping, or is not stored.
    Invalid(String),
}

/// The spec with `intent:` set to `intent`, edited in place (`spec_edit.rs`): the line is appended, or a replaceable
/// one replaced, and every other byte is kept as the client sent it. A spec that already states `intent` is kept as
/// it is; one stating `replaceable` (the Director's own stamp on the job a `from_job` intent offers again) is
/// restamped; any other value is refused. So is a spec too unusual to edit in place, or whose `budget.usd` a
/// reservation could not later set in place: refused now, with the reason, rather than failing when it is reserved.
pub fn with_intent(
    spec: &[u8],
    intent: &str,
    replaceable: Option<&str>,
) -> Result<Option<Vec<u8>>, Refused> {
    let map = crate::spec_edit::parse(spec).map_err(Refused::Invalid)?;
    match map.get("intent") {
        None | Some(serde_yaml::Value::Null) => {}
        Some(serde_yaml::Value::String(s)) if s == intent => return Ok(None),
        Some(serde_yaml::Value::String(s)) if Some(s.as_str()) == replaceable => {}
        Some(other) => {
            let shown = serde_yaml::to_string(other).unwrap_or_default();
            return Err(Refused::Mismatch(format!(
                "intent: the spec names intent {}, not this intent ({intent}); leave the field out and the Director adds it",
                shown.trim()
            )));
        }
    }
    let out = crate::spec_edit::set_top(spec, "intent", intent).map_err(Refused::Invalid)?;
    crate::spec_edit::set_budget_usd(&out, 1).map_err(Refused::Invalid)?;
    Ok(Some(out))
}

/// Stamp `intent` into the bundle's `spec` (see `with_intent`); the bundle is unchanged when it already names it.
pub fn intent(
    store: &Store,
    files: &mut [BundleFile],
    spec: &str,
    intent: &str,
    replaceable: Option<&str>,
) -> Result<(), Refused> {
    let bytes = files
        .iter()
        .find(|f| f.path == spec)
        .and_then(|f| store.read_object(&f.sha256))
        .ok_or_else(|| Refused::Invalid(format!("{spec} is not stored")))?;
    if let Some(out) = with_intent(&bytes, intent, replaceable)? {
        rewrite(store, files, spec, |_| Ok(out)).map_err(Refused::Invalid)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "01M74BZ2Y1DQ9SF36Y33WQ53R1";

    fn doc(b: &[u8]) -> serde_yaml::Value {
        serde_yaml::from_slice(b).unwrap_or_default()
    }

    #[test]
    fn absent_is_added_and_the_rest_kept() -> Result<(), String> {
        let spec = b"id: hello\nbudget: {turns: 4, usd: 0}\ntasks: [tasks/hello]\n";
        let out = with_intent(spec, ID, None)
            .map_err(|e| format!("{e:?}"))?
            .ok_or("unchanged")?;
        let d = doc(&out);
        assert_eq!(d["intent"].as_str(), Some(ID));
        assert_eq!(d["id"].as_str(), Some("hello"));
        assert_eq!(d["budget"]["turns"].as_i64(), Some(4));
        let null = with_intent(b"id: x\nintent:\n", ID, None).map_err(|e| format!("{e:?}"))?;
        assert_eq!(
            null.map(|o| doc(&o)["intent"].as_str().map(String::from)),
            Some(Some(ID.into()))
        );
        Ok(())
    }

    #[test]
    fn matching_is_kept_byte_for_byte() {
        let spec = format!("id: x  # keep me\nintent: {ID}\n");
        assert_eq!(with_intent(spec.as_bytes(), ID, None), Ok(None));
    }

    #[test]
    fn another_is_refused_naming_the_field() {
        for other in ["task-1", "01M74BZ2Y1RWHBP0N1QDR3ZS62", "7"] {
            let spec = format!("id: x\nintent: {other}\n");
            match with_intent(spec.as_bytes(), ID, None) {
                Err(Refused::Mismatch(m)) => {
                    assert!(m.starts_with("intent:") && m.contains(other), "{m}");
                }
                got => panic!("{other}: {got:?}"),
            }
        }
    }

    #[test]
    fn the_directors_own_stamp_is_replaced() -> Result<(), String> {
        let old = "01M74BZ2Y1RWHBP0N1QDR3ZS62";
        let out = with_intent(
            format!("id: x\nintent: {old}\nmodel: m\n").as_bytes(),
            ID,
            Some(old),
        )
        .map_err(|e| format!("{e:?}"))?
        .ok_or("unchanged")?;
        assert_eq!(out, format!("id: x\nintent: {ID}\nmodel: m\n").into_bytes());
        Ok(())
    }

    #[test]
    fn the_stamp_is_the_original_plus_one_line() -> Result<(), String> {
        for spec in [
            "id: web-1\nbudget: {turns: 30, tool_calls: 60, usd: 0, seconds: 900}\ntasks: [tasks/many]\n",
            "# job\nid: web-1  # kept\nbudget:\n  turns: 30\n  usd: 0\ntasks:\n- tasks/many\nhypothesis: 'a: b'\n",
        ] {
            let out = with_intent(spec.as_bytes(), ID, None)
                .map_err(|e| format!("{e:?}"))?
                .ok_or("unchanged")?;
            assert_eq!(out, format!("{spec}intent: {ID}\n").into_bytes());
        }
        Ok(())
    }

    #[test]
    fn an_unusual_spec_is_refused_naming_why() {
        match with_intent(b"id: x\nbudget: &b {usd: 1}\n", ID, None) {
            Err(Refused::Invalid(m)) => assert!(m.contains("anchor"), "{m}"),
            got => panic!("{got:?}"),
        }
    }

    #[test]
    fn not_a_mapping_is_invalid() {
        assert!(matches!(
            with_intent(b"- a\n- b\n", ID, None),
            Err(Refused::Invalid(_))
        ));
        assert!(matches!(
            with_intent(b"{", ID, None),
            Err(Refused::Invalid(_))
        ));
    }
}
