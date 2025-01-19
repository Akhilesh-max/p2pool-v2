// Copyright (C) 2024 [Kulpreet Singh]
//
//  This file is part of P2Poolv2
//
// P2Poolv2 is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free 
// Software Foundation, either version 3 of the License, or (at your option)
// any later version.
//
// P2Poolv2 is distributed in the hope that it will be useful, but WITHOUT ANY
// WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS 
// FOR A PARTICULAR PURPOSE. See the GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License along with 
// P2Poolv2. If not, see <https://www.gnu.org/licenses/>. 

pub mod behaviour;

use libp2p::{
    gossipsub, kad::{Event as KademliaEvent, QueryResult}, swarm::SwarmEvent, Multiaddr, Swarm
};
use tracing::{debug, error, info};
use std::time::Duration;
pub use crate::config::Config;
use libp2p::identify;
use libp2p::mdns::Event as MdnsEvent;
pub mod actor;
pub mod messages;
use behaviour::{P2PoolBehaviour, P2PoolBehaviourEvent};
use crate::node::behaviour::request_response::RequestResponseEvent; 
use crate::node::messages::{InventoryMessage, Message};
use crate::shares::chain::Chain;
use crate::shares::store::Store;

/// Node is the main struct that represents the node
struct Node {
    swarm: Swarm<P2PoolBehaviour>,
    share_topic: gossipsub::IdentTopic,
    chain: Chain,
}

impl Node {
    pub fn new(config: &Config) -> Result<Self, Box<dyn std::error::Error>> {
        let store = Store::new(config.store.path.clone());
        let chain = Chain::new(store);

        let id_keys = libp2p::identity::Keypair::generate_ed25519();
        let peer_id = id_keys.public().to_peer_id();

        let behavior = match P2PoolBehaviour::new(&id_keys) {
            Ok(behavior) => behavior,
            Err(err) => {
                error!("Failed to create P2PoolBehaviour: {}", err);
                std::process::exit(1);
            }
        };

        let mut swarm = libp2p::SwarmBuilder::with_existing_identity(id_keys)
            .with_tokio()
            .with_tcp(
                libp2p::tcp::Config::default(),
                libp2p::noise::Config::new,
                libp2p::yamux::Config::default,
            )?
            .with_behaviour(|_| behavior)?
            .with_swarm_config(|cfg| cfg.with_idle_connection_timeout(Duration::from_secs(u64::MAX)))
            .build();

        
        swarm.listen_on(config.network.listen_address.parse()?)?;

        for peer_addr in &config.network.dial_peers {
            match peer_addr.parse::<Multiaddr>() {
                Ok(remote) => {
                    if let Err(e) = swarm.dial(remote) {
                        debug!("Failed to dial {}: {}", peer_addr, e);
                    } else {
                        info!("Dialed {}", peer_addr);
                    }
                }
                Err(e) => debug!("Invalid multiaddr {}: {}", peer_addr, e),
            }
        }

        let share_topic = gossipsub::IdentTopic::new("share");
        if let Err(e) = swarm.behaviour_mut().gossipsub.subscribe(&share_topic) {
            error!("Failed to subscribe to share topic: {}", e);
        }

        Ok(Self { swarm, share_topic, chain })
    }

    /// Returns a Vec of peer IDs that are currently connected to this node
    pub fn connected_peers(&self) -> Vec<libp2p::PeerId> {
        self.swarm.connected_peers().cloned().collect()
    }

    pub fn shutdown(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        for peer_id in self.swarm.connected_peers().cloned().collect::<Vec<_>>() {
            self.swarm.disconnect_peer_id(peer_id).unwrap_or_default();
        }
        Ok(())
    }
    
    /// Send a share to the network
    pub fn send_gossip(&mut self, buf: Vec<u8>) {
        if let Err(e) = self.swarm.behaviour_mut().gossipsub.publish(self.share_topic.clone(), buf) {
            error!("Failed to send share: {}", e);  
        }
    }

    /// Send a message to a specific peer
    pub fn send_to_peer(&mut self, peer_id: libp2p::PeerId, message: Message) {
        info!("Sending message to peer: {peer_id}, message: {message:?}");
        self.swarm.behaviour_mut().request_response.send_request(&peer_id, message);
    }

    /// Handle swarm events, these are events that are generated by the libp2p library
    pub fn handle_swarm_event(&mut self, event: SwarmEvent<P2PoolBehaviourEvent>) {
        match event {
            SwarmEvent::NewListenAddr { address, .. } => info!("Listening on {address:?}"),
            SwarmEvent::ConnectionEstablished { peer_id, endpoint,.. } => {
                info!("Connected to peer: {peer_id} {endpoint:?}");
                self.handle_connection_established(peer_id);
            },
            SwarmEvent::ConnectionClosed { peer_id, .. } => {
                info!("Disconnected from peer: {peer_id}");
                self.swarm.behaviour_mut().remove_peer(&peer_id);
            },
            SwarmEvent::Behaviour(event) => {
                match event {
                    P2PoolBehaviourEvent::Mdns(mdns_event) => {
                        self.handle_mdns_event(mdns_event);
                    },
                    P2PoolBehaviourEvent::Identify(identify_event) => {
                        self.handle_identify_event(identify_event);
                    },
                    P2PoolBehaviourEvent::Kademlia(kad_event) => {
                        self.handle_kademlia_event(kad_event);
                    },
                    P2PoolBehaviourEvent::Gossipsub(gossip_event) => {
                        self.handle_gossipsub_event(gossip_event);
                    }, 
                    P2PoolBehaviourEvent::RequestResponse(request_response_event) => {
                        self.handle_request_response_event(request_response_event);
                    },
                }
            },
            _ => {}
        }
    }

    /// Handle identify events, these are events that are generated by the identify protocol
    fn handle_identify_event(&mut self, event: identify::Event) {
        match event {
            identify::Event::Received { peer_id, info } => {
                info!("Identified Peer {} with protocol version {}", peer_id, info.protocol_version);
                // Add the peer's advertised addresses to Kademlia
                for addr in info.listen_addrs {
                    self.swarm.behaviour_mut().add_address(peer_id, addr.clone());
                }
            },
            _ => {
                debug!("Other identify event: {:?}", event);
            }
        }
    }

    /// Handle mdns events, these are events that are generated by the mdns protocol
    fn handle_mdns_event(&mut self, event: MdnsEvent) {
        match event {
            MdnsEvent::Discovered(discovered) => {
                info!("Discovered peer: {:?}", discovered);
                for (peer_id, addr) in discovered {
                    // Check if we're not already connected to this peer
                    if !self.swarm.is_connected(&peer_id) {
                        // Try to dial the discovered peer
                        match self.swarm.dial(addr.clone()) {
                            Ok(_) => {
                                // Add the peer's address to Kademlia
                                self.swarm.behaviour_mut().add_address(peer_id, addr);
                            }
                            Err(e) => debug!("Failed to dial discovered peer {}: {}", peer_id, e),
                        }
                    }
                }
            },
            _ => debug!("Other Mdns event: {:?}", event),
        }
    }

    /// Handle kademlia events, these are events that are generated by the kademlia protocol
    fn handle_kademlia_event(&mut self, event: KademliaEvent) {
        match event {
            KademliaEvent::RoutingUpdated { peer, is_new_peer, addresses, bucket_range, old_peer } => {
                info!("Routing updated for peer: {peer}, is_new_peer: {is_new_peer}, addresses: {addresses:?}, bucket_range: {bucket_range:?}, old_peer: {old_peer:?}");
            },
            KademliaEvent::OutboundQueryProgressed { result, .. } => {
                match result {
                    QueryResult::GetClosestPeers(Ok(ok)) => {
                        debug!("Got closest peers: {:?}", ok.peers);
                    },
                    QueryResult::GetClosestPeers(Err(err)) => {
                        debug!("Failed to get closest peers: {err}");
                    },
                    _ => debug!("Other query result: {:?}", result),
                }
            },
            _ => debug!("Other Kademlia event: {:?}", event),
        }
    }

    /// Handle gossipsub events, these are events that are generated by the gossipsub protocol
    fn handle_gossipsub_event(&mut self, event: gossipsub::Event) {
        info!("Gossipsub event: {:?}", event);
    }

    /// Handle request-response events, these are events that are generated by the request-response protocol
    fn handle_request_response_event(&mut self, event: RequestResponseEvent<Message, Message>) {
        info!("Request-response event: {:?}", event);
    }

    /// Handle connection established events, these are events that are generated when a connection is established
    fn handle_connection_established(&mut self, peer_id: libp2p::PeerId) {
        info!("Connection established with peer: {peer_id}");
        self.send_inventory(peer_id);
    }

    /// Send inventory message to a specific peer
    /// For now we just send the tip of the chain
    fn send_inventory(&mut self, peer_id: libp2p::PeerId) {
        if let Some(tip) = self.chain.tip.clone() {
            info!("Sending inventory message to peer: {peer_id}, tip: {tip:?}");
            let inventory_msg = Message::Inventory(InventoryMessage {
                have_shares: vec![tip],
            });
            self.send_to_peer(peer_id, inventory_msg);
        }
    }
} 
