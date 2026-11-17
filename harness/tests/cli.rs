//! CLI parsing regressions.
mod estate_support;
use cs_harness::cli::execute_from;

#[tokio::test]
async fn unknown_subcommand_prints_usage() {
    match execute_from(["lab".into()].into_iter(), None).await {
        Err(err) => assert!(err.starts_with("usage:")),
        Ok(_) => panic!("expected usage error"),
    }
}

#[tokio::test]
async fn unknown_flag_rejects_before_spec_load() {
    match execute_from(
        ["run".into(), "-bogus".into(), "x.yaml".into()].into_iter(),
        None,
    )
    .await
    {
        Err(err) => assert!(err.contains("flag provided but not defined: -bogus")),
        Ok(_) => panic!("expected unknown flag error"),
    }
}

#[tokio::test]
async fn multi_task_spec_requires_task_flag() -> Result<(), String> {
    use estate_support::Lab;
    let lab = Lab::build()?;
    let yaml = "id: multi\nmodel: fixture\nsystem: prompts/coder-min.md\ntools: []\nbudget: {turns: 1}\ntasks: [tasks/fx, tasks/fx]\ntrials: 1\nhypothesis: 'fixture'\n";
    let path = lab.root.join("specs/two.yaml");
    std::fs::write(&path, yaml).map_err(|e| e.to_string())?;
    match execute_from(
        ["run".into(), "specs/two.yaml".into()].into_iter(),
        Some(&lab.root),
    )
    .await
    {
        Err(err) => assert!(err.contains("pass -task")),
        Ok(_) => panic!("expected multi-task error"),
    }
    Ok(())
}

#[tokio::test]
async fn invalid_trial_flag_is_rejected() {
    if let Err(err) = execute_from(
        [
            "run".into(),
            "-trial".into(),
            "nope".into(),
            "x.yaml".into(),
        ]
        .into_iter(),
        None,
    )
    .await
    {
        assert!(err.contains("invalid value \"nope\" for flag -trial"));
    } else {
        panic!("expected invalid trial error");
    }
}

#[tokio::test]
async fn missing_flag_argument_is_rejected() {
    if let Err(err) = execute_from(["run".into(), "-task".into()].into_iter(), None).await {
        assert!(err.contains("flag needs an argument: -task"));
    } else {
        panic!("expected missing flag value error");
    }
}

#[tokio::test]
async fn extra_positional_args_return_usage() {
    if let Err(err) = execute_from(
        ["run".into(), "a.yaml".into(), "extra".into()].into_iter(),
        None,
    )
    .await
    {
        assert!(err.starts_with("usage:"));
    } else {
        panic!("expected usage error");
    }
}
