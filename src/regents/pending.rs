//! The pending mention (#17): a new mention of the agent of an active thread,
//! in the channel of that thread, waits for the decision of the thread. The
//! rule is pure: the caller injects the clock and applies the effect. The
//! thread registry keeps the phase and makes each step a compare-and-set.

/// The life cycle of a pending mention. `Waiting` and `Delivered` are open;
/// the other phases are final.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// The thread works or waits for an approval: it did not get the mention.
    Waiting,
    /// The thread got the mention and must answer accept or fork.
    Delivered,
    /// The mention belongs to the thread.
    Accepted,
    /// A new thread took the mention.
    Forked,
    /// The thread did not decide before the deadline: a new thread took it.
    TimedOut,
}

impl Phase {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Delivered => "delivered",
            Self::Accepted => "accepted",
            Self::Forked => "forked",
            Self::TimedOut => "timed_out",
        }
    }

    /// `None` for a phase written by a newer Regents.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "waiting" => Some(Self::Waiting),
            "delivered" => Some(Self::Delivered),
            "accepted" => Some(Self::Accepted),
            "forked" => Some(Self::Forked),
            "timed_out" => Some(Self::TimedOut),
            _ => None,
        }
    }

    pub(crate) fn is_open(self) -> bool {
        matches!(self, Self::Waiting | Self::Delivered)
    }
}

/// What happens to the thread of a pending mention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Event {
    /// The thread ran `regents thread accept`.
    Accept,
    /// The thread ran `regents thread fork`.
    Fork,
    /// The clock: the periodic folder check.
    Tick,
    /// The agent finished its work: the first idle after work, where a
    /// thread without a pending mention is done.
    Idle,
    /// The thread stopped running: the server restarted or handed off, the
    /// CLI exited or its pane closed.
    Ended,
}

/// What the caller does after a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Effect {
    None,
    /// Write the mention into the thread CLI. The thread is not done.
    Deliver,
    /// The next turn of the thread starts at the mention.
    Link,
    /// Open a new thread for the mention.
    OpenThread,
}

/// One step of a pending mention whose deadline is `decide_by` (Unix
/// seconds), at `now`. Past the deadline an open mention times out, whatever
/// the event. The thread gets the mention only at an idle: in an approval
/// prompt, typed text answers the prompt (claude, codex and agy, #17).
pub(crate) fn step(
    phase: Phase,
    decide_by: i64,
    event: Event,
    now: time::OffsetDateTime,
) -> (Phase, Effect) {
    if phase.is_open() && now.unix_timestamp() >= decide_by {
        return (Phase::TimedOut, Effect::OpenThread);
    }
    match (phase, event) {
        (Phase::Waiting, Event::Idle) => (Phase::Delivered, Effect::Deliver),
        (Phase::Delivered, Event::Accept) => (Phase::Accepted, Effect::Link),
        // An idle after the delivery without an answer is a fork: the thread
        // ended its turn and does not take the mention.
        (Phase::Delivered, Event::Fork | Event::Idle | Event::Ended)
        | (Phase::Waiting, Event::Ended) => (Phase::Forked, Effect::OpenThread),
        (phase, _) => (phase, Effect::None),
    }
}

/// Why `accept` or `fork` did not decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The thread did not get the mention yet.
    NotDelivered,
    /// The mention is already decided, in this phase.
    AlreadyDecided(Phase),
}

/// The answer to `decision` (`Accept` or `Fork`) when the step left the
/// mention in `after`. A repeated decision gets the same answer.
pub(crate) fn answer(decision: Event, after: Phase) -> Result<(), Refusal> {
    match (decision, after) {
        (Event::Accept, Phase::Accepted) | (Event::Fork, Phase::Forked | Phase::TimedOut) => Ok(()),
        (_, Phase::Waiting) => Err(Refusal::NotDelivered),
        (_, phase) => Err(Refusal::AlreadyDecided(phase)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DECIDE_BY: i64 = 1_000;

    fn at(seconds: i64) -> time::OffsetDateTime {
        time::OffsetDateTime::from_unix_timestamp(seconds).unwrap()
    }

    const EVENTS: [Event; 5] = [
        Event::Accept,
        Event::Fork,
        Event::Tick,
        Event::Idle,
        Event::Ended,
    ];

    // The state × event table of #17, before the deadline. A row is a
    // phase; `Waiting` covers a working and a blocked thread.
    #[test]
    fn the_table_of_states_and_events() {
        use Effect::*;
        use Phase::*;
        let table: [(Phase, [(Phase, Effect); 5]); 5] = [
            //        accept              fork                tick              idle                ended
            (
                Waiting,
                [
                    (Waiting, None),
                    (Waiting, None),
                    (Waiting, None),
                    (Delivered, Deliver),
                    (Forked, OpenThread),
                ],
            ),
            (
                Delivered,
                [
                    (Accepted, Link),
                    (Forked, OpenThread),
                    (Delivered, None),
                    (Forked, OpenThread),
                    (Forked, OpenThread),
                ],
            ),
            (Accepted, [(Accepted, None); 5]),
            (Forked, [(Forked, None); 5]),
            (TimedOut, [(TimedOut, None); 5]),
        ];
        for (phase, row) in table {
            for (event, expected) in EVENTS.into_iter().zip(row) {
                assert_eq!(
                    step(phase, DECIDE_BY, event, at(DECIDE_BY - 1)),
                    expected,
                    "{phase:?} × {event:?}"
                );
            }
        }
    }

    #[test]
    fn past_the_deadline_an_open_mention_times_out_on_any_event() {
        for phase in [Phase::Waiting, Phase::Delivered] {
            for event in EVENTS {
                assert_eq!(
                    step(phase, DECIDE_BY, event, at(DECIDE_BY)),
                    (Phase::TimedOut, Effect::OpenThread),
                    "{phase:?} × {event:?}"
                );
            }
        }
        for phase in [Phase::Accepted, Phase::Forked, Phase::TimedOut] {
            assert_eq!(
                step(phase, DECIDE_BY, Event::Tick, at(DECIDE_BY + 60)),
                (phase, Effect::None)
            );
        }
    }

    #[test]
    fn a_repeated_decision_gets_the_same_answer() {
        assert_eq!(answer(Event::Accept, Phase::Accepted), Ok(()));
        assert_eq!(answer(Event::Fork, Phase::Forked), Ok(()));
        assert_eq!(answer(Event::Fork, Phase::TimedOut), Ok(()));
        assert_eq!(
            answer(Event::Accept, Phase::Waiting),
            Err(Refusal::NotDelivered)
        );
        assert_eq!(
            answer(Event::Fork, Phase::Accepted),
            Err(Refusal::AlreadyDecided(Phase::Accepted))
        );
        assert_eq!(
            answer(Event::Accept, Phase::TimedOut),
            Err(Refusal::AlreadyDecided(Phase::TimedOut))
        );
    }

    #[test]
    fn the_phase_names_round_trip() {
        for phase in [
            Phase::Waiting,
            Phase::Delivered,
            Phase::Accepted,
            Phase::Forked,
            Phase::TimedOut,
        ] {
            assert_eq!(Phase::parse(phase.as_str()), Some(phase));
        }
        assert_eq!(Phase::parse("newer"), None);
    }

    /// Every order of up to six events, each before or after the deadline
    /// (the clock only goes forward). The mention resolves once: one link or
    /// one new thread, never both, and a final phase never changes. It gets
    /// the mention at most once, before it resolves, and a link needs it.
    #[test]
    fn every_order_of_the_events_resolves_the_mention_once() {
        // An event, and whether the clock passed the deadline before it.
        let moves: Vec<(Event, bool)> = EVENTS
            .into_iter()
            .flat_map(|event| [(event, false), (event, true)])
            .collect();
        let mut orders = 0usize;
        let mut sequence = Vec::new();
        explore(&moves, &mut sequence, 6, &mut orders);
        assert_eq!(orders, (1..=6).map(|n| 10usize.pow(n)).sum::<usize>());
    }

    fn explore(
        moves: &[(Event, bool)],
        sequence: &mut Vec<(Event, bool)>,
        depth: usize,
        orders: &mut usize,
    ) {
        if !sequence.is_empty() {
            *orders += 1;
            check(sequence);
        }
        if depth == 0 {
            return;
        }
        for &next in moves {
            sequence.push(next);
            explore(moves, sequence, depth - 1, orders);
            sequence.pop();
        }
    }

    fn check(sequence: &[(Event, bool)]) {
        let mut phase = Phase::Waiting;
        let (mut late, mut delivered, mut resolutions) = (false, 0, 0);
        for &(event, past) in sequence {
            late |= past;
            let now = at(if late { DECIDE_BY } else { DECIDE_BY - 1 });
            let before = phase;
            let effect;
            (phase, effect) = step(phase, DECIDE_BY, event, now);
            if !before.is_open() {
                assert_eq!((phase, effect), (before, Effect::None), "{sequence:?}");
            }
            match effect {
                Effect::Deliver => delivered += 1,
                Effect::Link => {
                    assert_eq!(delivered, 1, "a link needs the delivery: {sequence:?}");
                    resolutions += 1;
                }
                Effect::OpenThread => resolutions += 1,
                Effect::None => {}
            }
            assert!(delivered <= 1, "{sequence:?}");
            assert!(resolutions <= 1, "{sequence:?}");
            assert_eq!(phase.is_open(), resolutions == 0, "{sequence:?}");
            if late {
                assert!(!phase.is_open(), "past the deadline: {sequence:?}");
            }
            if let Event::Accept | Event::Fork = event {
                let answered = answer(event, phase);
                assert_eq!(
                    answered.is_ok(),
                    matches!(
                        (event, phase),
                        (Event::Accept, Phase::Accepted)
                            | (Event::Fork, Phase::Forked | Phase::TimedOut)
                    ),
                    "{sequence:?}"
                );
            }
        }
    }
}
