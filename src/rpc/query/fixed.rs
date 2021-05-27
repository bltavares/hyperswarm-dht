// Copyright 2019 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

use std::{collections::hash_map::Entry, collections::VecDeque, num::NonZeroUsize};

use fnv::FnvHashMap;
use wasm_timer::Instant;

use crate::kbucket::PEER_REQUEST_TIMEOUT;
use crate::rpc::query::peers::PeersIterState;
use crate::rpc::Peer;

/// A peer iterator for bootstrapping a query.
#[derive(Debug)]
pub struct FixedPeersIter {
    /// The permitted parallelism, i.e. number of pending results.
    parallelism: NonZeroUsize,

    /// The state of peers emitted by the iterator.
    peers: FnvHashMap<Peer, PeerState>,

    /// The backlog of peers that can still be emitted.
    iter: VecDeque<Peer>,

    /// The internal state of the iterator.
    state: State,

    /// The last peer so we can retry
    pub(crate) last_peer: Option<Peer>,

    start: Instant,
}

#[derive(Debug, PartialEq, Eq)]
enum State {
    Waiting { num_waiting: usize },
    Finished,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum PeerState {
    /// The iterator is waiting for a result to be reported back for the peer.
    Waiting { retries: u8 },

    /// The iterator has been informed that the attempt to contact the peer
    /// failed.
    Failed,

    /// The iterator has been informed of a successful result from the peer.
    Succeeded,
}

impl FixedPeersIter {
    pub fn new<I>(peers: I, parallelism: NonZeroUsize) -> Self
    where
        I: IntoIterator<Item = Peer>,
    {
        let peers = peers.into_iter().collect::<VecDeque<_>>();

        Self {
            parallelism,
            iter: peers,
            peers: Default::default(),
            state: State::Waiting { num_waiting: 0 },
            last_peer: None,
            start: Instant::now(),
        }
    }

    /// Callback for delivering the result of a successful request to a peer.
    ///
    /// If the iterator is currently waiting for a result from `peer`,
    /// the iterator state is updated and `true` is returned. In that
    /// case, after calling this function, `next` should eventually be
    /// called again to obtain the new state of the iterator.
    ///
    /// If the iterator is finished, it is not currently waiting for a
    /// result from `peer`, or a result for `peer` has already been reported,
    /// calling this function has no effect and `false` is returned.
    pub fn on_success(&mut self, peer: &Peer) -> bool {
        if let State::Waiting { num_waiting } = &mut self.state {
            if let Some(state @ PeerState::Waiting { .. }) = self.peers.get_mut(peer) {
                *state = PeerState::Succeeded;
                *num_waiting -= 1;
                return true;
            }
        }
        false
    }

    /// Callback for informing the iterator about a failed request to a peer.
    ///
    /// If the iterator is currently waiting for a result from `peer`,
    /// the iterator state is updated and `true` is returned. In that
    /// case, after calling this function, `next` should eventually be
    /// called again to obtain the new state of the iterator.
    ///
    /// If the iterator is finished, it is not currently waiting for a
    /// result from `peer`, or a result for `peer` has already been reported,
    /// calling this function has no effect and `false` is returned.
    pub fn on_failure(&mut self, peer: &Peer) -> bool {
        if let State::Waiting { num_waiting } = &mut self.state {
            if let Some(state @ PeerState::Waiting { .. }) = self.peers.get_mut(peer) {
                *state = PeerState::Failed;
                *num_waiting -= 1;
                return true;
            }
        }
        false
    }

    pub fn finish(&mut self) {
        if let State::Waiting { .. } = self.state {
            self.state = State::Finished
        }
    }

    /// Checks whether the iterator has finished.
    pub fn is_finished(&self) -> bool {
        self.state == State::Finished
    }

    pub fn next(&mut self) -> PeersIterState {
        match &mut self.state {
            State::Finished => PeersIterState::Finished,
            State::Waiting { num_waiting } => {
                if *num_waiting >= self.parallelism.get() {
                    return PeersIterState::WaitingAtCapacity;
                }
                loop {
                    match self.iter.pop_front() {
                        None => {
                            if *num_waiting == 0 || self.start.elapsed() >= PEER_REQUEST_TIMEOUT {
                                self.state = State::Finished;
                                return PeersIterState::Finished;
                            } else {
                                return PeersIterState::Waiting(None);
                            }
                        }
                        Some(p) => {
                            *num_waiting += 1;
                            self.start = Instant::now();
                            self.last_peer = Some(p.clone());
                            match self.peers.entry(p.clone()) {
                                Entry::Vacant(e) => {
                                    e.insert(PeerState::Waiting { retries: 0 });
                                }
                                _ => {}
                            }
                            return PeersIterState::Waiting(Some(p));
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn retry(&mut self) -> Option<Peer> {
        if let Some(peer) = self.last_peer.clone() {
            match self.peers.get_mut(&peer) {
                Some(PeerState::Waiting { retries: 1 }) => {
                    self.on_failure(&peer);
                    return None;
                }
                Some(PeerState::Waiting { retries }) => {
                    self.iter.push_front(peer.clone());
                    *retries += 1;
                    match &mut self.state {
                        State::Waiting { num_waiting } => {
                            *num_waiting -= 1;
                        }
                        _ => {}
                    }

                    return Some(peer);
                }
                _ => {
                    return None;
                }
            };
        }

        return None;
    }
}
