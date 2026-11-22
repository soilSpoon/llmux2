//! Explicit state machine for one request's attempt loop.
//!
//! `State::step` is pure: it maps (state, event) to the next state plus the
//! effect the driver in `Executor::execute` must perform. The TLA+ model in
//! `tla/RequestPath.tla` mirrors these transitions.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Choosing the next deployment candidate.
    Select,
    /// Waiting for a permit on the selected deployment.
    Admit,
    /// Building the upstream request.
    Build,
    /// Rechecking the permit after the build (credentials may have changed cooldown).
    Gate,
    /// Sending upstream.
    Send,
    /// Waiting for a credential refresh.
    Refresh,
    /// A response was committed; no further attempt may start.
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Built {
    Unsupported,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Success,
    Refreshable { can_refresh: bool },
    AuthFatal,
    Limited { fallback: bool },
    QuotaExhausted { fallback: bool },
    ModelNotFound,
    Transient,
    Fatal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    Ineligible,
    Cooling,
    DeadlinePassed,
    Eligible,
    Admit { permit: bool },
    Built(Built),
    Gate { permit: bool },
    Result(Outcome),
    Refreshed(bool),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    Admit,
    Build,
    Send,
    Refresh,
    Skip { unavailable: bool },
    Incompatible,
    StopGroups,
    FailTimeout,
    Apply { outcome: Outcome, stop_groups: bool },
    Respond,
    Abort,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct State {
    pub phase: Phase,
    pub attempts: u32,
    pub max_attempts: u32,
    pub refreshed: bool,
}

impl State {
    pub fn new(max_attempts: u32) -> Self {
        Self {
            phase: Phase::Select,
            attempts: 0,
            max_attempts,
            refreshed: false,
        }
    }

    /// Starts a new deployment candidate.
    pub fn select(&mut self) {
        self.phase = Phase::Select;
        self.refreshed = false;
    }

    fn exhausted(&self) -> bool {
        self.attempts >= self.max_attempts
    }

    pub fn step(&mut self, event: Event) -> Effect {
        use Event as E;
        use Phase as P;
        match (self.phase, event) {
            (P::Select, _) if self.exhausted() => Effect::StopGroups,
            (P::Select, E::Ineligible) => Effect::Skip { unavailable: false },
            (P::Select, E::Cooling) => Effect::Skip { unavailable: true },
            (P::Select, E::DeadlinePassed) => Effect::FailTimeout,
            (P::Select, E::Eligible) => {
                self.phase = P::Admit;
                Effect::Admit
            }
            (P::Admit, _) if self.exhausted() => Effect::StopGroups,
            (P::Admit, E::Admit { permit: false, .. }) => Effect::Skip { unavailable: true },
            (P::Admit, E::Admit { .. }) => {
                self.phase = P::Build;
                Effect::Build
            }
            (P::Build, E::Built(Built::Unsupported)) => Effect::Incompatible,
            (P::Build, E::Built(Built::Other)) => {
                self.phase = P::Gate;
                Effect::Admit
            }
            (P::Gate, E::Gate { permit: false }) => Effect::Skip { unavailable: true },
            (P::Gate, E::Gate { permit: true }) => {
                self.attempts += 1;
                self.phase = P::Send;
                Effect::Send
            }
            (P::Send, E::Result(Outcome::Success)) => {
                self.phase = P::Done;
                Effect::Respond
            }
            (P::Send, E::Result(Outcome::Refreshable { can_refresh })) => {
                if can_refresh && !self.refreshed {
                    self.refreshed = true;
                    self.phase = P::Refresh;
                    return Effect::Refresh;
                }
                Effect::Apply {
                    outcome: Outcome::AuthFatal,
                    stop_groups: false,
                }
            }
            (P::Send, E::Result(Outcome::Fatal)) => Effect::Abort,
            (
                P::Send,
                E::Result(
                    outcome @ (Outcome::Limited { fallback: false }
                    | Outcome::QuotaExhausted { fallback: false }),
                ),
            ) => Effect::Apply {
                outcome,
                stop_groups: true,
            },
            (P::Send, E::Result(outcome)) => Effect::Apply {
                outcome,
                stop_groups: false,
            },
            (P::Refresh, E::Refreshed(true)) => {
                self.phase = P::Admit;
                Effect::Admit
            }
            (P::Refresh, E::Refreshed(false)) => Effect::Apply {
                outcome: Outcome::AuthFatal,
                stop_groups: false,
            },
            (phase, event) => unreachable!("invalid transition {phase:?} on {event:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Effect as F;
    use Event as E;
    use Phase as P;

    fn at(phase: Phase, attempts: u32, refreshed: bool) -> State {
        State {
            phase,
            attempts,
            max_attempts: 3,
            refreshed,
        }
    }

    #[test]
    fn transition_table_covers_every_documented_edge() {
        let lim = |fallback| Outcome::Limited { fallback };
        let table: Vec<(State, Event, Effect, Phase, u32)> = vec![
            (
                at(P::Select, 3, false),
                E::Eligible,
                F::StopGroups,
                P::Select,
                3,
            ),
            (
                at(P::Select, 0, false),
                E::Ineligible,
                F::Skip { unavailable: false },
                P::Select,
                0,
            ),
            (
                at(P::Select, 0, false),
                E::Cooling,
                F::Skip { unavailable: true },
                P::Select,
                0,
            ),
            (
                at(P::Select, 0, false),
                E::DeadlinePassed,
                F::FailTimeout,
                P::Select,
                0,
            ),
            (at(P::Select, 2, false), E::Eligible, F::Admit, P::Admit, 2),
            (
                at(P::Admit, 3, true),
                E::Admit { permit: true },
                F::StopGroups,
                P::Admit,
                3,
            ),
            (
                at(P::Admit, 0, false),
                E::Admit { permit: false },
                F::Skip { unavailable: true },
                P::Admit,
                0,
            ),
            (
                at(P::Admit, 0, false),
                E::Admit { permit: true },
                F::Build,
                P::Build,
                0,
            ),
            (
                at(P::Build, 0, false),
                E::Built(Built::Unsupported),
                F::Incompatible,
                P::Build,
                0,
            ),
            (
                at(P::Build, 0, false),
                E::Built(Built::Other),
                F::Admit,
                P::Gate,
                0,
            ),
            (
                at(P::Gate, 0, false),
                E::Gate { permit: false },
                F::Skip { unavailable: true },
                P::Gate,
                0,
            ),
            (
                at(P::Gate, 2, false),
                E::Gate { permit: true },
                F::Send,
                P::Send,
                3,
            ),
            (
                at(P::Send, 1, false),
                E::Result(Outcome::Success),
                F::Respond,
                P::Done,
                1,
            ),
            (
                at(P::Send, 1, false),
                E::Result(Outcome::Refreshable { can_refresh: true }),
                F::Refresh,
                P::Refresh,
                1,
            ),
            (
                at(P::Send, 2, true),
                E::Result(Outcome::Refreshable { can_refresh: true }),
                F::Apply {
                    outcome: Outcome::AuthFatal,
                    stop_groups: false,
                },
                P::Send,
                2,
            ),
            (
                at(P::Send, 1, false),
                E::Result(Outcome::Refreshable { can_refresh: false }),
                F::Apply {
                    outcome: Outcome::AuthFatal,
                    stop_groups: false,
                },
                P::Send,
                1,
            ),
            (
                at(P::Send, 1, false),
                E::Result(Outcome::Fatal),
                F::Abort,
                P::Send,
                1,
            ),
            (
                at(P::Send, 1, false),
                E::Result(lim(false)),
                F::Apply {
                    outcome: lim(false),
                    stop_groups: true,
                },
                P::Send,
                1,
            ),
            (
                at(P::Send, 1, false),
                E::Result(lim(true)),
                F::Apply {
                    outcome: lim(true),
                    stop_groups: false,
                },
                P::Send,
                1,
            ),
            (
                at(P::Send, 1, false),
                E::Result(Outcome::QuotaExhausted { fallback: false }),
                F::Apply {
                    outcome: Outcome::QuotaExhausted { fallback: false },
                    stop_groups: true,
                },
                P::Send,
                1,
            ),
            (
                at(P::Send, 1, false),
                E::Result(Outcome::ModelNotFound),
                F::Apply {
                    outcome: Outcome::ModelNotFound,
                    stop_groups: false,
                },
                P::Send,
                1,
            ),
            (
                at(P::Send, 1, false),
                E::Result(Outcome::Transient),
                F::Apply {
                    outcome: Outcome::Transient,
                    stop_groups: false,
                },
                P::Send,
                1,
            ),
            (
                at(P::Refresh, 1, true),
                E::Refreshed(true),
                F::Admit,
                P::Admit,
                1,
            ),
            (
                at(P::Refresh, 1, true),
                E::Refreshed(false),
                F::Apply {
                    outcome: Outcome::AuthFatal,
                    stop_groups: false,
                },
                P::Refresh,
                1,
            ),
        ];
        for (mut state, event, effect, phase, attempts) in table {
            let before = state;
            assert_eq!(state.step(event), effect, "{before:?} on {event:?}");
            assert_eq!(
                (state.phase, state.attempts),
                (phase, attempts),
                "{before:?} on {event:?}"
            );
        }
    }

    #[test]
    fn attempts_never_exceed_the_cap_and_none_starts_after_done() {
        let mut state = State::new(2);
        for _ in 0..5 {
            state.select();
            if state.step(E::Eligible) == F::StopGroups {
                break;
            }
            state.step(E::Admit { permit: true });
            state.step(E::Built(Built::Other));
            assert_eq!(state.step(E::Gate { permit: true }), F::Send);
            state.step(E::Result(Outcome::Transient));
        }
        assert_eq!(state.attempts, 2);
    }

    #[test]
    #[should_panic(expected = "invalid transition")]
    fn done_accepts_no_further_events() {
        at(P::Done, 1, false).step(E::Eligible);
    }
}
