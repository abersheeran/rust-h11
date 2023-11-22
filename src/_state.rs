use crate::{Event, ProtocolError};
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::hash::Hash;

#[derive(Debug, Eq, PartialEq, Copy, Clone, Hash)]
pub enum Role {
    Client,
    Server,
}

#[derive(Debug, Eq, PartialEq, Copy, Clone, Hash)]
pub enum State {
    Idle,
    SendResponse,
    SendBody,
    Done,
    MustClose,
    Closed,
    Error,
    MightSwitchProtocol,
    SwitchedProtocol,
}

#[derive(Debug, Eq, PartialEq, Copy, Clone, Hash)]
pub enum Switch {
    SwitchUpgrade,
    SwitchConnect,
    Client,
}

#[derive(Debug, Eq, PartialEq, Copy, Clone, Hash)]
pub enum EventType {
    Request,
    InformationalResponse,
    NormalResponse,
    Data,
    EndOfMessage,
    ConnectionClosed,
    NeedData,
    Paused,
    // Combination of EventType and Sentinel
    RequestClient,                      // (Request, Switch::Client)
    InformationalResponseSwitchUpgrade, // (InformationalResponse, Switch::SwitchUpgrade)
    NormalResponseSwitchConnect,        // (NormalResponse, Switch::SwitchConnect)
}

impl From<&Event> for EventType {
    fn from(value: &Event) -> Self {
        match value {
            Event::Request(_) => EventType::Request,
            Event::NormalResponse(_) => EventType::NormalResponse,
            Event::InformationalResponse(_) => EventType::InformationalResponse,
            Event::Data(_) => EventType::Data,
            Event::EndOfMessage(_) => EventType::EndOfMessage,
            Event::ConnectionClosed(_) => EventType::ConnectionClosed,
            Event::NeedData() => EventType::NeedData,
            Event::Paused() => EventType::Paused,
        }
    }
}

pub struct ConnectionState {
    pub keep_alive: bool,
    pub pending_switch_proposals: HashSet<Switch>,
    pub states: HashMap<Role, State>,
}

impl ConnectionState {
    pub fn new() -> Self {
        ConnectionState {
            keep_alive: true,
            pending_switch_proposals: HashSet::new(),
            states: HashMap::from([(Role::Client, State::Idle), (Role::Server, State::Idle)]),
        }
    }

    pub fn process_error(&mut self, role: Role) {
        self.states.insert(role, State::Error);
        self._fire_state_triggered_transitions();
    }

    pub fn process_keep_alive_disabled(&mut self) {
        self.keep_alive = false;
        self._fire_state_triggered_transitions();
    }

    pub fn process_client_switch_proposal(&mut self, switch_event: Switch) {
        self.pending_switch_proposals.insert(switch_event);
        self._fire_state_triggered_transitions();
    }

    pub fn process_event(
        &mut self,
        role: Role,
        event_type: EventType,
        server_switch_event: Option<Switch>,
    ) -> Result<(), ProtocolError> {
        let mut _event_type = event_type;
        if let Some(server_switch_event) = server_switch_event {
            assert_eq!(role, Role::Server);
            if !self.pending_switch_proposals.contains(&server_switch_event) {
                return Err(ProtocolError::LocalProtocolError(
                    format!(
                        "Received server {:?} event without a pending proposal",
                        server_switch_event
                    )
                    .into(),
                ));
            }
            _event_type = match (event_type, server_switch_event) {
                (EventType::Request, Switch::Client) => EventType::RequestClient,
                (EventType::NormalResponse, Switch::SwitchConnect) => {
                    EventType::NormalResponseSwitchConnect
                }
                (EventType::InformationalResponse, Switch::SwitchUpgrade) => {
                    EventType::InformationalResponseSwitchUpgrade
                }
                _ => panic!(
                    "Can't handle event type {:?} when role={:?} and state={:?}",
                    _event_type, role, self.states[&role]
                ),
            };
        }
        if server_switch_event.is_none() && _event_type == EventType::NormalResponse {
            self.pending_switch_proposals.clear();
        }
        self._fire_event_triggered_transitions(role, _event_type)?;
        if _event_type == EventType::Request {
            assert_eq!(role, Role::Client);
            self._fire_event_triggered_transitions(Role::Server, EventType::RequestClient)?
        }
        self._fire_state_triggered_transitions();
        Ok(())
    }

    fn _fire_event_triggered_transitions(
        &mut self,
        role: Role,
        event_type: EventType,
    ) -> Result<(), ProtocolError> {
        let state = self.states[&role];
        let new_state = match (role, state, event_type) {
            (Role::Client, State::Idle, EventType::Request) => State::SendBody,
            (Role::Client, State::Idle, EventType::ConnectionClosed) => State::Closed,
            (Role::Client, State::SendBody, EventType::Data) => State::SendBody,
            (Role::Client, State::SendBody, EventType::EndOfMessage) => State::Done,
            (Role::Client, State::Done, EventType::ConnectionClosed) => State::Closed,
            (Role::Client, State::MustClose, EventType::ConnectionClosed) => State::Closed,
            (Role::Client, State::Closed, EventType::ConnectionClosed) => State::Closed,

            (Role::Server, State::Idle, EventType::ConnectionClosed) => State::Closed,
            (Role::Server, State::Idle, EventType::NormalResponse) => State::SendBody,
            (Role::Server, State::Idle, EventType::RequestClient) => State::SendResponse,
            (Role::Server, State::SendResponse, EventType::InformationalResponse) => {
                State::SendResponse
            }
            (Role::Server, State::SendResponse, EventType::NormalResponse) => State::SendBody,
            (Role::Server, State::SendResponse, EventType::InformationalResponseSwitchUpgrade) => {
                State::SwitchedProtocol
            }
            (Role::Server, State::SendResponse, EventType::NormalResponseSwitchConnect) => {
                State::SwitchedProtocol
            }
            (Role::Server, State::SendBody, EventType::Data) => State::SendBody,
            (Role::Server, State::SendBody, EventType::EndOfMessage) => State::Done,
            (Role::Server, State::Done, EventType::ConnectionClosed) => State::Closed,
            (Role::Server, State::MustClose, EventType::ConnectionClosed) => State::Closed,
            (Role::Server, State::Closed, EventType::ConnectionClosed) => State::Closed,
            _ => {
                return Err(ProtocolError::LocalProtocolError(
                    format!(
                        "Can't handle event type {:?} when role={:?} and state={:?}",
                        event_type, role, state
                    )
                    .into(),
                ))
            }
        };
        self.states.insert(role, new_state);
        Ok(())
    }

    fn _fire_state_triggered_transitions(&mut self) {
        loop {
            let start_states = self.states.clone();

            if self.pending_switch_proposals.len() > 0 {
                if self.states[&Role::Client] == State::Done {
                    self.states.insert(Role::Client, State::MightSwitchProtocol);
                }
            }

            if self.pending_switch_proposals.is_empty() {
                if self.states[&Role::Client] == State::MightSwitchProtocol {
                    self.states.insert(Role::Client, State::Done);
                }
            }

            if !self.keep_alive {
                for role in &[Role::Client, Role::Server] {
                    if self.states[role] == State::Done {
                        self.states.insert(*role, State::MustClose);
                    }
                }
            }

            let joint_state = (self.states[&Role::Client], self.states[&Role::Server]);
            let changes = match joint_state {
                (State::MightSwitchProtocol, State::SwitchedProtocol) => {
                    vec![(Role::Client, State::SwitchedProtocol)]
                }
                (State::Closed, State::Done) => {
                    vec![(Role::Server, State::MustClose)]
                }
                (State::Closed, State::Idle) => {
                    vec![(Role::Server, State::MustClose)]
                }
                (State::Error, State::Done) => vec![(Role::Server, State::MustClose)],
                (State::Done, State::Closed) => {
                    vec![(Role::Client, State::MustClose)]
                }
                (State::Idle, State::Closed) => {
                    vec![(Role::Client, State::MustClose)]
                }
                (State::Done, State::Error) => vec![(Role::Client, State::MustClose)],
                _ => vec![],
            };
            for (role, new_state) in changes {
                self.states.insert(role, new_state);
            }

            if self.states == start_states {
                return;
            }
        }
    }

    pub fn start_next_cycle(&mut self) -> Result<(), ProtocolError> {
        if self.states != HashMap::from([(Role::Client, State::Done), (Role::Server, State::Done)])
        {
            return Err(ProtocolError::LocalProtocolError(
                format!("Not in a reusable state. self.states={:?}", self.states).into(),
            ));
        }
        assert!(self.keep_alive);
        assert!(self.pending_switch_proposals.is_empty());
        self.states.clear();
        self.states.insert(Role::Client, State::Idle);
        self.states.insert(Role::Server, State::Idle);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_connection_state() {
        let mut cs = ConnectionState::new();

        // Basic event-triggered transitions

        assert_eq!(
            cs.states,
            HashMap::from([(Role::Client, State::Idle), (Role::Server, State::Idle)])
        );

        cs.process_event(Role::Client, EventType::Request, None)
            .unwrap();
        // The SERVER-Request special case:
        assert_eq!(
            cs.states,
            HashMap::from([
                (Role::Client, State::SendBody),
                (Role::Server, State::SendResponse)
            ])
        );

        // Illegal transitions raise an error and nothing happens
        cs.process_event(Role::Client, EventType::Request, None)
            .expect_err("Expected LocalProtocolError");
        assert_eq!(
            cs.states,
            HashMap::from([
                (Role::Client, State::SendBody),
                (Role::Server, State::SendResponse)
            ])
        );

        cs.process_event(Role::Server, EventType::InformationalResponse, None)
            .unwrap();
        assert_eq!(
            cs.states,
            HashMap::from([
                (Role::Client, State::SendBody),
                (Role::Server, State::SendResponse)
            ])
        );

        cs.process_event(Role::Server, EventType::NormalResponse, None)
            .unwrap();
        assert_eq!(
            cs.states,
            HashMap::from([
                (Role::Client, State::SendBody),
                (Role::Server, State::SendBody)
            ])
        );

        cs.process_event(Role::Client, EventType::EndOfMessage, None)
            .unwrap();
        cs.process_event(Role::Server, EventType::EndOfMessage, None)
            .unwrap();
        assert_eq!(
            cs.states,
            HashMap::from([(Role::Client, State::Done), (Role::Server, State::Done)])
        );

        // State-triggered transition

        cs.process_event(Role::Server, EventType::ConnectionClosed, None)
            .unwrap();
        assert_eq!(
            cs.states,
            HashMap::from([
                (Role::Client, State::MustClose),
                (Role::Server, State::Closed)
            ])
        );
    }

    #[test]
    fn test_connection_state_keep_alive() {
        // keep_alive = False
        let mut cs = ConnectionState::new();
        cs.process_event(Role::Client, EventType::Request, None)
            .unwrap();
        cs.process_keep_alive_disabled();
        cs.process_event(Role::Client, EventType::EndOfMessage, None)
            .unwrap();
        assert_eq!(
            cs.states,
            HashMap::from([
                (Role::Client, State::MustClose),
                (Role::Server, State::SendResponse)
            ])
        );

        cs.process_event(Role::Server, EventType::NormalResponse, None)
            .unwrap();
        cs.process_event(Role::Server, EventType::EndOfMessage, None)
            .unwrap();
        assert_eq!(
            cs.states,
            HashMap::from([
                (Role::Client, State::MustClose),
                (Role::Server, State::MustClose)
            ])
        );
    }

    #[test]
    fn test_connection_state_keep_alive_in_done() {
        // Check that if keep_alive is disabled when the CLIENT is already in DONE,
        // then this is sufficient to immediately trigger the DONE -> MUST_CLOSE
        // transition
        let mut cs = ConnectionState::new();
        cs.process_event(Role::Client, EventType::Request, None)
            .unwrap();
        cs.process_event(Role::Client, EventType::EndOfMessage, None)
            .unwrap();
        assert_eq!(cs.states[&Role::Client], State::Done);
        cs.process_keep_alive_disabled();
        assert_eq!(cs.states[&Role::Client], State::MustClose);
    }

    #[test]
    fn test_connection_state_switch_denied() {
        for switch_type in [Switch::SwitchConnect, Switch::SwitchUpgrade] {
            for deny_early in [true, false] {
                let mut cs = ConnectionState::new();
                cs.process_client_switch_proposal(switch_type);
                cs.process_event(Role::Client, EventType::Request, None)
                    .unwrap();
                cs.process_event(Role::Client, EventType::Data, None)
                    .unwrap();
                assert_eq!(
                    cs.states,
                    HashMap::from([
                        (Role::Client, State::SendBody),
                        (Role::Server, State::SendResponse)
                    ])
                );

                assert!(cs.pending_switch_proposals.contains(&switch_type));

                if deny_early {
                    // before client reaches DONE
                    cs.process_event(Role::Server, EventType::NormalResponse, None)
                        .unwrap();
                    assert!(cs.pending_switch_proposals.is_empty());
                }

                cs.process_event(Role::Client, EventType::EndOfMessage, None)
                    .unwrap();

                if deny_early {
                    assert_eq!(
                        cs.states,
                        HashMap::from([
                            (Role::Client, State::Done),
                            (Role::Server, State::SendBody)
                        ])
                    );
                } else {
                    assert_eq!(
                        cs.states,
                        HashMap::from([
                            (Role::Client, State::MightSwitchProtocol),
                            (Role::Server, State::SendResponse)
                        ])
                    );

                    cs.process_event(Role::Server, EventType::InformationalResponse, None)
                        .unwrap();
                    assert_eq!(
                        cs.states,
                        HashMap::from([
                            (Role::Client, State::MightSwitchProtocol),
                            (Role::Server, State::SendResponse)
                        ])
                    );

                    cs.process_event(Role::Server, EventType::NormalResponse, None)
                        .unwrap();
                    assert_eq!(
                        cs.states,
                        HashMap::from([
                            (Role::Client, State::Done),
                            (Role::Server, State::SendBody)
                        ])
                    );
                    assert!(cs.pending_switch_proposals.is_empty());
                }
            }
        }
    }

    #[test]
    fn test_connection_state_protocol_switch_accepted() {
        for switch_event in [Switch::SwitchUpgrade, Switch::SwitchConnect] {
            let mut cs = ConnectionState::new();
            cs.process_client_switch_proposal(switch_event);
            cs.process_event(Role::Client, EventType::Request, None)
                .unwrap();
            cs.process_event(Role::Client, EventType::Data, None)
                .unwrap();
            assert_eq!(
                cs.states,
                HashMap::from([
                    (Role::Client, State::SendBody),
                    (Role::Server, State::SendResponse)
                ])
            );

            cs.process_event(Role::Client, EventType::EndOfMessage, None)
                .unwrap();
            assert_eq!(
                cs.states,
                HashMap::from([
                    (Role::Client, State::MightSwitchProtocol),
                    (Role::Server, State::SendResponse)
                ])
            );

            cs.process_event(Role::Server, EventType::InformationalResponse, None)
                .unwrap();
            assert_eq!(
                cs.states,
                HashMap::from([
                    (Role::Client, State::MightSwitchProtocol),
                    (Role::Server, State::SendResponse)
                ])
            );

            cs.process_event(
                Role::Server,
                match switch_event {
                    Switch::SwitchUpgrade => EventType::InformationalResponse,
                    Switch::SwitchConnect => EventType::NormalResponse,
                    _ => panic!(),
                },
                Some(switch_event),
            )
            .unwrap();
            assert_eq!(
                cs.states,
                HashMap::from([
                    (Role::Client, State::SwitchedProtocol),
                    (Role::Server, State::SwitchedProtocol)
                ])
            );
        }
    }

    #[test]
    fn test_connection_state_double_protocol_switch() {
        // CONNECT + Upgrade is legal! Very silly, but legal. So we support
        // it. Because sometimes doing the silly thing is easier than not.
        for server_switch in [
            None,
            Some(Switch::SwitchUpgrade),
            Some(Switch::SwitchConnect),
        ] {
            let mut cs = ConnectionState::new();
            cs.process_client_switch_proposal(Switch::SwitchUpgrade);
            cs.process_client_switch_proposal(Switch::SwitchConnect);
            cs.process_event(Role::Client, EventType::Request, None)
                .unwrap();
            cs.process_event(Role::Client, EventType::EndOfMessage, None)
                .unwrap();
            assert_eq!(
                cs.states,
                HashMap::from([
                    (Role::Client, State::MightSwitchProtocol),
                    (Role::Server, State::SendResponse)
                ])
            );
            cs.process_event(
                Role::Server,
                match server_switch {
                    Some(Switch::SwitchUpgrade) => EventType::InformationalResponse,
                    Some(Switch::SwitchConnect) => EventType::NormalResponse,
                    None => EventType::NormalResponse,
                    _ => panic!(),
                },
                server_switch,
            )
            .unwrap();
            if server_switch.is_none() {
                assert_eq!(
                    cs.states,
                    HashMap::from([(Role::Client, State::Done), (Role::Server, State::SendBody)])
                );
            } else {
                assert_eq!(
                    cs.states,
                    HashMap::from([
                        (Role::Client, State::SwitchedProtocol),
                        (Role::Server, State::SwitchedProtocol)
                    ])
                );
            }
        }
    }

    #[test]
    fn test_connection_state_inconsistent_protocol_switch() {
        for (client_switches, server_switch) in [
            (vec![], Switch::SwitchUpgrade),
            (vec![], Switch::SwitchConnect),
            (vec![Switch::SwitchUpgrade], Switch::SwitchConnect),
            (vec![Switch::SwitchConnect], Switch::SwitchUpgrade),
        ] {
            let mut cs = ConnectionState::new();
            for client_switch in client_switches.clone() {
                cs.process_client_switch_proposal(client_switch);
            }
            cs.process_event(Role::Client, EventType::Request, None)
                .unwrap();
            cs.process_event(Role::Server, EventType::NormalResponse, Some(server_switch))
                .expect_err("Expected LocalProtocolError");
        }
    }

    #[test]
    fn test_connection_state_keepalive_protocol_switch_interaction() {
        // keep_alive=False + pending_switch_proposals
        let mut cs = ConnectionState::new();
        cs.process_client_switch_proposal(Switch::SwitchUpgrade);
        cs.process_event(Role::Client, EventType::Request, None)
            .unwrap();
        cs.process_keep_alive_disabled();
        cs.process_event(Role::Client, EventType::Data, None)
            .unwrap();
        assert_eq!(
            cs.states,
            HashMap::from([
                (Role::Client, State::SendBody),
                (Role::Server, State::SendResponse)
            ])
        );
    }

    #[test]
    fn test_connection_state_reuse() {
        let mut cs = ConnectionState::new();

        cs.start_next_cycle()
            .expect_err("Expected LocalProtocolError");

        cs.process_event(Role::Client, EventType::Request, None)
            .unwrap();
        cs.process_event(Role::Client, EventType::EndOfMessage, None)
            .unwrap();

        cs.start_next_cycle()
            .expect_err("Expected LocalProtocolError");

        cs.process_event(Role::Server, EventType::NormalResponse, None)
            .unwrap();
        cs.process_event(Role::Server, EventType::EndOfMessage, None)
            .unwrap();

        cs.start_next_cycle().unwrap();
        assert_eq!(
            cs.states,
            HashMap::from([(Role::Client, State::Idle), (Role::Server, State::Idle)])
        );

        // No keepalive

        cs.process_event(Role::Client, EventType::Request, None)
            .unwrap();
        cs.process_keep_alive_disabled();
        cs.process_event(Role::Client, EventType::EndOfMessage, None)
            .unwrap();
        cs.process_event(Role::Server, EventType::NormalResponse, None)
            .unwrap();
        cs.process_event(Role::Server, EventType::EndOfMessage, None)
            .unwrap();

        cs.start_next_cycle()
            .expect_err("Expected LocalProtocolError");

        // One side closed

        cs = ConnectionState::new();
        cs.process_event(Role::Client, EventType::Request, None)
            .unwrap();
        cs.process_event(Role::Client, EventType::EndOfMessage, None)
            .unwrap();
        cs.process_event(Role::Client, EventType::ConnectionClosed, None)
            .unwrap();
        cs.process_event(Role::Server, EventType::NormalResponse, None)
            .unwrap();
        cs.process_event(Role::Server, EventType::EndOfMessage, None)
            .unwrap();

        cs.start_next_cycle()
            .expect_err("Expected LocalProtocolError");

        // Succesful protocol switch

        cs = ConnectionState::new();
        cs.process_client_switch_proposal(Switch::SwitchUpgrade);
        cs.process_event(Role::Client, EventType::Request, None)
            .unwrap();
        cs.process_event(Role::Client, EventType::EndOfMessage, None)
            .unwrap();
        cs.process_event(
            Role::Server,
            EventType::InformationalResponse,
            Some(Switch::SwitchUpgrade),
        )
        .unwrap();

        cs.start_next_cycle()
            .expect_err("Expected LocalProtocolError");

        // Failed protocol switch

        cs = ConnectionState::new();
        cs.process_client_switch_proposal(Switch::SwitchUpgrade);
        cs.process_event(Role::Client, EventType::Request, None)
            .unwrap();
        cs.process_event(Role::Client, EventType::EndOfMessage, None)
            .unwrap();
        cs.process_event(Role::Server, EventType::NormalResponse, None)
            .unwrap();
        cs.process_event(Role::Server, EventType::EndOfMessage, None)
            .unwrap();

        cs.start_next_cycle().unwrap();
        assert_eq!(
            cs.states,
            HashMap::from([(Role::Client, State::Idle), (Role::Server, State::Idle)])
        );
    }

    #[test]
    fn test_server_request_is_illegal() {
        // There used to be a bug in how we handled the Request special case that
        // made this allowed...
        let mut cs = ConnectionState::new();
        cs.process_event(Role::Server, EventType::Request, None)
            .expect_err("Expected LocalProtocolError");
    }
}
