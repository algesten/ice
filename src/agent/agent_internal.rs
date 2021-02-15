use super::*;
use crate::candidate::candidate_base::CandidateBaseConfig;
use crate::candidate::candidate_peer_reflexive::CandidatePeerReflexiveConfig;
use crate::util::*;

pub struct AgentInternal {
    // State owned by the taskLoop
    pub(crate) on_connected_tx: Option<mpsc::Sender<()>>,

    // State for closing
    pub(crate) done: Option<mpsc::Sender<()>>,
    pub(crate) chan_candidate: Option<mpsc::Sender<Arc<dyn Candidate + Send + Sync>>>,
    pub(crate) chan_candidate_pair: Option<mpsc::Sender<()>>,
    pub(crate) chan_state: Option<mpsc::Sender<ConnectionState>>,

    pub(crate) on_connection_state_change_hdlr: Option<OnConnectionStateChangeHdlrFn>,
    pub(crate) on_selected_candidate_pair_change_hdlr: Option<OnSelectedCandidatePairChangeHdlrFn>,
    pub(crate) on_candidate_hdlr: Option<OnCandidateHdlrFn>,
    pub(crate) selected_pair: Option<CandidatePair>,

    pub(crate) on_connected_rx: mpsc::Receiver<()>,

    // force candidate to be contacted immediately (instead of waiting for task ticker)
    pub(crate) force_candidate_contact: Option<mpsc::Receiver<bool>>,
    pub(crate) tie_breaker: u64,

    pub(crate) is_controlling: bool,
    pub(crate) lite: bool,
    pub(crate) start_time: Instant,
    pub(crate) nominated_pair: Option<CandidatePair>,

    pub(crate) connection_state: ConnectionState,
    pub(crate) gathering_state: GatheringState,

    pub(crate) mdns_mode: MulticastDNSMode,
    pub(crate) mdns_name: String,
    pub(crate) mdns_conn: Option<DNSConn>,

    pub(crate) started_ch_tx: Option<broadcast::Sender<()>>,

    pub(crate) max_binding_requests: u16,

    pub(crate) host_acceptance_min_wait: Duration,
    pub(crate) srflx_acceptance_min_wait: Duration,
    pub(crate) prflx_acceptance_min_wait: Duration,
    pub(crate) relay_acceptance_min_wait: Duration,

    pub(crate) port_min: u16,
    pub(crate) port_max: u16,

    pub(crate) candidate_types: Vec<CandidateType>,

    // How long connectivity checks can fail before the ICE Agent
    // goes to disconnected
    pub(crate) disconnected_timeout: Duration,

    // How long connectivity checks can fail before the ICE Agent
    // goes to failed
    pub(crate) failed_timeout: Duration,

    // How often should we send keepalive packets?
    // 0 means never
    pub(crate) keepalive_interval: Duration,

    // How often should we run our internal taskLoop to check for state changes when connecting
    pub(crate) check_interval: Duration,

    pub(crate) local_ufrag: String,
    pub(crate) local_pwd: String,
    pub(crate) local_candidates: HashMap<NetworkType, Vec<Arc<dyn Candidate + Send + Sync>>>,

    pub(crate) remote_ufrag: String,
    pub(crate) remote_pwd: String,
    pub(crate) remote_candidates: HashMap<NetworkType, Vec<Arc<dyn Candidate + Send + Sync>>>,

    pub(crate) checklist: Vec<CandidatePair>,

    pub(crate) urls: Vec<URL>,
    pub(crate) network_types: Vec<NetworkType>,

    pub(crate) buffer: Option<Buffer>,

    // LRU of outbound Binding request Transaction IDs
    pub(crate) pending_binding_requests: Vec<BindingRequest>,

    // 1:1 D-NAT IP address mapping
    pub(crate) ext_ip_mapper: ExternalIPMapper,

    //TODO: err  atomicError
    pub(crate) gather_candidate_cancel: Option<GatherCandidateCancelFn>,

    //TODO: net    *vnet.Net
    //TODO: tcpMux TCPMux
    pub(crate) interface_filter: Option<Box<dyn Fn(String) -> bool>>,

    pub(crate) insecure_skip_verify: bool,
    //TODO: proxyDialer proxy.Dialer
}

//TODO: remove unsafe
unsafe impl Send for AgentInternal {}
unsafe impl Sync for AgentInternal {}

impl AgentInternal {
    pub(crate) fn close(&mut self) -> Result<(), Error> {
        if self.done.is_none() {
            return Err(ERR_CLOSED.to_owned());
        }

        if let Some(gather_candidate_cancel) = &self.gather_candidate_cancel {
            gather_candidate_cancel();
        }

        //TODO: ? a.tcpMux.RemoveConnByUfrag(a.localUfrag)

        self.done.take();

        Ok(())
    }

    pub(crate) fn get_selected_pair(&self) -> Option<&CandidatePair> {
        self.selected_pair.as_ref()
    }

    pub(crate) async fn set_selected_pair(&mut self, p: Option<CandidatePair>) {
        log::trace!("Set selected candidate pair: {:?}", p);

        if let Some(mut p) = p {
            p.nominated = true;
            self.selected_pair = Some(p);

            self.update_connection_state(ConnectionState::Connected)
                .await;

            // Notify when the selected pair changes
            if let Some(chan_candidate_pair) = &self.chan_candidate_pair {
                let _ = chan_candidate_pair.send(()).await;
            }

            // Signal connected
            self.on_connected_tx.take();
        } else {
            self.selected_pair = None;
        }
    }

    pub(crate) async fn update_connection_state(&mut self, new_state: ConnectionState) {
        if self.connection_state != new_state {
            // Connection has gone to failed, release all gathered candidates
            if new_state == ConnectionState::Failed {
                self.delete_all_candidates().await;
            }

            log::info!("Setting new connection state: {}", new_state);
            self.connection_state = new_state;

            // Call handler after finishing current task since we may be holding the agent lock
            // and the handler may also require it
            if let Some(chan_state) = &self.chan_state {
                let _ = chan_state.send(new_state).await;
            }
        }
    }

    // Remove all candidates. This closes any listening sockets
    // and removes both the local and remote candidate lists.
    //
    // This is used for restarts, failures and on close
    pub(crate) async fn delete_all_candidates(&mut self) {
        for cs in &mut self.local_candidates.values_mut() {
            for c in cs {
                if let Err(err) = c.close().await {
                    log::warn!("Failed to close candidate {}: {}", c, err);
                }
            }
        }
        self.local_candidates.clear();

        for cs in self.remote_candidates.values_mut() {
            for c in cs {
                if let Err(err) = c.close().await {
                    log::warn!("Failed to close candidate {}: {}", c, err);
                }
            }
        }
        self.remote_candidates.clear();
    }

    pub(crate) fn get_best_available_candidate_pair(&self) -> Option<&CandidatePair> {
        let mut best: Option<&CandidatePair> = None;

        for p in &self.checklist {
            if p.state == CandidatePairState::Failed {
                continue;
            }

            if let Some(b) = &mut best {
                if b.priority() < p.priority() {
                    *b = p;
                }
            } else {
                best = Some(p);
            }
        }

        best
    }

    pub(crate) fn get_best_available_candidate_pair_mut(&mut self) -> Option<&mut CandidatePair> {
        let mut best: Option<&mut CandidatePair> = None;

        for p in &mut self.checklist {
            if p.state == CandidatePairState::Failed {
                continue;
            }

            if let Some(b) = &mut best {
                if b.priority() < p.priority() {
                    *b = p;
                }
            } else {
                best = Some(p);
            }
        }

        best
    }

    pub(crate) fn add_pair(
        &mut self,
        local: Arc<dyn Candidate + Send + Sync>,
        remote: Arc<dyn Candidate + Send + Sync>,
    ) /*-> Option<&CandidatePair>*/
    {
        let p = CandidatePair::new(local, remote, self.is_controlling);
        self.checklist.push(p);
        //return p;
    }

    pub(crate) fn find_pair(
        &self,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    ) -> Option<&CandidatePair> {
        for p in &self.checklist {
            if p.local.equal(&**local) && p.remote.equal(&**remote) {
                return Some(p);
            }
        }
        None
    }

    pub(crate) fn get_pair_mut(
        &mut self,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    ) -> Option<&mut CandidatePair> {
        for p in &mut self.checklist {
            if p.local.equal(&**local) && p.remote.equal(&**remote) {
                return Some(p);
            }
        }
        None
    }

    // validate_selected_pair checks if the selected pair is (still) valid
    // Note: the caller should hold the agent lock.
    pub(crate) async fn validate_selected_pair(&mut self) -> bool {
        if let Some(selected_pair) = &self.selected_pair {
            let disconnected_time =
                match SystemTime::now().duration_since(selected_pair.remote.last_received()) {
                    Ok(d) => d,
                    Err(_) => Duration::from_secs(0),
                };

            // Only allow transitions to failed if a.failedTimeout is non-zero
            let mut total_time_to_failure = self.failed_timeout;
            if total_time_to_failure != Duration::from_secs(0) {
                total_time_to_failure += self.disconnected_timeout;
            }

            if total_time_to_failure != Duration::from_secs(0)
                && disconnected_time > total_time_to_failure
            {
                self.update_connection_state(ConnectionState::Failed).await;
            } else if self.disconnected_timeout != Duration::from_secs(0)
                && disconnected_time > self.disconnected_timeout
            {
                self.update_connection_state(ConnectionState::Disconnected)
                    .await;
            } else {
                self.update_connection_state(ConnectionState::Connected)
                    .await;
            }

            true
        } else {
            false
        }
    }

    // checkKeepalive sends STUN Binding Indications to the selected pair
    // if no packet has been sent on that pair in the last keepaliveInterval
    // Note: the caller should hold the agent lock.
    pub(crate) async fn check_keepalive(&mut self) {
        if let Some(selected_pair) = &self.selected_pair {
            let last_sent = match SystemTime::now().duration_since(selected_pair.local.last_sent())
            {
                Ok(d) => d,
                Err(_) => Duration::from_secs(0),
            };

            let last_received =
                match SystemTime::now().duration_since(selected_pair.remote.last_received()) {
                    Ok(d) => d,
                    Err(_) => Duration::from_secs(0),
                };

            if (self.keepalive_interval != Duration::from_secs(0))
                && ((last_sent > self.keepalive_interval)
                    || (last_received > self.keepalive_interval))
            {
                // we use binding request instead of indication to support refresh consent schemas
                // see https://tools.ietf.org/html/rfc7675
                let local = selected_pair.local.clone();
                let remote = selected_pair.remote.clone();
                self.ping_candidate(&local, &remote).await;
            }
        }
    }

    pub(crate) async fn ping_all_candidates(&mut self) {
        log::trace!("pinging all candidates");

        if self.checklist.is_empty() {
            log::warn!(
                "pingAllCandidates called with no candidate pairs. Connection is not possible yet."
            );
        }

        let mut pairs: Vec<(
            Arc<dyn Candidate + Send + Sync>,
            Arc<dyn Candidate + Send + Sync>,
        )> = vec![];

        for p in &mut self.checklist {
            if p.state == CandidatePairState::Waiting {
                p.state = CandidatePairState::InProgress;
            } else if p.state != CandidatePairState::InProgress {
                continue;
            }

            if p.binding_request_count > self.max_binding_requests {
                log::trace!("max requests reached for pair {}, marking it as failed", p);
                p.state = CandidatePairState::Failed;
            } else {
                p.binding_request_count += 1;
                let local = p.local.clone();
                let remote = p.remote.clone();
                pairs.push((local, remote));
            }
        }

        for (local, remote) in pairs {
            self.ping_candidate(&local, &remote).await;
        }
    }

    pub(crate) async fn send_binding_request(
        &mut self,
        m: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    ) {
        log::trace!("ping STUN from {} to {}", local, remote);

        self.invalidate_pending_binding_requests(Instant::now());
        self.pending_binding_requests.push(BindingRequest {
            timestamp: Instant::now(),
            transaction_id: m.transaction_id,
            destination: remote.addr(),
            is_use_candidate: m.contains(ATTR_USE_CANDIDATE),
        });

        self.send_stun(m, local, remote).await;
    }

    pub(crate) async fn send_binding_success(
        &mut self,
        m: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    ) {
        let addr = remote.addr();
        let (ip, port) = (addr.ip(), addr.port());

        let (out, result) = {
            let mut out = Message::new();
            let result = out.build(&[
                Box::new(m.clone()),
                Box::new(BINDING_SUCCESS),
                Box::new(XORMappedAddress { ip, port }),
                Box::new(MessageIntegrity::new_short_term_integrity(
                    self.local_pwd.clone(),
                )),
                Box::new(FINGERPRINT),
            ]);
            (out, result)
        };

        if let Err(err) = result {
            log::warn!(
                "Failed to handle inbound ICE from: {} to: {} error: {}",
                local,
                remote,
                err
            );
        } else {
            self.send_stun(&out, local, remote).await;
        }
    }

    /* Removes pending binding requests that are over maxBindingRequestTimeout old
       Let HTO be the transaction timeout, which SHOULD be 2*RTT if
       RTT is known or 500 ms otherwise.
       https://tools.ietf.org/html/rfc8445#appendix-B.1
    */
    fn invalidate_pending_binding_requests(&mut self, filter_time: Instant) {
        let initial_size = self.pending_binding_requests.len();

        let mut temp = vec![];
        for binding_request in self.pending_binding_requests.drain(..) {
            if filter_time.duration_since(binding_request.timestamp) < MAX_BINDING_REQUEST_TIMEOUT {
                temp.push(binding_request);
            }
        }

        self.pending_binding_requests = temp;
        let bind_requests_removed = initial_size - self.pending_binding_requests.len();
        if bind_requests_removed > 0 {
            log::trace!(
                "Discarded {} binding requests because they expired",
                bind_requests_removed
            );
        }
    }

    async fn send_stun(
        &self,
        msg: &Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: &Arc<dyn Candidate + Send + Sync>,
    ) {
        if let Err(err) = local.write_to(&msg.raw, &**remote).await {
            log::trace!("failed to send STUN message: {}", err);
        }
    }

    // Assert that the passed TransactionID is in our pendingBindingRequests and returns the destination
    // If the bindingRequest was valid remove it from our pending cache
    pub(crate) fn handle_inbound_binding_success(
        &mut self,
        id: TransactionId,
    ) -> Option<BindingRequest> {
        self.invalidate_pending_binding_requests(Instant::now());
        for i in 0..self.pending_binding_requests.len() {
            if self.pending_binding_requests[i].transaction_id == id {
                let valid_binding_request = self.pending_binding_requests.remove(i);
                return Some(valid_binding_request);
            }
        }
        None
    }

    // handleInbound processes STUN traffic from a remote candidate
    pub(crate) async fn handle_inbound(
        &mut self,
        m: &mut Message,
        local: &Arc<dyn Candidate + Send + Sync>,
        remote: SocketAddr,
    ) {
        if m.typ.method != METHOD_BINDING
            || !(m.typ.class == CLASS_SUCCESS_RESPONSE
                || m.typ.class == CLASS_REQUEST
                || m.typ.class == CLASS_INDICATION)
        {
            log::trace!(
                "unhandled STUN from {} to {} class({}) method({})",
                remote,
                local,
                m.typ.class,
                m.typ.method
            );
            return;
        }

        if self.is_controlling {
            if m.contains(ATTR_ICE_CONTROLLING) {
                log::debug!("inbound isControlling && a.isControlling == true");
                return;
            } else if m.contains(ATTR_USE_CANDIDATE) {
                log::debug!("useCandidate && a.isControlling == true");
                return;
            }
        } else if m.contains(ATTR_ICE_CONTROLLED) {
            log::debug!("inbound isControlled && a.isControlling == false");
            return;
        }

        let mut remote_candidate = self.find_remote_candidate(local.network_type(), remote);
        if m.typ.class == CLASS_SUCCESS_RESPONSE {
            if let Err(err) = assert_inbound_message_integrity(m, self.remote_pwd.as_bytes()) {
                log::warn!("discard message from ({}), {}", remote, err);
                return;
            }

            if let Some(rc) = &remote_candidate {
                self.handle_success_response(m, local, rc, remote).await;
            } else {
                log::warn!("discard success message from ({}), no such remote", remote);
                return;
            }
        } else if m.typ.class == CLASS_REQUEST {
            let username = self.local_ufrag.clone() + ":" + self.remote_ufrag.as_str();
            if let Err(err) = assert_inbound_username(m, username) {
                log::warn!("discard message from ({}), {}", remote, err);
                return;
            } else if let Err(err) = assert_inbound_message_integrity(m, self.local_pwd.as_bytes())
            {
                log::warn!("discard message from ({}), {}", remote, err);
                return;
            }

            if remote_candidate.is_none() {
                let (ip, port, network_type) = (remote.ip(), remote.port(), NetworkType::UDP4);

                let prflx_candidate_config = CandidatePeerReflexiveConfig {
                    base_config: CandidateBaseConfig {
                        network: network_type.to_string(),
                        address: ip.to_string(),
                        port,
                        component: local.component(),
                        ..Default::default()
                    },
                    rel_addr: "".to_owned(),
                    rel_port: 0,
                };

                match prflx_candidate_config.new_candidate_peer_reflexive().await {
                    Ok(prflx_candidate) => remote_candidate = Some(Arc::new(prflx_candidate)),
                    Err(err) => {
                        log::error!("Failed to create new remote prflx candidate ({})", err);
                        return;
                    }
                };

                log::debug!("adding a new peer-reflexive candidate: {} ", remote);
                if let Some(rc) = &remote_candidate {
                    self.add_remote_candidate(rc);
                }
            }

            log::trace!("inbound STUN (Request) from {} to {}", remote, local);

            if let Some(rc) = &remote_candidate {
                self.handle_binding_request(m, local, rc).await;
            }
        }

        if let Some(rc) = remote_candidate {
            rc.seen(false);
        }
    }

    fn find_remote_candidate(
        &self,
        network_type: NetworkType,
        addr: SocketAddr,
    ) -> Option<Arc<dyn Candidate + Send + Sync>> {
        let (ip, port) = (addr.ip(), addr.port());

        if let Some(cands) = self.remote_candidates.get(&network_type) {
            for c in cands {
                if c.address() == ip.to_string() && c.port() == port {
                    return Some(c.clone());
                }
            }
        }
        None
    }

    // add_remote_candidate assumes you are holding the lock (must be execute using a.run)
    fn add_remote_candidate(&mut self, c: &Arc<dyn Candidate + Send + Sync>) {
        let network_type = c.network_type();

        if let Some(cands) = self.remote_candidates.get(&network_type) {
            for cand in cands {
                if cand.equal(&**c) {
                    return;
                }
            }
        }

        if let Some(cands) = self.remote_candidates.get_mut(&network_type) {
            cands.push(c.clone());
        } else {
            self.remote_candidates.insert(network_type, vec![c.clone()]);
        }

        let mut local_cands = vec![];
        if let Some(cands) = self.local_candidates.get(&network_type) {
            local_cands = cands.clone();
        }

        for cand in local_cands {
            self.add_pair(cand, c.clone());
        }

        //TODO: self.requestConnectivityCheck();
    }
}