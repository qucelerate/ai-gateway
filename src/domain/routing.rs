//! Picking one of the deployments an alias routes to (D-023, Phase 9).
//!
//! Each deployment of a route has a weight and a priority. A request goes to
//! the lowest priority that has a healthy deployment, and among that
//! priority's healthy deployments by weight. When none is healthy anywhere,
//! it goes by weight to the lowest priority, so the request still fails with
//! that deployment's own error (an open circuit's retry-after, the provider's
//! refusal) instead of one the route made up.

/// One deployment of a route, as selection sees it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Choice {
    pub weight: u32,
    pub priority: u32,
    pub healthy: bool,
}

/// What selection picked.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Pick {
    /// The index into the choices.
    pub index: usize,
    /// False when no deployment was healthy and this one was picked anyway.
    pub healthy: bool,
}

/// Picks one of `choices` for a `roll` drawn uniformly from `0..u64::MAX`.
/// `None` only when there are no choices.
pub fn pick(choices: &[Choice], roll: u64) -> Option<Pick> {
    let lowest = |healthy_only: bool| {
        choices
            .iter()
            .filter(|choice| !healthy_only || choice.healthy)
            .map(|choice| choice.priority)
            .min()
    };
    let (priority, healthy_only) = match lowest(true) {
        Some(priority) => (priority, true),
        None => (lowest(false)?, false),
    };
    let eligible =
        |choice: &Choice| choice.priority == priority && (!healthy_only || choice.healthy);
    let total: u64 = choices
        .iter()
        .filter(|choice| eligible(choice))
        .map(|choice| u64::from(choice.weight.max(1)))
        .sum();
    // `roll % total` is biased by at most total / 2^64, far below anything
    // measurable at these weights.
    let mut at = roll % total;
    for (index, choice) in choices.iter().enumerate() {
        if !eligible(choice) {
            continue;
        }
        let weight = u64::from(choice.weight.max(1));
        if at < weight {
            return Some(Pick {
                index,
                healthy: healthy_only,
            });
        }
        at -= weight;
    }
    unreachable!("the roll is below the eligible weights' total")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(weight: u32, priority: u32, healthy: bool) -> Choice {
        Choice {
            weight,
            priority,
            healthy,
        }
    }

    fn counts(choices: &[Choice], draws: u64) -> Vec<u64> {
        let mut counts = vec![0; choices.len()];
        // Evenly spaced rolls: the counts are exactly proportional.
        let step = u64::MAX / draws;
        for draw in 0..draws {
            counts[pick(choices, draw * step).unwrap().index] += 1;
        }
        counts
    }

    #[test]
    fn traffic_splits_by_weight_within_a_priority() {
        let choices = [choice(3, 1, true), choice(1, 1, true)];
        let counts = counts(&choices, 1000);
        assert!((740..=760).contains(&counts[0]), "{counts:?}");
        assert_eq!(counts[0] + counts[1], 1000);
    }

    #[test]
    fn a_lower_priority_serves_only_while_every_higher_one_is_unhealthy() {
        let mut choices = [choice(1, 1, true), choice(1, 1, true), choice(5, 2, true)];
        assert_eq!(counts(&choices, 100)[2], 0);
        choices[0].healthy = false;
        let counts_one_down = counts(&choices, 100);
        assert_eq!(counts_one_down, vec![0, 100, 0]);
        choices[1].healthy = false;
        assert_eq!(counts(&choices, 100), vec![0, 0, 100]);
        assert!(pick(&choices, 7).unwrap().healthy);
    }

    #[test]
    fn with_nothing_healthy_the_lowest_priority_is_tried_anyway() {
        let choices = [choice(1, 2, false), choice(1, 1, false)];
        let picked = pick(&choices, 12345).unwrap();
        assert_eq!(
            picked,
            Pick {
                index: 1,
                healthy: false
            }
        );
        assert_eq!(pick(&[], 1), None);
    }

    #[test]
    fn one_deployment_is_always_picked() {
        for roll in [0, 1, u64::MAX - 1, u64::MAX] {
            assert_eq!(
                pick(&[choice(1, 1, true)], roll),
                Some(Pick {
                    index: 0,
                    healthy: true
                })
            );
        }
    }
}
