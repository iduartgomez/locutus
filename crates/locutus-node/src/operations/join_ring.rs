use std::collections::HashSet;

use rust_fsm::*;

use super::{OpError, OperationResult};
use crate::{
    conn_manager::{self, ConnectionBridge, PeerKey, PeerKeyLocation},
    message::{Message, Transaction, TransactionType},
    node::{OpExecutionError, OpStateStorage},
    operations::Operation,
    ring::{Location, Ring},
};

pub(crate) use self::messages::{JoinRequest, JoinResponse, JoinRingMsg};

pub(crate) struct JoinRingOp(StateMachine<JROpSM>);

impl JoinRingOp {
    pub fn initial_request(
        req_peer: PeerKey,
        target_loc: PeerKeyLocation,
        max_hops_to_live: usize,
    ) -> Self {
        let mut sm = StateMachine::new();
        sm.consume(&JoinRingMsg::Req {
            id: Transaction::new(<JoinRingMsg as TransactionType>::tx_type_id()),
            msg: JoinRequest::Initial {
                req_peer,
                target_loc,
                max_hops_to_live,
                // initially is the max hops, will be decreased over each hop
                hops_to_live: max_hops_to_live,
            },
        })
        .unwrap();
        JoinRingOp(sm)
    }
}

#[derive(Debug)]
struct JROpSM;

impl StateMachineImpl for JROpSM {
    type Input = JoinRingMsg;

    type State = JRState;

    type Output = JoinRingMsg;

    const INITIAL_STATE: Self::State = JRState::Initializing;

    fn transition(state: &Self::State, input: &Self::Input) -> Option<Self::State> {
        match (state, input) {
            (
                JRState::Initializing,
                JoinRingMsg::Req {
                    msg:
                        JoinRequest::Initial {
                            req_peer,
                            target_loc,
                            max_hops_to_live,
                            ..
                        },
                    ..
                },
            ) => Some(JRState::Connecting(ConnectionInfo {
                gateway: *target_loc,
                this_peer: *req_peer,
                max_hops_to_live: *max_hops_to_live,
            })),
            (
                JRState::Connecting { .. } | JRState::Initializing,
                JoinRingMsg::Resp {
                    msg: JoinResponse::ReceivedOC { .. },
                    ..
                },
            ) => Some(JRState::OCReceived),
            (
                JRState::Connecting { .. } | JRState::OCReceived,
                JoinRingMsg::Req { .. } | JoinRingMsg::Connected { .. },
            ) => Some(JRState::Connected),
            (JRState::Connected, _) => None,
            _ => None,
        }
    }

    fn output(state: &Self::State, input: &Self::Input) -> Option<Self::Output> {
        match (state, input) {
            (
                JRState::Initializing,
                JoinRingMsg::Req {
                    id,
                    msg:
                        JoinRequest::Initial {
                            target_loc,
                            req_peer,
                            ..
                        },
                },
            ) => Some(JoinRingMsg::Resp {
                id: *id,
                msg: JoinResponse::ReceivedOC {
                    by_peer: *target_loc,
                },
                sender: PeerKeyLocation {
                    peer: *req_peer,
                    location: None,
                },
            }),
            (
                JRState::Initializing | JRState::Connecting(_),
                JoinRingMsg::Resp {
                    msg: JoinResponse::ReceivedOC { .. },
                    ..
                }
                | JoinRingMsg::Connected,
            ) => Some(JoinRingMsg::Connected),
            (JRState::OCReceived, JoinRingMsg::Connected) => Some(JoinRingMsg::Connected),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
enum JRState {
    Initializing,
    Connecting(ConnectionInfo),
    OCReceived,
    Connected,
}

#[derive(Debug, Clone)]
struct ConnectionInfo {
    gateway: PeerKeyLocation,
    this_peer: PeerKey,
    max_hops_to_live: usize,
}

impl JRState {
    fn try_unwrap_connecting(self) -> Result<ConnectionInfo, OpError> {
        if let Self::Connecting(conn_info) = self {
            Ok(conn_info)
        } else {
            Err(OpError::IllegalStateTransition)
        }
    }
}

/// Join ring routine, called upon processing a request to join or while performing
/// a join operation for this node after initial request (see [`initial_join_request`]).
///
/// # Cancellation Safety
/// This future is not cancellation safe.
pub(crate) async fn join_ring_op<CB>(
    op_storage: &mut OpStateStorage,
    conn_manager: &mut CB,
    join_op: JoinRingMsg,
) -> Result<(), OpError>
where
    CB: ConnectionBridge,
{
    let sender;
    let tx = *join_op.id();
    let result = match op_storage.pop(join_op.id()) {
        Some(Operation::JoinRing(state)) => {
            sender = join_op.sender().cloned();
            // was an existing operation, the other peer messaged back
            update_state(conn_manager, state, join_op, &op_storage.ring).await
        }
        Some(_) => return Err(OpExecutionError::TxUpdateFailure(tx).into()),
        None => {
            sender = join_op.sender().cloned();
            // new request to join from this node, initialize the machine
            let machine = JoinRingOp(StateMachine::new());
            update_state(conn_manager, machine, join_op, &op_storage.ring).await
        }
    };

    match result {
        Err(err) => {
            log::error!("error while processing join request: {}", err);
            if let Some(sender) = sender {
                conn_manager.send(&sender, Message::Canceled(tx)).await?;
            }
            return Err(err);
        }
        Ok(OperationResult {
            return_msg: Some(msg),
            state: Some(updated_state),
        }) => {
            // updated op
            let id = *msg.id();
            if let Some(target) = msg.sender().cloned() {
                conn_manager.send(&target, msg).await?;
            }
            op_storage.push(id, Operation::JoinRing(updated_state))?;
        }
        Ok(OperationResult {
            return_msg: Some(msg),
            state: None,
        }) => {
            // finished the operation at this node, informing back
            if let Some(target) = msg.sender().cloned() {
                conn_manager.send(&target, msg).await?;
            }
        }
        Ok(OperationResult {
            return_msg: None,
            state: None,
        }) => {
            // operation finished_completely
        }
        _ => unreachable!(),
    }
    Ok(())
}

#[inline(always)]
async fn update_state<CB>(
    conn_manager: &mut CB,
    state: JoinRingOp,
    other_host_msg: JoinRingMsg,
    ring: &Ring,
) -> Result<OperationResult<JoinRingOp>, OpError>
where
    CB: ConnectionBridge,
{
    let return_msg;
    let new_state;
    match other_host_msg {
        JoinRingMsg::Req {
            id,
            msg:
                JoinRequest::Initial {
                    target_loc: your_location,
                    req_peer,
                    hops_to_live,
                    max_hops_to_live,
                },
        } => {
            log::debug!(
                "Initial join request received by {} with HTL {}",
                req_peer,
                hops_to_live
            );

            let new_location = Location::random();
            let accepted_by = if ring.should_accept(
                &your_location
                    .location
                    .ok_or(OpExecutionError::TxUpdateFailure(id))?,
                &new_location,
            ) {
                log::debug!(
                    "Accepting connections from {}, establising connection @ {}",
                    req_peer,
                    &your_location.peer
                );
                // FIXME: self_cp.establish_conn(peer_key_loc, tx);
                vec![your_location]
            } else {
                log::debug!("Not accepting new connection for sender {}", req_peer);
                Vec::new()
            };

            log::debug!(
                "Sending JoinResponse to {} accepting {} connections",
                req_peer,
                accepted_by.len()
            );

            let join_response = Message::from(JoinRingMsg::Resp {
                id,
                sender: your_location,
                msg: JoinResponse::Initial {
                    accepted_by: accepted_by.clone(),
                    your_location: new_location,
                    your_peer_id: req_peer,
                },
            });
            let new_peer_loc = PeerKeyLocation {
                location: Some(new_location),
                peer: req_peer,
            };

            if hops_to_live > 0 && !ring.connections_by_location.read().is_empty() {
                let forward_to = if hops_to_live >= ring.rnd_if_htl_above {
                    log::debug!(
                        "Randomly selecting peer to forward JoinRequest, sender: {}",
                        req_peer
                    );
                    ring.random_peer(|p| p.peer != req_peer)
                } else {
                    log::debug!(
                        "Selecting close peer to forward request, sender: {}",
                        req_peer
                    );
                    ring.connections_by_location
                        .read()
                        .get(&new_location)
                        .filter(|it| it.peer != req_peer)
                        .copied()
                };

                if let Some(forward_to) = forward_to {
                    let forwarded = Message::from(JoinRingMsg::Req {
                        id,
                        msg: JoinRequest::Proxy {
                            joiner: new_peer_loc,
                            hops_to_live: hops_to_live.min(ring.max_hops_to_live) - 1,
                        },
                    });
                    log::debug!(
                        "Forwarding JoinRequest from sender {} to {}",
                        req_peer,
                        forward_to.peer
                    );
                    conn_manager.send(&forward_to, forwarded).await?;
                    let _forwarded_acceptors = accepted_by.into_iter().collect::<HashSet<_>>();
                    // this will would jump to JoinRingMsg::Resp::JoinResponse::Proxy after peer return
                    // TODO: add a new state that transits from Connecting -> WaitingProxyResponse
                    todo!()
                } else {
                    new_state = Some(state);
                    return_msg = Some(join_response);
                }
            } else {
                new_state = Some(state);
                return_msg = Some(join_response);
            }
        }
        JoinRingMsg::Req {
            id,
            msg:
                JoinRequest::Proxy {
                    joiner,
                    hops_to_live,
                },
        } => {
            todo!()
        }
        JoinRingMsg::Resp {
            id,
            sender,
            msg:
                JoinResponse::Initial {
                    accepted_by,
                    your_location,
                    your_peer_id,
                },
        } => {
            log::debug!("JoinResponse received from {}", sender.peer,);
            // state.0.consume(input);

            // let loc = &mut *ring.location.write();
            // *loc = Some(your_location);
            // let self_location = &*ring_proto.location.read();
            // let self_location = &self_location.ok_or(conn_manager::ConnError::LocationUnknown)?;
            // for new_peer_key in accepted_by {
            //     if ring_proto.ring.should_accept(
            //         self_location,
            //         &new_peer_key
            //             .location
            //             .ok_or(conn_manager::ConnError::LocationUnknown)?,
            //     ) {
            //         log::info!("Establishing connection to {}", new_peer_key.peer);
            //         ring_proto.establish_conn(new_peer_key, tx);
            //     } else {
            //         log::debug!("Not accepting connection to {}", new_peer_key.peer);
            //     }
            // }
            todo!()
        }
        JoinRingMsg::Resp {
            id,
            sender,
            msg: JoinResponse::Proxy { accepted_by },
        } => {
            //         let register_acceptors =
            //             move |jr_sender: PeerKeyLocation, join_resp| -> conn_manager::Result<()> {
            //                 if let Message::JoinResponse(tx, resp) = join_resp {
            //                     let new_acceptors = match resp {
            //                         JoinResponse::Initial { accepted_by, .. } => accepted_by,
            //                         JoinResponse::Proxy { accepted_by, .. } => accepted_by,
            //                     };
            //                     let fa = &mut *forwarded_acceptors.lock();
            //                     new_acceptors.iter().for_each(|p| {
            //                         if !fa.contains(p) {
            //                             fa.insert(*p);
            //                         }
            //                     });
            //                     let msg = Message::from((
            //                         tx,
            //                         JoinResponse::Proxy {
            //                             accepted_by: new_acceptors,
            //                         },
            //                     ));
            //                     self_cp2.conn_manager.send(jr_sender, tx, msg)?;
            //                 };
            //                 Ok(())
            //             };
            todo!()
        }
        JoinRingMsg::Resp {
            id,
            sender,
            msg: JoinResponse::ReceivedOC { .. },
        } => {
            //
            todo!()
        }
        JoinRingMsg::Connected => todo!(),
    }

    Ok(OperationResult {
        return_msg,
        state: new_state,
    })
}

/// Join ring routine, called upon performing a join operation for this node.
pub(crate) async fn initial_join_request<CB>(
    op_storage: &mut OpStateStorage,
    conn_manager: &mut CB,
    join_op: JoinRingOp,
) -> Result<(), OpError>
where
    CB: ConnectionBridge,
{
    let ConnectionInfo {
        gateway,
        this_peer,
        max_hops_to_live,
    } = (&join_op.0).state().clone().try_unwrap_connecting()?;

    log::info!(
        "Joining ring via {} (@{})",
        gateway.peer,
        gateway
            .location
            .ok_or(conn_manager::ConnError::LocationUnknown)?
    );

    conn_manager.add_connection(gateway, true);
    let tx = Transaction::new(<JoinRingMsg as TransactionType>::tx_type_id());
    let join_req = Message::from(messages::JoinRingMsg::Req {
        id: tx,
        msg: messages::JoinRequest::Initial {
            target_loc: gateway,
            req_peer: this_peer,
            hops_to_live: max_hops_to_live,
            max_hops_to_live,
        },
    });
    log::debug!(
        "Sending initial join tx: {:?} to {}",
        join_req,
        gateway.peer
    );
    conn_manager.send(&gateway, join_req).await?;
    op_storage.push(tx, Operation::JoinRing(join_op))?;
    Ok(())
}

// fn establish_conn<CB>(conn_manager: &mut CB, new_peer: PeerKeyLocation, tx: Transaction)
// where
//     CB: ConnectionBridge,
// {
//     conn_manager.add_connection(new_peer, false);
//     let state = Arc::new(RwLock::new(messages::OpenConnection::Connecting));

//     let ack_peer = move |peer: PeerKeyLocation, msg: Message| -> conn_manager::Result<()> {
//         let (tx, oc) = match msg {
//             Message::OpenConnection(tx, oc) => (tx, oc),
//             msg => return Err(conn_manager::ConnError::UnexpectedResponseMessage(msg)),
//         };
//         current_state.transition(oc);
//         if !current_state.is_connected() {
//             let open_conn: Message = (tx, *current_state).into();
//             log::debug!("Acknowledging OC");
//             conn_manager.send(peer, *open_conn.id(), open_conn)?;
//         } else {
//             log::info!(
//                 "{} connected to {}, adding to ring",
//                 peer_key,
//                 new_peer.peer
//             );
//             conn_manager.send(
//                 peer,
//                 tx,
//                 Message::from((tx, messages::OpenConnection::Connected)),
//             )?;
//             ring.connections_by_location.write().insert(
//                 new_peer
//                     .location
//                     .ok_or(conn_manager::ConnError::LocationUnknown)?,
//                 new_peer,
//             );
//         }
//         Ok(())
//     };
//     self.conn_manager.listen_to_replies(tx, ack_peer);
//     let conn_manager = self.conn_manager.clone();
//     tokio::spawn(async move {
//         let curr_time = Instant::now();
//         let mut attempts = 0;
//         while !state.read().is_connected() && curr_time.elapsed() <= Duration::from_secs(30) {
//             log::debug!(
//                 "Sending {} to {}, number of messages sent: {}",
//                 *state.read(),
//                 new_peer.peer,
//                 attempts
//             );
//             conn_manager.send(new_peer, tx, Message::OpenConnection(tx, *state.read()))?;
//             attempts += 1;
//             tokio::time::sleep(Duration::from_millis(200)).await
//         }
//         if curr_time.elapsed() > Duration::from_secs(30) {
//             log::error!("Timed out trying to connect to {}", new_peer.peer);
//             Err(conn_manager::ConnError::NegotationFailed)
//         } else {
//             conn_manager.remove_listener(tx);
//             log::info!("Success negotiating connection to {}", new_peer.peer);
//             Ok(())
//         }
//     });
// }

mod messages {
    use super::*;
    use crate::{conn_manager::PeerKeyLocation, ring::Location};

    use serde::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
    pub(crate) enum JoinRingMsg {
        Req {
            id: Transaction,
            msg: JoinRequest,
        },
        Resp {
            id: Transaction,
            sender: PeerKeyLocation,
            msg: JoinResponse,
        },
        Connected,
    }

    impl JoinRingMsg {
        pub fn id(&self) -> &Transaction {
            use JoinRingMsg::*;
            match self {
                Req { id, .. } => id,
                Resp { id, .. } => id,
                Connected => todo!(),
            }
        }

        pub fn sender(&self) -> Option<&PeerKeyLocation> {
            use JoinRingMsg::*;
            match self {
                Req { .. } => None,
                Resp { sender, .. } => Some(sender),
                Connected => todo!(),
            }
        }
    }

    #[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
    pub(crate) enum JoinRequest {
        Initial {
            target_loc: PeerKeyLocation,
            req_peer: PeerKey,
            hops_to_live: usize,
            max_hops_to_live: usize,
        },
        Proxy {
            joiner: PeerKeyLocation,
            hops_to_live: usize,
        },
    }

    #[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
    pub(crate) enum JoinResponse {
        Initial {
            accepted_by: Vec<PeerKeyLocation>,
            your_location: Location,
            your_peer_id: PeerKey,
        },
        ReceivedOC {
            by_peer: PeerKeyLocation,
        },
        Proxy {
            accepted_by: Vec<PeerKeyLocation>,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use libp2p::identity::Keypair;

    use super::*;
    use crate::{
        config::tracing::Logger,
        message::TransactionTypeId,
        node::test_utils::{EventType, SimNetwork},
    };

    #[test]
    fn join_ring_transitions() {
        let id = Transaction::new(TransactionTypeId::JoinRing);
        let h1 = PeerKeyLocation {
            peer: PeerKey::from(Keypair::generate_ed25519().public()),
            location: None,
        };
        let h2 = PeerKeyLocation {
            peer: PeerKey::from(Keypair::generate_ed25519().public()),
            location: None,
        };

        let mut join_op_host_1 = StateMachine::<JROpSM>::new();
        let res = join_op_host_1
            .consume(&JoinRingMsg::Req {
                id,
                msg: JoinRequest::Initial {
                    target_loc: h1,
                    req_peer: h2.peer,
                    hops_to_live: 0,
                    max_hops_to_live: 0,
                },
            })
            .unwrap()
            .unwrap();
        let expected = JoinRingMsg::Resp {
            id,
            sender: h2,
            msg: JoinResponse::ReceivedOC { by_peer: h1 },
        };
        assert_eq!(res, expected);
        assert!(matches!(join_op_host_1.state(), JRState::Connecting(_)));

        let mut join_op_host_2 = StateMachine::<JROpSM>::new();
        let res = join_op_host_2.consume(&res).unwrap().unwrap();
        let expected = JoinRingMsg::Connected;
        assert_eq!(res, expected);
        assert!(matches!(join_op_host_2.state(), JRState::OCReceived));

        let res = join_op_host_1.consume(&res).unwrap().unwrap();
        let expected = JoinRingMsg::Connected;
        assert_eq!(res, expected);
        assert!(matches!(join_op_host_1.state(), JRState::Connected));

        let res = join_op_host_2.consume(&res).unwrap().unwrap();
        let expected = JoinRingMsg::Connected;
        assert_eq!(res, expected);
        assert!(matches!(join_op_host_2.state(), JRState::Connected));

        // transaction finished, should not return anymore
        assert!(join_op_host_1.consume(&res).is_err());
        assert!(join_op_host_2.consume(&res).is_err());
        assert!(matches!(join_op_host_1.state(), JRState::Connected));
        assert!(matches!(join_op_host_2.state(), JRState::Connected));
    }

    // #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn node0_to_gateway_conn() -> Result<(), Box<dyn std::error::Error>> {
        //! Given a network of one node and one gateway test that both are connected.
        Logger::init_logger();
        let mut sim_net = SimNetwork::build(1, 1, 0);
        tokio::time::sleep(Duration::from_secs(300)).await;
        match tokio::time::timeout(Duration::from_secs(300), sim_net.recv_net_events()).await {
            Ok(Some(Ok(event))) => match event.event {
                EventType::JoinSuccess { gateway, new_node } => {
                    log::info!("Successful join op between {} and {}", gateway, new_node);
                    Ok(())
                }
            },
            _ => Err("no event received".into()),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn all_nodes_should_connect() -> Result<(), Box<dyn std::error::Error>> {
        //! Given a network of 1000 peers all nodes should have connections.
        Logger::init_logger();

        let _sim_nodes = SimNetwork::build(10, 10, 7);
        // tokio::time::sleep(Duration::from_secs(300)).await;
        // let _hist: Vec<_> = _ring_distribution(sim_nodes.values()).collect();

        // FIXME: enable probing
        // const NUM_PROBES: usize = 10;
        // let mut probe_responses = Vec::with_capacity(NUM_PROBES);
        // for probe_idx in 0..NUM_PROBES {
        //     let target = Location::random();
        //     let idx: usize = rand::thread_rng().gen_range(0..sim_nodes.len());
        //     let rnd_node = sim_nodes
        //         .get_mut(&format!("node-{}", idx))
        //         .ok_or("node not found")?;
        //     let probe_response = ProbeProtocol::probe(
        //         rnd_node.ring_protocol.clone(),
        //         Transaction::new(<ProbeRequest as TransactionType>::msg_type_id()),
        //         ProbeRequest {
        //             hops_to_live: 7,
        //             target,
        //         },
        //     )
        //     .await
        //     .expect("failed to get probe response");
        //     probe_responses.push(probe_response);
        // }
        // probe_proto::utils::plot_probe_responses(probe_responses);

        // let any_empties = sim_nodes
        //     .peers
        //     .values()
        //     .map(|node| {
        //         node.op_storage
        //             .ring
        //             .connections_by_location
        //             .read()
        //             .is_empty()
        //     })
        //     .any(|is_empty| is_empty);
        // assert!(!any_empties);

        Ok(())
    }
}
