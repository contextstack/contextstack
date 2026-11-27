//! Validate the five numeric cron fields understood by the node's schedule tick.

pub fn cron(value: &str) -> bool {
    let fields: Vec<_> = value.split_ascii_whitespace().collect();
    value.len() <= 500
        && fields.len() == 5
        && fields
            .iter()
            .zip([(0, 59), (0, 23), (1, 31), (1, 12), (0, 7)])
            .all(|(field, (low, high))| {
                field.split(',').all(|part| {
                    let base = if let Some((base, step)) = part.split_once('/') {
                        if !step.parse::<u32>().is_ok_and(|n| n > 0) {
                            return false;
                        }
                        base
                    } else {
                        part
                    };
                    let number =
                        |s: &str| s.parse::<u32>().ok().filter(|n| (low..=high).contains(n));
                    base == "*"
                        || match base.split_once('-') {
                            Some((a, b)) => number(a).zip(number(b)).is_some_and(|(a, b)| a <= b),
                            None => number(base).is_some(),
                        }
                })
            })
}

pub fn timezone(value: &str) -> bool {
    value == "local" || value.parse::<chrono_tz::Tz>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_the_scheduler_grammar() {
        assert!(cron("*/15 8-17 1,15 * 0,7"));
        for expression in [
            "* * * *",
            "60 * * * *",
            "* 9-8 * * *",
            "* * 0 * *",
            "* * * 13 *",
            "* * * * */0",
            "* * * * 1,,2",
            "* * * * Mon",
        ] {
            assert!(!cron(expression), "{expression}");
        }
        assert!(timezone("America/New_York"));
        assert!(timezone("local"));
        assert!(!timezone("Mars/Olympus"));
    }
}
